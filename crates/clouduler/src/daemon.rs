//! Daemon bootstrap. Subsystems get wired in here as later phases land.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cl_config::{ClusterConfig, FailoverConfig, Node, NodeRole};
use cl_raft::ids::node_id;
use cl_raft::liveness::{Fence, FenceAction};
use cl_raft::node::{ClusterNode, Options};
use cl_secret::ClusterKey;
use cl_sched::materialize::Materializer;
use cl_sched::Scheduler;
use std::sync::atomic::{AtomicBool, Ordering};
use cl_raft::state::{Availability, NodeState, Request, Role};
use openraft::BasicNode;
use tracing::{info, warn};

/// How long to wait for dockerd at startup before carrying on without it.
/// Generous because this is a boot race: on a host bringing several machines
/// up at once, dockerd can take a minute to answer its own socket.
const DOCKER_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

pub async fn run(
    cfg: ClusterConfig,
    node_name: String,
    data_dir_override: Option<PathBuf>,
    config_path: PathBuf,
) -> Result<()> {
    let Some(node) = cfg.node(&node_name) else {
        bail!(
            "node {node_name} is not declared in the config (known: {})",
            cfg.nodes.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    };

    // The config is where the data dir belongs, so the web ui can move it. The
    // flag stays as an override for running a second daemon out of the way.
    let data_dir = data_dir_override.unwrap_or_else(|| node.data_dir(&cfg).to_path_buf());

    let rpc_addr = cfg
        .rpc_addr(node)
        .context("node has no address on the internal network")?;

    info!(
        cluster = %cfg.cluster.name,
        node = %node_name,
        role = %node.role,
        rpc = %rpc_addr,
        drives = node.drives.len(),
        "starting"
    );

    bail_if_removed(&cfg, node, &node_name, &data_dir).await?;

    // `ClusterKey` is deliberately not `Clone` (see its doc comment), so it is
    // wrapped in an `Arc` the moment it is loaded: every subsystem that needs
    // it shares the one allocation instead of each holding its own copy.
    let key = load_cluster_key(&cfg, node).map(Arc::new);
    let node_keypair = load_node_keypair(&cfg);
    let node_pubkey = node_keypair.as_ref().map(cl_secret::NodeKeypair::public_key_encoded);

    // Broadcasts the cluster config and cluster key to every subsystem that
    // needs a cheap, always-current read rather than a snapshot frozen at
    // boot. `config_tx` is fed by `watch_cluster_config` on every generation
    // bump it already polls for; `key_tx` is fed by `cl-api`'s existing
    // in-process key delivery (`spawn_unwrap_own_key`, the manual upload
    // route), so a node that starts with no key still picks one up live once
    // another node delivers it.
    let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::new(cfg.clone()));
    let (key_tx, key_rx) = tokio::sync::watch::channel(key.clone());

    let cluster = ClusterNode::start(Options {
        name: node_name.clone(),
        id: node.id,
        rpc_addr,
        data_dir: data_dir.join("raft"),
        cluster_name: cfg.cluster.name.clone(),
    })
    .await?;

    let cluster = Arc::new(cluster);
    form_cluster(&cfg, &node_name, &cluster).await?;

    let failover = cfg.failover.clone();
    let detector = tokio::spawn(cluster.clone().run_failure_detector(
        failover.probe_interval,
        failover.node_timeout,
    ));

    // Shared between the fence watcher and the scheduler, which consults it
    // before starting anything. Closed to begin with, matching `Fence::new`:
    // the scheduler's first sweep lands about a second into the process, long
    // before raft has caught up, and until then this node's idea of what runs
    // here is whatever was true when it died.
    let fenced = Arc::new(AtomicBool::new(true));
    let fencing = tokio::spawn(run_fence(
        cluster.clone(),
        failover.clone(),
        fenced.clone(),
    ));

    // The api comes up before docker does, and picks the client up whenever it
    // lands. A node whose dockerd is slow or absent still has to be reachable,
    // since the web ui is where someone would go to find out why.
    let docker: Arc<tokio::sync::RwLock<Option<cl_docker::Docker>>> = Arc::default();
    // Shared between the scheduler (which marks/unmarks it around an image
    // pull) and the api (which reads it to report a "pulling" instance
    // state) — see `cl_sched::PullTracker`.
    let pulling: cl_sched::PullTracker = Arc::default();

    // Fires the same clean shutdown path `ctrl_c()` drives, so the internal
    // self-update restart route can trigger it programmatically after
    // swapping in a new binary.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());

    // Shared by the api's cross-node forwarding and by self-update's peer
    // calls. Accepts invalid certs for both: peers are authenticated by an
    // HMAC header, not TLS trust, since every node's certificate is self
    // signed until ACME issues a real one, and there is no shared CA to check
    // it against.
    let internal_http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the internal node-to-node http client")?;

    let api = start_api(StartApiArgs {
        cfg: &cfg,
        node,
        node_name: &node_name,
        config_path: &config_path,
        cluster: &cluster,
        data_dir: &data_dir,
        docker: docker.clone(),
        pulling: pulling.clone(),
        key: key.clone(),
        key_tx,
        shutdown: shutdown_tx,
        http: internal_http.clone(),
        node_keypair,
    })
    .await;

    let drives = crate::drives::start(&cfg, &node_name, &cluster, config_rx.clone());

    // Containers resolve each other through this, so it comes up alongside the
    // overlay rather than waiting for anything to be scheduled.
    let internal_dns = crate::internal_dns::start(&cfg, &cluster);
    let exposed = crate::expose::start(&cluster);
    let proxy = crate::proxy::start(&cfg, &cluster, &data_dir, config_rx.clone(), key_rx.clone());

    // Worth waiting for: on a node that has just booted, dockerd is often a
    // second or two behind us, and a node that gave up on docker for the life
    // of the process would vote but run nothing until someone restarted it.
    *docker.write().await = connect_docker().await;
    // Keeps retrying afterwards: a node whose dockerd was not up yet during
    // the wait above, or whose dockerd is later bounced independently of this
    // process, gets a working `Docker` handle again without a restart. Every
    // subsystem below holds the same `Arc`, so this is the one place that
    // needs to notice.
    let docker_reconnect = tokio::spawn(run_docker_reconnect(docker.clone()));

    let materializer = Materializer::new(&data_dir, key_rx.clone());
    let scheduler = Scheduler::new(
        node_name.clone(),
        cluster.clone(),
        docker.clone(),
        materializer,
        cfg.clone(),
        pulling.clone(),
    )
    .with_zone(cl_net::dns::zone_origin(&cfg));
    let gate = fenced.clone();
    let scheduler = tokio::spawn(scheduler.run(move || gate.load(Ordering::Relaxed)));

    // Built before the scheduler needs it, but idempotent and on a loop, so
    // ordering against a stack deploy does not matter.
    let overlay = crate::overlay::start(&cfg, &node_name, &cluster, docker.clone());
    let egress = crate::egress::start(&cfg, &node_name, &cluster, docker.clone(), key_rx.clone());

    let self_update = start_self_update(&cfg, &node_name, &cluster, &data_dir, internal_http.clone(), key_rx.clone());

    // The api records a move and returns; this is what carries it out. It
    // needs both halves of the job, a docker to stop the container with and a
    // registry to find the volume on disk, so a node missing either runs no
    // migrations, which is correct: it can hold no volume to move.
    let migrations = crate::migration::start(
        config_rx.clone(),
        &node_name,
        &cluster,
        docker.clone(),
        drives.registry(),
    );

    let config_writer = tokio::spawn(watch_cluster_config(
        cluster.clone(),
        config_path.clone(),
        failover.probe_interval,
        config_tx,
    ));

    let watcher = tokio::spawn(watch_leadership(cfg, config_path, cluster.clone(), failover.probe_interval));
    let self_report = tokio::spawn(report_self(node_name.clone(), cluster.clone(), node_pubkey));
    let reaper = tokio::spawn(reap_removed_nodes(cluster.clone(), failover.probe_interval));

    tokio::select! {
        result = tokio::signal::ctrl_c() => { result?; }
        _ = shutdown_rx.changed() => {}
    }
    info!("shutting down");
    watcher.abort();
    self_report.abort();
    reaper.abort();
    config_writer.abort();
    detector.abort();
    fencing.abort();
    docker_reconnect.abort();
    scheduler.abort();
    if let Some((orchestrator, worker)) = self_update {
        orchestrator.abort();
        worker.abort();
    }
    if let Some(api) = api {
        api.abort();
    }
    migrations.abort();
    overlay.abort();
    egress.abort();
    if let Some(internal_dns) = internal_dns {
        internal_dns.abort();
    }
    exposed.abort();
    proxy.shutdown().await;
    // Before the raft shutdown, so the mounts are gone while the runtime that
    // serves their requests is still alive.
    drives.shutdown().await;
    cluster.shutdown().await?;
    Ok(())
}

