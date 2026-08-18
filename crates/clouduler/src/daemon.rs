//! Daemon bootstrap. Subsystems get wired in here as later phases land.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

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

    let key = load_cluster_key(&cfg, node);

    let cluster = ClusterNode::start(Options {
        name: node_name.clone(),
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
    // before starting anything.
    let fenced = Arc::new(AtomicBool::new(false));
    let fencing = tokio::spawn(run_fence(
        cluster.clone(),
        failover.clone(),
        fenced.clone(),
    ));

    let scheduler = match cl_docker::Docker::connect() {
        Ok(docker) => {
            let materializer = Materializer::new(&data_dir, key);
            let scheduler = Scheduler::new(
                node_name.clone(),
                cluster.clone(),
                docker,
                materializer,
            )
            .with_zone(cl_net::dns::zone_origin(&cfg));
            let gate = fenced.clone();
            Some(tokio::spawn(
                scheduler.run(move || gate.load(Ordering::Relaxed)),
            ))
        }
        Err(e) => {
            // A manager with no docker is still useful: it votes, holds state
            // and serves the ui. It just runs nothing.
            warn!(error = %e, "no docker daemon, this node will not run containers");
            None
        }
    };

    // Built before the scheduler needs it, but idempotent and on a loop, so
    // ordering against a stack deploy does not matter.
    let overlay = crate::overlay::start(
        &cfg,
        &node_name,
        &cluster,
        cl_docker::Docker::connect().ok(),
    );

    let drives = match crate::drives::start(&cfg, node, &node_name, &cluster) {
        Ok(drives) => Some(drives),
        Err(e) => {
            // A node whose drives will not start still votes and still runs
            // anything that needs no volume, so this is not fatal.
            warn!(error = %e, "drive subsystem did not start");
            None
        }
    };

    // Containers resolve each other through this, so it comes up alongside the
    // overlay rather than waiting for anything to be scheduled.
    let internal_dns = crate::internal_dns::start(&cfg, &cluster);
    let exposed = crate::expose::start(&cluster);
    let proxy = crate::proxy::start(&cfg, &cluster, &data_dir);

    let api = start_api(&cfg, node, &node_name, &config_path, &cluster, &data_dir).await;

    let config_writer = tokio::spawn(watch_cluster_config(
        cluster.clone(),
        config_path.clone(),
        failover.probe_interval,
    ));

    let watcher = tokio::spawn(watch_leadership(cfg, config_path, cluster.clone()));

    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    watcher.abort();
    config_writer.abort();
    detector.abort();
    fencing.abort();
    if let Some(scheduler) = scheduler {
        scheduler.abort();
    }
    if let Some(api) = api {
        api.abort();
    }
    if let Some(overlay) = overlay {
        overlay.abort();
    }
    if let Some(internal_dns) = internal_dns {
        internal_dns.abort();
    }
    exposed.abort();
    if let Some(proxy) = proxy {
        proxy.shutdown();
    }
    // Before the raft shutdown, so the mounts are gone while the runtime that
    // serves their requests is still alive.
    if let Some(drives) = drives {
        drives.shutdown();
    }
    cluster.shutdown().await?;
    Ok(())
}

/// Stand up the http api and the web ui.
///
/// Every node listens; a node that is not the leader answers with a redirect to
/// the one that is. That is simpler than starting and stopping a listener on
/// every election, and it means someone who bookmarked a node still gets in.
async fn start_api(
    cfg: &ClusterConfig,
    node: &Node,
    node_name: &str,
    config_path: &Path,
    cluster: &Arc<ClusterNode>,
    data_dir: &Path,
) -> Option<tokio::task::JoinHandle<()>> {
    let docker = match cl_docker::Docker::connect() {
        Ok(docker) => docker,
        Err(e) => {
            warn!(error = %e, "no docker, the api will not serve container detail");
            return None;
        }
    };

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
    let options = cl_api::ApiServerOptions {
        cluster: cluster.clone(),
        docker,
        node_name: node_name.to_string(),
        config_path: config_path.to_path_buf(),
        cluster_config: cfg.clone(),
        key_path: cfg.secrets.key_file.clone(),
        key: load_cluster_key(cfg, node),
        tls_cert_pem: cert_pem.into_bytes(),
        tls_key_pem: key_pem.into_bytes(),
        listen_addr,
    };
    let _ = data_dir;

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

/// Load the key that opens stored secrets, creating it on first launch.
///
/// A node without a key is not an error: it replicates sealed secrets like any
/// other state and simply cannot run containers that need them. That is the
/// intended setup for a worker you do not want holding the keys to everything.
fn load_cluster_key(cfg: &ClusterConfig, node: &Node) -> Option<ClusterKey> {
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
                info!("quorum restored, containers may start again");
                fenced.store(false, Ordering::Relaxed);
            }
            FenceAction::None => {}
        }
    }
}

/// Write the replicated cluster config to this node's config file whenever it
/// changes.
///
/// This is what makes an edit in the web ui reach every node: the api submits
/// it through raft, and each daemon lands its own copy here. A node that was
/// down during the edit picks it up on its next tick after rejoining, rather
/// than coming back with a stale file.
///
/// Running subsystems are not reconfigured in place, so the file is ahead of
/// the process until the daemon restarts. That is deliberate: rebuilding the
/// overlay or the raft membership under a running cluster from a config diff
/// is not something to do behind the operator's back.
async fn watch_cluster_config(
    cluster: Arc<ClusterNode>,
    config_path: PathBuf,
    interval: std::time::Duration,
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
                    "cluster config updated, restart the daemon to apply it"
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

/// Keep the replicated roster in step with the config for as long as this node
/// leads. Seeding is done on every leadership change rather than once at
/// startup, because at startup no leader has been elected yet.
async fn watch_leadership(cfg: ClusterConfig, config_path: PathBuf, cluster: Arc<ClusterNode>) {
    let mut quorum = cluster.quorum();
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
            if let Err(e) = seed_roster(&cfg, &cluster).await {
                warn!(error = %e, "could not publish the node roster");
            }
        }

        if !quorum.changed().await {
            return;
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
        .filter_map(|(name, node)| {
            Some((node_id(name), BasicNode::new(cfg.rpc_addr(node)?.to_string())))
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
        let Some(rpc_addr) = cfg.rpc_addr(node) else {
            warn!(node = %name, "skipping node with no internal address");
            continue;
        };

        // A node the roster has never seen is one that joined after the cluster
        // was formed, so it still has to be added to raft membership. Managers
        // become voters, workers only learners.
        if !known.nodes.contains_key(name) {
            let result = match node.role {
                NodeRole::Manager => cluster.add_manager(node_id(name), rpc_addr).await,
                NodeRole::Worker => cluster.add_worker(node_id(name), rpc_addr).await,
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
        role: match node.role {
            NodeRole::Manager => Role::Manager,
            NodeRole::Worker => Role::Worker,
        },
        rpc_addr,
        drives: node.drives.clone(),
        availability: current
            .map(|c| c.availability)
            .unwrap_or(Availability::Active),
    }
}