/// Stand up the http api and the web ui.
///
/// Every node listens; a node that is not the leader answers with a redirect to
/// the one that is. That is simpler than starting and stopping a listener on
/// every election, and it means someone who bookmarked a node still gets in.
struct StartApiArgs<'a> {
    cfg: &'a ClusterConfig,
    node: &'a Node,
    node_name: &'a str,
    config_path: &'a Path,
    cluster: &'a Arc<ClusterNode>,
    data_dir: &'a Path,
    docker: Arc<tokio::sync::RwLock<Option<cl_docker::Docker>>>,
    pulling: cl_sched::PullTracker,
    key: Option<Arc<ClusterKey>>,
    key_tx: tokio::sync::watch::Sender<Option<Arc<ClusterKey>>>,
    shutdown: tokio::sync::watch::Sender<()>,
    http: reqwest::Client,
    node_keypair: Option<cl_secret::NodeKeypair>,
}

async fn start_api(args: StartApiArgs<'_>) -> Option<tokio::task::JoinHandle<()>> {
    let StartApiArgs {
        cfg,
        node: _node,
        node_name,
        config_path,
        cluster,
        data_dir,
        docker,
        pulling,
        key,
        key_tx,
        shutdown,
        http,
        node_keypair,
    } = args;
    // The issued certificate when there is one, and a self signed one until
    // then, so the ui is reachable on a brand new cluster. Browsers complain
    // about the fallback, which is correct: it is not trusted.
    let fqdn = cfg.web_fqdn();
    let (cert_pem, key_pem) = match cluster.state().await.certificates.get(&fqdn) {
        Some(stored) => (stored.chain_pem.clone(), stored.key_pem.clone()),
        None => match rcgen::generate_simple_self_signed(vec![fqdn.clone()]) {
            Ok(cert) => {
                warn!(%fqdn, "serving the ui with a self signed certificate");
                (cert.cert.pem(), cert.signing_key.serialize_pem())
            }
            Err(e) => {
                warn!(error = %e, "cannot generate a certificate, the api will not start");
                return None;
            }
        },
    };

    let listen_addr = SocketAddr::from(([0, 0, 0, 0], cfg.web.port));
    // Loopback only: this listener has no TLS, and the proxy that talks to it
    // runs on this same machine.
    let plain_listen_addr = cfg
        .web
        .through_proxy
        .then(|| SocketAddr::from(([127, 0, 0, 1], cfg.web.backend_port)));
    let options = cl_api::ApiServerOptions {
        cluster: cluster.clone(),
        docker,
        pulling,
        node_name: node_name.to_string(),
        config_path: config_path.to_path_buf(),
        cluster_config: cfg.clone(),
        key_path: cfg.secrets.key_file.clone(),
        key,
        key_tx,
        tls_cert_pem: cert_pem.into_bytes(),
        tls_key_pem: key_pem.into_bytes(),
        listen_addr,
        plain_listen_addr,
        self_update_artifact_path: self_update_artifact_path(data_dir),
        shutdown,
        http,
        node_keypair,
    };

    let server = match cl_api::ApiServer::new(options).await {
        Ok(server) => server,
        Err(e) => {
            warn!(error = %e, "cannot build the api");
            return None;
        }
    };

    info!(%listen_addr, url = %format!("https://{fqdn}:{}", cfg.web.port), "web ui listening");
    Some(tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            // Port 443 needs root, and that is the usual reason this fails.
            warn!(error = %e, "api server stopped");
        }
    }))
}

/// Where a self-update artifact this node built or fetched is staged.
fn self_update_artifact_path(data_dir: &Path) -> PathBuf {
    data_dir.join("self-update").join("artifact")
}

/// Wire up the leader-gated orchestrator and the per-node worker for
/// GitHub-release-based self-update, alongside the scheduler. Both hold a
/// live `key_rx` rather than a key loaded once: a node started with no
/// cluster key still gets self-update working once one is delivered to it,
/// without a restart. Unlike the old build pipeline this needs no docker
/// connection: downloading a prebuilt binary and swapping it in are both
/// plain filesystem operations.
fn start_self_update(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    data_dir: &Path,
    http: reqwest::Client,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
) -> Option<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)> {
    cfg.node(node_name)?;

    let artifact_path = self_update_artifact_path(data_dir);
    if let Some(parent) = artifact_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let orchestrator = cl_update::orchestrator::Orchestrator {
        cluster: cluster.clone(),
        http: http.clone(),
        cluster_config: cfg.clone(),
        cluster_key: key_rx.clone(),
    };
    let worker = cl_update::worker::Worker {
        node_name: node_name.to_string(),
        cluster: cluster.clone(),
        cluster_key: key_rx,
        http,
        cluster_config: cfg.clone(),
        artifact_path,
    };

    Some((
        tokio::spawn(orchestrator.run()),
        tokio::spawn(worker.run()),
    ))
}

/// Keep `docker` pointed at a live client for as long as the process runs.
///
/// The initial connect in `run` waits `DOCKER_WAIT` and gives up; this picks
/// up from there and keeps trying, so a node whose dockerd was not up yet at
/// boot (or whose dockerd is later restarted on its own) gets a working
/// client again without restarting the daemon. bollard's client normally
/// reconnects its own socket transparently per call once it has one, so the
/// steady-state cost here is just a cheap ping.
async fn run_docker_reconnect(docker: Arc<tokio::sync::RwLock<Option<cl_docker::Docker>>>) {
    const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
    let mut ticker = tokio::time::interval(RETRY_INTERVAL);
    loop {
        ticker.tick().await;
        let alive = match docker.read().await.as_ref() {
            Some(client) => client.ping().await.is_ok(),
            None => false,
        };
        if alive {
            continue;
        }
        match cl_docker::Docker::connect() {
            Ok(client) => match client.ping().await {
                Ok(()) => {
                    let had_one = docker.read().await.is_some();
                    *docker.write().await = Some(client);
                    if !had_one {
                        info!("docker connected");
                    }
                }
                Err(_) => *docker.write().await = None,
            },
            Err(_) => *docker.write().await = None,
        }
    }
}

/// Connect to docker, waiting for it to come up.
///
/// The wait is what makes a node that reboots with the cluster come back with
/// its containers: the daemon and dockerd start at the same time, and the
/// first connect often loses that race by a second or two. Giving up for the
/// life of the process over that would leave the node voting but idle.
async fn connect_docker() -> Option<cl_docker::Docker> {
    let deadline = Instant::now() + DOCKER_WAIT;
    let mut last = None;
    while Instant::now() < deadline {
        match cl_docker::Docker::connect() {
            Ok(docker) => match docker.ping().await {
                Ok(()) => return Some(docker),
                Err(e) => last = Some(e.to_string()),
            },
            Err(e) => last = Some(e.to_string()),
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    warn!(waited = ?DOCKER_WAIT, error = ?last, "docker never answered");
    None
}

/// Load the key that opens stored secrets, creating it on first launch.
///
/// A node without a key is not an error: it replicates sealed secrets like any
/// other state and simply cannot run containers that need them. That is the
/// intended setup for a worker you do not want holding the keys to everything.
pub(crate) fn load_cluster_key(cfg: &ClusterConfig, node: &Node) -> Option<ClusterKey> {
    let path = &cfg.secrets.key_file;

    match ClusterKey::load_or_create(path, node.generates_secret_key(cfg)) {
        Ok((key, created)) => {
            if created {
                // The one moment to tell them, since it is never printed again.
                warn!(
                    path = %path.display(),
                    fingerprint = %key.fingerprint(),
                    "generated a new cluster key, back this file up. Without it every stored secret is unrecoverable"
                );
            } else {
                info!(fingerprint = %key.fingerprint(), "cluster key loaded");
            }
            Some(key)
        }
        Err(cl_secret::SecretError::NoKey) => {
            info!(
                path = %path.display(),
                "no cluster key on this node, secrets cannot be decrypted here"
            );
            None
        }
        Err(e) => {
            warn!(error = %e, "cannot use the cluster key, secrets will not be available");
            None
        }
    }
}

/// Load this node's own asymmetric keypair, creating it on first launch.
///
/// Every node has one, unlike the cluster key: it is what lets another node
/// wrap a copy of the cluster key for this one without ever putting the
/// plaintext on the wire. Not fatal on failure: a node that somehow cannot
/// get one just never receives an automatic key delivery, the same steady
/// state as before this existed.
pub(crate) fn load_node_keypair(cfg: &ClusterConfig) -> Option<cl_secret::NodeKeypair> {
    let path = &cfg.secrets.node_key_file;
    match cl_secret::NodeKeypair::load_or_create(path) {
        Ok((keypair, created)) => {
            if created {
                info!(path = %path.display(), public = %keypair.public_key_encoded(), "generated this node's keypair");
            }
            Some(keypair)
        }
        Err(e) => {
            warn!(error = %e, "cannot use this node's keypair, automatic key delivery will not work");
            None
        }
    }
}

/// Stop this node's containers once it has been without a quorum for longer
/// than `failover.fence_timeout`.
///
/// This is the other half of the failure handling: the leader will hand our
/// containers to another node after `node_timeout`, and we must have let go of
/// them before that happens, or the same volume ends up with two writers.
async fn run_fence(cluster: Arc<ClusterNode>, failover: FailoverConfig, fenced: Arc<AtomicBool>) {
    let quorum = cluster.quorum();
    let mut fence = Fence::new();
    let mut ticker = tokio::time::interval(failover.probe_interval);

    loop {
        ticker.tick().await;
        match fence.observe(quorum.has_quorum(), Instant::now(), failover.fence_timeout) {
            FenceAction::Fence => {
                warn!(
                    timeout = ?failover.fence_timeout,
                    "no quorum, stopping local containers"
                );
                fenced.store(true, Ordering::Relaxed);
            }
            FenceAction::Release => {
                // Also the normal path on a healthy start: the fence opens on
                // the first tick that sees a quorum.
                info!("quorum confirmed, containers may start");
                fenced.store(false, Ordering::Relaxed);
            }
            FenceAction::None => {}
        }
    }
}

/// Write the replicated cluster config to this node's config file whenever it
/// changes, and broadcast the parsed config to every live subsystem through
/// `config_tx`.
///
/// This is what makes an edit in the web ui reach every node: the api submits
/// it through raft, and each daemon lands its own copy here. A node that was
/// down during the edit picks it up on its next tick after rejoining, rather
/// than coming back with a stale file.
///
/// Most of what a config edit can change now applies live, through
/// `config_tx` (see `docs/live-config.md`) — the file write remains so a
/// restarted daemon boots from the config the cluster last agreed on, not a
/// possibly-reverted local copy. What still needs a restart is raft's own
/// listen address, data dir and cluster identity: rebuilding raft membership
/// or transport under a running cluster is not something to do unattended.
async fn watch_cluster_config(
    cluster: Arc<ClusterNode>,
    config_path: PathBuf,
    interval: std::time::Duration,
    config_tx: tokio::sync::watch::Sender<Arc<ClusterConfig>>,
) {
    // Whatever is on disk right now is taken as already applied, so a restart
    // does not rewrite an identical file and log a change that did not happen.
    let mut written = 0;
    let mut ticker = tokio::time::interval(interval);

    loop {
        ticker.tick().await;
        let Some(entry) = cluster.state().await.cluster_config else {
            continue;
        };
        if entry.generation == written {
            continue;
        }

        match ClusterConfig::parse(&entry.yaml) {
            Ok(parsed) => {
                let _ = config_tx.send(Arc::new(parsed));
            }
            Err(e) => warn!(error = %e, "replicated cluster config failed to parse, not broadcasting it"),
        }

        let current = std::fs::read_to_string(&config_path).unwrap_or_default();
        if current == entry.yaml {
            written = entry.generation;
            continue;
        }

        match write_atomically(&config_path, entry.yaml.as_bytes()) {
            Ok(()) => {
                written = entry.generation;
                info!(
                    generation = entry.generation,
                    path = %config_path.display(),
                    "cluster config updated"
                );
            }
            Err(e) => warn!(error = %e, path = %config_path.display(), "cannot write the config file"),
        }
    }
}

/// Write through a temp file in the same directory, so a reader never sees a
/// half written config.
fn write_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, contents)?;
    std::fs::rename(&temp, path)
}

/// The cluster config as replicated state currently has it, falling back to
/// `boot` only for the brief window before any leader has published one, or
/// if the replicated copy fails to parse.
pub(crate) fn live_config(state: &cl_raft::state::ClusterState, boot: &ClusterConfig) -> ClusterConfig {
    match &state.cluster_config {
        Some(entry) => ClusterConfig::parse(&entry.yaml).unwrap_or_else(|e| {
            warn!(error = %e, "replicated cluster config failed to parse, using the config this node booted with");
            boot.clone()
        }),
        None => boot.clone(),
    }
}

/// Keep the replicated roster in step with the config for as long as this node
/// leads. Seeding is done on every leadership change and, in between, on a
/// timer.
///
/// The timer is not just a safety net: a node newly added to the config is
/// never itself a raft voter/learner yet, so it can never appear in
/// `quorum`'s `reachable` set, so adding it can never itself flip
/// `quorum.changed()`. Without the timer, `seed_roster` - the only thing that
/// calls `add_manager`/`add_worker` - would simply never rerun for it. The
/// timer breaks that chicken-and-egg deadlock; `probe_interval` is reused
/// from `failover` for the same reason `reap_removed_nodes` does, a plain
/// state write doesn't move the raft-metrics needle `quorum.changed()` gates
/// on.
///
/// `cfg` is only the config this node booted with; a node joining after that
/// changes the replicated `ClusterConfig`, not this snapshot. So the roster is
/// seeded from the replicated config (already fresh, since a join pushes it
/// straight into raft), falling back to `cfg` only for the brief window before
/// `seed_config` has published anything.
async fn watch_leadership(
    cfg: ClusterConfig,
    config_path: PathBuf,
    cluster: Arc<ClusterNode>,
    probe_interval: Duration,
) {
    let mut quorum = cluster.quorum();
    let mut ticker = tokio::time::interval(probe_interval);
    loop {
        let status = quorum.status();
        info!(
            quorum = status.has_quorum,
            leader = ?status.leader,
            reachable = status.reachable.len(),
            voters = status.voters,
            "cluster status"
        );

        if status.is_leader(cluster.id) {
            if let Err(e) = seed_config(&config_path, &cluster).await {
                warn!(error = %e, "could not publish the cluster config");
            }

            let current = live_config(&cluster.state().await, &cfg);
            if let Err(e) = seed_roster(&current, &cluster).await {
                warn!(error = %e, "could not publish the node roster");
            }
        }

        tokio::select! {
            _ = ticker.tick() => {}
            changed = quorum.changed() => {
                if !changed {
                    return;
                }
            }
        }
    }
}

/// Ask any other configured peer whether this node was deleted through the
/// API while it was down, and if so, wipe local secrets rather than let it
/// rejoin (or keep holding cluster key material for a machine an operator
/// has already decommissioned).
///
/// A restarting node has no way to tell this from its own state: raft simply
/// stops delivering it new log entries the moment it drops out of
/// membership, so its own copy of `removed` is frozen at whatever it last
/// saw before going down. A live peer's copy is authoritative.
///
/// An unreachable peer is not evidence of removal - this only acts once a
/// peer actually confirms it, and gives up quietly if none answer (e.g. this
/// is the very first node, still forming the cluster, or the network is
/// briefly partitioned).
async fn bail_if_removed(cfg: &ClusterConfig, this_node: &Node, node_name: &str, data_dir: &Path) -> Result<()> {
    let own_id = this_node.id;
    for (name, node) in &cfg.nodes {
        if name == node_name {
            continue;
        }
        let Some(addr) = cfg.rpc_addr(node) else { continue };

        let client = cl_raft::Client::connect(addr.to_string());
        let state = match tokio::time::timeout(Duration::from_secs(5), client.state()).await {
            Ok(Ok(state)) => state,
            _ => continue,
        };

        if state.removed.contains(&own_id) {
            warn!(
                node = %node_name,
                confirmed_by = %name,
                "this node was removed from the cluster while it was down; wiping local secrets"
            );
            wipe_local_secrets(cfg, data_dir);
            bail!("removed from the cluster while down; local secrets wiped, re-provision to rejoin");
        }
        return Ok(());
    }
    Ok(())
}

/// Best effort: a file already gone is not a failure, and a removed node
/// should not fail to start over one leftover file it could not delete.
fn wipe_local_secrets(cfg: &ClusterConfig, data_dir: &Path) {
    for path in [&cfg.secrets.key_file, &cfg.secrets.node_key_file] {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %path.display(), error = %e, "could not remove secret file");
            }
        }
    }
    if let Err(e) = std::fs::remove_dir_all(data_dir.join("raft")) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %data_dir.join("raft").display(), error = %e, "could not remove local raft state");
        }
    }
}

/// Form the cluster on first boot.
///
/// The first manager by name is the one that forms it. That rule is
/// deterministic, so several managers starting at once cannot each form their
/// own cluster.
async fn form_cluster(cfg: &ClusterConfig, node_name: &str, cluster: &ClusterNode) -> Result<()> {
    let founder = cfg
        .managers()
        .next()
        .map(|(name, _)| name.clone())
        .context("config has no manager")?;

    if node_name != founder || cluster.is_initialized().await? {
        return Ok(());
    }

    let members: BTreeMap<_, _> = cfg
        .managers()
        .filter_map(|(_name, node)| {
            Some((node_id(node.id), BasicNode::new(cfg.rpc_addr(node)?.to_string())))
        })
        .collect();
    info!(members = members.len(), "forming cluster");
    cluster.initialize(members).await?;
    Ok(())
}

/// Seed the replicated cluster config from the leader's file, once, on a
/// cluster that has never had one.
///
/// Only the very first leader does anything here. After that the stored config
/// is the authority and is only replaced through an explicit edit, so a node
/// starting with an out of date file cannot push it back over everyone else's.
async fn seed_config(config_path: &Path, cluster: &ClusterNode) -> Result<()> {
    if cluster.state().await.cluster_config.is_some() {
        return Ok(());
    }

    let yaml = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let response = cluster
        .submit(Request::PutClusterConfig {
            yaml,
            expect_generation: 0,
        })
        .await?;
    match response.error {
        // Another manager got there first, which is the expected race and not
        // a problem: both published the same file.
        Some(error) => info!(%error, "cluster config was already published"),
        None => info!("published the cluster config"),
    }
    Ok(())
}

/// Publish every configured node into the replicated roster, and attach the
/// workers as learners so they receive state without voting.
async fn seed_roster(cfg: &ClusterConfig, cluster: &ClusterNode) -> Result<()> {
    let known = cluster.state().await;

    for (name, node) in &cfg.nodes {
        // Deleted through the API: the config file still lists it until an
        // operator edits it out, but the roster must not resurrect it just
        // because this leader's in-memory config is stale. Keyed by id, not
        // name, so a node re-provisioned under this name (fresh id) is never
        // caught by a previous instance's tombstone.
        if known.removed.contains(&node.id) {
            continue;
        }

        let Some(rpc_addr) = cfg.rpc_addr(node) else {
            warn!(node = %name, "skipping node with no internal address");
            continue;
        };

        // Same name, different id: this node was re-provisioned fresh under
        // an identity `RemoveNode` never tombstoned, because the name never
        // left the roster - `clouduler init --force` after local state was
        // lost, or a co-manager `force_quorum` demoted rather than dropped,
        // just came back under a new id via `init --join`. The old id is
        // still sitting in raft membership as a learner nothing will ever
        // reach again, at the same address the new one now owns. Retiring
        // it here, once, is what lets `reap_removed_nodes` clean it out on
        // its own timer instead of it lingering forever.
        if let Some(previous) = known.nodes.get(name) {
            if previous.id != node.id {
                let response = cluster
                    .submit(Request::RetireNodeId { id: previous.id })
                    .await?;
                if let Some(error) = response.error {
                    warn!(node = %name, %error, "could not retire the previous node id");
                }
            }
        }

        // A node the roster has never seen is one that joined after the
        // cluster was formed, so it still has to be added to raft
        // membership. A manager gets a second check even once known: unlike
        // a learner, a voter can be demoted without being dropped from the
        // roster - `force_quorum`'s disaster recovery does exactly that, on
        // purpose, so the demoted node comes back as a learner rather than a
        // stranger. Re-checking here is what turns that back into a voter
        // the moment it is reachable again, with nothing for an operator to
        // run by hand.
        let id = node_id(node.id);
        let needs_membership = match node.role {
            NodeRole::Manager => !cluster.is_voter(id).await,
            NodeRole::Worker => !known.nodes.contains_key(name),
        };
        if needs_membership {
            let result = match node.role {
                NodeRole::Manager => cluster.add_manager(id, rpc_addr).await,
                NodeRole::Worker => cluster.add_worker(id, rpc_addr).await,
            };
            if let Err(e) = result {
                // Usually the node is not up yet. Retried on the next pass.
                warn!(node = %name, error = %e, "could not add the node to raft membership");
                continue;
            }
        }

        let desired = to_state(name, node, rpc_addr, known.nodes.get(name));
        if known.nodes.get(name) != Some(&desired) {
            let response = cluster.submit(Request::UpsertNode(desired)).await?;
            if let Some(error) = response.error {
                warn!(node = %name, %error, "could not publish node");
            }
        }
    }

    Ok(())
}

/// Drop nodes removed through the API from actual raft membership.
///
/// Separate from `watch_leadership`, which only reruns on a quorum status
/// change: a plain `RemoveNode` write does not itself change quorum status
/// (leader, voters, reachability), so a removal could otherwise sit in the
/// `removed` tombstone set indefinitely without ever being applied to raft
/// membership. `remove_node` is a no-op once the node is gone, so retrying on
/// every tick is harmless.
async fn reap_removed_nodes(cluster: Arc<ClusterNode>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        if !cluster.is_leader() {
            continue;
        }
        let known = cluster.state().await;
        for id in &known.removed {
            if let Err(e) = cluster.remove_node(node_id(*id)).await {
                warn!(node = %id, error = %e, "could not drop removed node from raft membership");
            }
        }
    }
}

/// `cl_raft::PortProtocol` mirrors `cl_config::Protocol` structurally so
/// `cl-raft` does not depend on `cl-config`; this is the one place the two
/// meet, converting a node's declared open ports into replicated state.
fn to_port_protocol(protocol: cl_config::Protocol) -> cl_raft::PortProtocol {
    match protocol {
        cl_config::Protocol::Tcp => cl_raft::PortProtocol::Tcp,
        cl_config::Protocol::Udp => cl_raft::PortProtocol::Udp,
    }
}

/// Build the roster entry from config, keeping any availability the cluster has
/// already set. Draining a node from the ui must not be undone by a restart.
fn to_state(
    name: &str,
    node: &Node,
    rpc_addr: std::net::SocketAddr,
    current: Option<&NodeState>,
) -> NodeState {
    NodeState {
        name: name.to_string(),
        id: node.id,
        role: match node.role {
            NodeRole::Manager => Role::Manager,
            NodeRole::Worker => Role::Worker,
        },
        rpc_addr,
        drives: node.drives.clone(),
        // Straight from the config, not merged with what is stored: the config
        // is the source of truth for what a node is, so removing a label there
        // has to actually remove it, or a stack keeps landing on a node that
        // no longer qualifies.
        labels: node.labels.clone(),
        availability: current
            .map(|c| c.availability)
            .unwrap_or(Availability::Active),
        egress_networks: node
            .network
            .iter()
            .filter(|(_, addr)| matches!(addr, cl_config::NodeAddress::Interface(_)))
            .map(|(name, _)| name.clone())
            .collect(),
        networks: node.network.keys().cloned().collect(),
        // Public networks declare open ports on the router address; wireguard
        // ones on the tunnel interface address (a VPN provider's port
        // forwarding, e.g. AirVPN). Either way, this is the set of ports the
        // scheduler may place a raw `x-clouduler.expose` entry on this node for.
        open_ports: node
            .network
            .iter()
            .filter_map(|(name, addr)| {
                let open_ports = match addr {
                    cl_config::NodeAddress::Public(p) => &p.open_ports,
                    cl_config::NodeAddress::Interface(i) => &i.open_ports,
                    cl_config::NodeAddress::Ip(_) => return None,
                };
                if open_ports.is_empty() {
                    return None;
                }
                Some((
                    name.clone(),
                    open_ports.iter().map(|p| (to_port_protocol(p.protocol), p.port)).collect(),
                ))
            })
            .collect(),
        // Not derived from this process's own environment: this runs on
        // whichever node currently leads, so `std::env::consts::ARCH` and the
        // local binary would describe the leader, not `name`. Left as
        // whatever is already stored (empty for a brand new node) and healed
        // by `report_self`, which is the one thing every node — including a
        // non-leader — runs about itself.
        arch: current.map(|c| c.arch.clone()).unwrap_or_default(),
        binary_hash: current.map(|c| c.binary_hash.clone()).unwrap_or_default(),
        node_pubkey: current.and_then(|c| c.node_pubkey.clone()),
    }
}

/// Keep this node's own `arch` and `binary_hash` accurate in the roster.
/// Unlike the rest of a `NodeState`, these two fields describe facts only the
/// node itself can observe, so every node runs this about itself regardless
/// of leadership — the leader-driven `seed_roster` never touches them.
///
/// Also the fix for a node whose roster entry predates one of these fields:
/// such an entry keeps whatever was stored (empty) forever otherwise, since
/// `to_state` only fills a field in when the node is being created for the
/// first time.
async fn report_self(node_name: String, cluster: Arc<ClusterNode>, node_pubkey: Option<String>) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        let state = cluster.state().await;
        let Some(current) = state.nodes.get(&node_name) else {
            continue;
        };
        let arch = std::env::consts::ARCH.to_string();
        let binary_hash = current_binary_hash();
        if current.arch == arch && current.binary_hash == binary_hash && current.node_pubkey == node_pubkey {
            continue;
        }
        let desired = NodeState {
            arch,
            binary_hash,
            node_pubkey: node_pubkey.clone(),
            ..current.clone()
        };
        if let Err(e) = cluster.submit(Request::UpsertNode(desired)).await {
            warn!(error = %e, "could not report this node's arch, binary hash and public key");
        }
    }
}

/// Blake3 hash of the running binary, hex encoded.
///
/// Used by the self-update orchestrator to confirm a restarted node actually
/// came back on the freshly built binary rather than just coming back at all.
/// Empty on any failure to locate or read the executable, which is treated as
/// "unknown" rather than a startup error: reporting the roster is more
/// important than this one field.
fn current_binary_hash() -> String {
    let Ok(path) = std::env::current_exe() else {
        return String::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return String::new();
    };
    blake3::hash(&bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{ClusterConfigEntry, ClusterState};

    fn boot_cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).expect("example parses")
    }

    #[test]
    fn live_config_falls_back_to_boot_when_nothing_is_replicated_yet() {
        let state = ClusterState::default();
        assert_eq!(live_config(&state, &boot_cfg()), boot_cfg());
    }

    #[test]
    fn live_config_prefers_the_replicated_copy() {
        let mut edited = boot_cfg();
        edited.cluster.name = "edited".to_string();
        let state = ClusterState {
            cluster_config: Some(ClusterConfigEntry {
                yaml: serde_yaml::to_string(&edited).expect("encodes"),
                generation: 1,
            }),
            ..ClusterState::default()
        };

        let live = live_config(&state, &boot_cfg());
        assert_eq!(live.cluster.name, "edited");
    }

    #[test]
    fn live_config_falls_back_to_boot_on_unparseable_replicated_yaml() {
        let state = ClusterState {
            cluster_config: Some(ClusterConfigEntry { yaml: "not: valid: cluster: config".to_string(), generation: 1 }),
            ..ClusterState::default()
        };

        assert_eq!(live_config(&state, &boot_cfg()), boot_cfg());
    }
}
