//! Wiring the reverse proxy into the daemon.
//!
//! Every node runs the listeners and every node holds the whole routing table,
//! because a route's hostname can resolve to any node and the backend it
//! points at is reachable from all of them: directly when the node runs it,
//! through the owning node's relay (`crate::relay`) otherwise. Certificates come
//! out of replicated state for the same reason: whichever node a client lands
//! on has to be able to terminate TLS for the name it asked for.
//!
//! Issuance and renewal are the leader's alone. Several nodes ordering the
//! same certificate at once would burn the CA's rate limit and race each other
//! writing the result back.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::supervisor::{log_err, Supervisor};
use cl_api::cloudflare::CLOUDFLARE_TOKEN_SECRET;
use cl_api::redirects::RedirectStore;
use cl_api::static_routes::StaticRouteStore;
use cl_config::{qualify, ClusterConfig, NetworkKind, StackFile};
use cl_dns::{Cloudflare, DesiredRecord};
use cl_net::egress::nat_subnet;
use cl_net::relay::Dialer;
use cl_net::Allocations;
use cl_proxy::{
    ensure_crypto_provider, issue_certificate, issue_certificate_http01, AcmeConfig, CertStore,
    Http01Store, LocalClients, RedirectTarget, Route, RouteOptions, RouteTable, ZoneResolver,
    DEFAULT_PROPAGATION_TIMEOUT, LETS_ENCRYPT_STAGING_URL,
};
use cl_raft::node::ClusterNode;
use cl_raft::state::{
    dns_key, CertificateState, ClusterState, DnsRecordState, ReconcileStatus, Request,
};
use cl_sched::run::{overlay_name, overlay_network_name};
use cl_secret::{ClusterKey, Sealed};
use ipnet::IpNet;
use tracing::{info, warn};

/// Restricts a route to clients on `network`, when that network is subnetted
/// (internal or private): its own subnet plus the subnet of every subnetted
/// network whose `access` lists it. A public network is reachable from
/// anywhere, so it gets no restriction (an empty list).
fn allowed_subnets(cfg: &ClusterConfig, network: &str) -> Vec<IpNet> {
    let Some(net) = cfg.networks.get(network) else {
        return Vec::new();
    };
    if !net.kind.is_subnetted() {
        return Vec::new();
    }
    let mut out: Vec<IpNet> = net.subnet.into_iter().collect();
    for (name, other) in &cfg.networks {
        if name != network && other.kind.is_subnetted() && cfg.reaches(name, network) {
            out.extend(other.subnet);
        }
    }
    out
}

/// `network` plus every other network that reaches it, the networks whose
/// clients a route on `network` serves.
fn client_networks<'a>(cfg: &'a ClusterConfig, network: &'a str) -> Vec<&'a str> {
    let mut out = vec![network];
    out.extend(
        cfg.networks
            .iter()
            .filter(|(name, net)| {
                name.as_str() != network
                    && net.kind != NetworkKind::Public
                    && cfg.reaches(name, network)
            })
            .map(|(name, _)| name.as_str()),
    );
    out
}

/// Overlay names of every deployed stack whose overlay network (the one the
/// scheduler builds for it, `overlay_network_name`) is exactly `network`.
fn overlays_on_network(cfg: &ClusterConfig, state: &ClusterState, network: &str) -> Vec<String> {
    state
        .stacks
        .iter()
        .filter_map(|(name, stack)| {
            let file = StackFile::parse(&stack.spec).ok()?;
            (overlay_network_name(&file, cfg)? == network).then(|| overlay_name(name, network))
        })
        .collect()
}

/// Overlay subnets of every stack published on `network`, plus every nat
/// network's own subnet, so a same-node client whose request never left the node
/// still passes `gate_by_network` (see `RouteOptions::trusted_overlay_subnets`).
///
/// A nat subnet is trusted for every gated route, not just one matching by name:
/// a nat-attached client's traffic is masqueraded out whatever route the host
/// picks, so it can hairpin back into any local route. That packet resolves to
/// local delivery before `POSTROUTING`, so the proxy sees the container's raw
/// nat-subnet address.
fn trusted_overlay_subnets(cfg: &ClusterConfig, state: &ClusterState, network: &str) -> Vec<IpNet> {
    let mut subnets: Vec<IpNet> = match Allocations::decode(state.overlay.as_deref()) {
        Some(allocations) => {
            // Exact keys from state, never a parse of the overlay name back:
            // `x_a_b` is stack `x` on `a_b` as much as stack `x_a` on `b`.
            let overlays: Vec<String> = client_networks(cfg, network)
                .into_iter()
                .flat_map(|n| overlays_on_network(cfg, state, n))
                .collect();
            allocations
                .subnets_of_overlays(overlays.iter().map(String::as_str))
                .map(IpNet::V4)
                .collect()
        }
        None => Vec::new(),
    };
    subnets.extend(
        cfg.networks
            .iter()
            .filter(|(_, net)| net.kind == NetworkKind::Nat)
            .map(|(name, _)| IpNet::V4(nat_subnet(name))),
    );
    subnets
}

/// Clients of `network` that arrive through a container network's router:
/// their connections land on the node's own address on that network's
/// segment, so the local address is the identity. Every container network
/// that is `network` or lists it in `access` contributes its nodes'
/// segment addresses.
fn local_clients(cfg: &ClusterConfig, state: &ClusterState, network: &str) -> LocalClients {
    let only = cfg
        .networks
        .get(network)
        .is_some_and(|n| n.kind == NetworkKind::Container);
    let mut addrs = Vec::new();
    if let Some(allocations) = Allocations::decode(state.overlay.as_deref()) {
        for (name, _) in cfg.container_networks() {
            if !cfg.reaches(name, network) {
                continue;
            }
            let segment = cl_net::container_net::container_docker_name(name);
            addrs.extend(
                allocations
                    .node_ips(&segment)
                    .map(|(_, ip)| std::net::IpAddr::V4(ip)),
            );
        }
    }
    LocalClients { addrs, only }
}

/// Addresses that count as "inside the cluster" for the `authenticated`
/// middleware's internal bypass: every container overlay subnet, every nat
/// network's subnet, and each node's own internal address (a container on
/// another node reaches the proxy masqueraded as its node). Deliberately not
/// the internal/private networks' whole subnets, which other machines on the
/// LAN may also sit in.
fn internal_peer_subnets(cfg: &ClusterConfig, state: &ClusterState) -> Vec<IpNet> {
    let mut subnets: Vec<IpNet> = Vec::new();
    if let Some(allocations) = Allocations::decode(state.overlay.as_deref()) {
        // A container network's segment is left out: its router forwards
        // outside traffic and masquerades it to an address inside the segment,
        // so trusting the segment would let that traffic skip authentication.
        // Its client containers are covered by their own stack overlay. A
        // managed network is left out too: its subnet is also reachable from
        // outside, so it cannot vouch for its source.
        subnets.extend(
            allocations
                .subnets_of_overlays(allocations.network_names().filter(|name| {
                    !name.starts_with("CTN_") && !cl_net::ipam::is_managed_key(name)
                }))
                .map(IpNet::V4),
        );
    }
    subnets.extend(
        cfg.networks
            .iter()
            .filter(|(_, net)| net.kind == NetworkKind::Nat)
            .map(|(name, _)| IpNet::V4(nat_subnet(name))),
    );
    subnets.extend(
        cfg.nodes
            .values()
            .filter_map(|node| cfg.internal_ip(node))
            .map(IpNet::from),
    );
    subnets
}

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// How often the leader looks for a name with no certificate, and for one
/// near expiry. Nothing is gained by checking a 30 day renewal window more
/// often than this, and a newly deployed route waits at most one pass.
const ISSUANCE_INTERVAL: Duration = Duration::from_secs(300);

pub struct Proxy {
    supervisor: tokio::task::JoinHandle<()>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Latest active health check per route, shared with the api so
    /// `/api/instances` and `clouduler stack unhealthy` can report a backend
    /// the proxy can't reach without waiting for a real client to hit it.
    pub health: cl_proxy::HealthTable,
    /// The live routing table, shared with the api for `/api/proxy/routes`.
    pub routes: cl_proxy::SharedRoutes,
}

impl Proxy {
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        log_err("the proxy supervisor failed", self.supervisor.await);
    }
}

/// The parts of the config that decide which proxy tasks run, compared on
/// every pass so the supervisor only restarts what actually changed instead
/// of the whole subsystem on any edit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Desired {
    listen: Option<(u16, u16)>,
    /// Also listen on `[::]` (IPv6 only): this node has a public network
    /// with an `ipv6` address, so AAAA records point clients at it.
    v6: bool,
    acme: Option<(Vec<String>, String)>,
    dns: bool,
    /// Not a task input: the loops re-read the zone list every pass. It is
    /// here so a change wakes them instead of waiting out their ticker.
    zones: Vec<String>,
}

fn desired_of(cfg: &ClusterConfig, node: &str) -> Desired {
    Desired {
        listen: cfg
            .proxy
            .enabled
            .then_some((cfg.proxy.http_port, cfg.proxy.https_port)),
        v6: cfg.proxy.enabled && cfg.node_has_public_v6(node),
        acme: acme_setup(cfg, Path::new(""))
            .map(|a| (a.config.contact_emails, a.config.directory_url)),
        dns: cfg.cloudflare.is_some(),
        zones: cfg
            .cloudflare
            .as_ref()
            .map(|c| c.zones.clone())
            .unwrap_or_default(),
    }
}

/// What the proxy's task builders share, so none of them takes a dozen
/// separate arguments.
struct TaskCtx {
    cluster: Arc<ClusterNode>,
    data_dir: PathBuf,
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    health: cl_proxy::HealthTable,
    http01: Arc<Http01Store>,
    dialer: Dialer,
    shared_routes: cl_proxy::SharedRoutes,
    nudges: cl_api::Nudges,
}

struct Group {
    tasks: Supervisor,
}

impl Group {
    fn teardown(mut self) {
        self.tasks.abort_all();
    }
}

/// Proxy ports bound at the very start of the daemon, before raft is up, so a
/// restart leaves clients queued in the listen backlog instead of refused.
/// Taken by `build_listen`; a port that was not pre-bound is bound there.
/// Keyed by port and whether the socket is the IPv6-only one.
static PREBOUND: parking_lot::Mutex<BTreeMap<(u16, bool), std::net::TcpListener>> =
    parking_lot::Mutex::new(BTreeMap::new());

/// A non-blocking listener on `[::]:port` that accepts only IPv6, so it
/// coexists with the `0.0.0.0` one whatever `net.ipv6.bindv6only` says.
fn bind_v6(port: u16) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(true)?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

fn bind_v4(port: u16) -> std::io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Bind the proxy's ports now. Failures are ignored: `build_listen` binds
/// again and reports the error where it always did.
pub fn prebind(cfg: &ClusterConfig, node: &str) {
    let desired = desired_of(cfg, node);
    let Some((http_port, https_port)) = desired.listen else {
        return;
    };
    let mut bound = PREBOUND.lock();
    // Sockets systemd already holds for us (see `clouduler.socket`) are the
    // best case: they were bound before this process existed.
    for listener in crate::systemd::take_listen_fds() {
        if let Ok(addr) = listener.local_addr() {
            bound.insert((addr.port(), addr.is_ipv6()), listener);
        }
    }
    use std::collections::btree_map::Entry;
    for port in [http_port, https_port] {
        if let Entry::Vacant(slot) = bound.entry((port, false)) {
            if let Ok(listener) = bind_v4(port) {
                slot.insert(listener);
            }
        }
        if desired.v6 {
            if let Entry::Vacant(slot) = bound.entry((port, true)) {
                if let Ok(listener) = bind_v6(port) {
                    slot.insert(listener);
                }
            }
        }
    }
}

fn take_listener(port: u16, v6: bool) -> std::io::Result<tokio::net::TcpListener> {
    let prebound = PREBOUND.lock().remove(&(port, v6));
    match prebound {
        Some(listener) => tokio::net::TcpListener::from_std(listener),
        None => {
            let std = if v6 { bind_v6(port)? } else { bind_v4(port)? };
            tokio::net::TcpListener::from_std(std)
        }
    }
}

pub struct StartProxyArgs<'a> {
    pub cfg: &'a ClusterConfig,
    pub node: &'a str,
    pub cluster: &'a Arc<ClusterNode>,
    pub data_dir: &'a Path,
    pub config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    pub key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    pub dialer: Dialer,
    pub nudges: cl_api::Nudges,
}

pub fn start(args: StartProxyArgs<'_>) -> Proxy {
    let StartProxyArgs {
        cfg,
        node,
        cluster,
        data_dir,
        config_rx,
        key_rx,
        dialer,
        nudges,
    } = args;
    ensure_crypto_provider();

    let cluster = cluster.clone();
    let data_dir = data_dir.to_path_buf();
    let boot_cfg = cfg.clone();
    let node = node.to_string();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let health = cl_proxy::HealthTable::default();
    let task_health = health.clone();
    let shared_routes = cl_proxy::SharedRoutes::default();
    let task_routes = shared_routes.clone();
    // Shared across `listen` and `acme` regardless of which one restarts: an
    // HTTP-01 token the issuance task hands out must still be answered by
    // whichever listener is live when the CA's validator comes knocking.
    let http01 = Arc::new(Http01Store::default());

    let mut config_rx = config_rx;
    let supervisor = tokio::spawn(async move {
        let ctx = TaskCtx {
            cluster,
            data_dir,
            config_rx: config_rx.clone(),
            key_rx,
            health: task_health,
            http01,
            dialer,
            shared_routes: task_routes,
            nudges,
        };
        let mut cfg = boot_cfg;
        let mut desired = desired_of(&cfg, &node);
        let mut listen = build_listen(&desired, &ctx);
        let mut acme = build_acme(&desired, &cfg, &ctx);
        let mut dns = build_dns(&desired, &ctx);

        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                changed = config_rx.changed() => {
                    if changed.is_err() {
                        continue;
                    }
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        if let Some(group) = listen { group.teardown(); }
                        if let Some(group) = acme { group.teardown(); }
                        if let Some(group) = dns { group.teardown(); }
                        return;
                    }
                }
            }

            cfg = (**config_rx.borrow()).clone();
            let live = desired_of(&cfg, &node);
            if live == desired {
                continue;
            }
            if live.zones != desired.zones {
                info!("cloudflare zones changed, waking the dns and certificate loops");
                ctx.nudges.dns.notify_one();
                ctx.nudges.acme.notify_one();
            }
            if live.listen == desired.listen
                && live.v6 == desired.v6
                && live.acme == desired.acme
                && live.dns == desired.dns
            {
                desired = live;
                continue;
            }
            info!("reverse proxy config changed, restarting the affected tasks");

            if live.listen != desired.listen || live.v6 != desired.v6 {
                if let Some(group) = listen.take() {
                    group.teardown();
                }
                listen = build_listen(&live, &ctx);
            }
            if live.acme != desired.acme {
                if let Some(group) = acme.take() {
                    group.teardown();
                }
                acme = build_acme(&live, &cfg, &ctx);
            }
            if live.dns != desired.dns {
                if let Some(group) = dns.take() {
                    group.teardown();
                }
                dns = build_dns(&live, &ctx);
            }
            desired = live;
        }
    });

    Proxy {
        supervisor,
        shutdown_tx,
        health,
        routes: shared_routes,
    }
}

/// The two listeners plus the routing table/certificate reconcile loop.
/// `None` when the proxy is disabled.
fn build_listen(desired: &Desired, ctx: &TaskCtx) -> Option<Group> {
    let TaskCtx {
        cluster,
        config_rx,
        key_rx,
        health,
        http01,
        dialer,
        shared_routes,
        ..
    } = ctx;
    let Some((http_port, https_port)) = desired.listen else {
        shared_routes.clear();
        return None;
    };

    let routes = Arc::new(RouteTable::default());
    shared_routes.set(routes.clone());
    let peers = Arc::new(cl_proxy::PeerPolicies::new());
    let certs = Arc::new(CertStore::new());
    let auth_gate =
        match cl_proxy::AuthGate::new(config_rx.clone(), key_rx.clone(), cluster.clone()) {
            Ok(gate) => Arc::new(gate),
            Err(e) => {
                warn!(error = %e, "cannot build the auth gate http client; proxy not started");
                return None;
            }
        };
    let server = Arc::new(
        cl_proxy::ProxyServer::full(
            routes.clone(),
            certs.clone(),
            Some(auth_gate),
            http01.clone(),
        )
        .with_dialer(dialer.clone())
        .with_peer_policies(peers.clone()),
    );
    let mut tasks = Supervisor::new();
    // Flipped by `reconcile_loop` once the first full route set is in place.
    let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);

    let families: &[bool] = if desired.v6 { &[false, true] } else { &[false] };
    for (port, tls, v6) in [(http_port, false), (https_port, true)]
        .into_iter()
        .flat_map(|(port, tls)| families.iter().map(move |&v6| (port, tls, v6)))
    {
        let addr = if v6 {
            SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port))
        } else {
            SocketAddr::from(([0, 0, 0, 0], port))
        };
        let server = server.clone();
        let mut ready = ready_rx.clone();
        // Bound here, synchronously, so the port is held from now on.
        let listener = take_listener(port, v6);
        tasks.spawn("proxy-listener", async move {
            // Connections made before the routes load wait in the backlog
            // rather than being answered 404 for every host. Bounded, so a
            // reconcile that never finishes cannot keep the proxy dark.
            let _ = tokio::time::timeout(Duration::from_secs(3), ready.wait_for(|r| *r)).await;
            let served = match listener {
                Ok(listener) if tls => server.serve_https_on(listener).await,
                Ok(listener) => server.serve_http_on(listener).await,
                Err(e) => Err(e.into()),
            };
            if let Err(e) = served {
                // Ports below 1024 need root, and that is the usual reason.
                warn!(%addr, tls, error = %e, "proxy listener stopped");
            }
        });
        info!(%addr, tls, "proxy listening");
    }

    tasks.spawn(
        "proxy-reconcile",
        reconcile_loop(
            config_rx.clone(),
            cluster.clone(),
            routes.clone(),
            peers,
            certs,
            ready_tx,
        ),
    );
    tasks.spawn(
        "proxy-health-checks",
        cl_proxy::run_health_checks(
            routes,
            health.clone(),
            cl_proxy::HEALTH_CHECK_INTERVAL,
            dialer.clone(),
        ),
    );

    Some(Group { tasks })
}

/// The certificate issuance/renewal loop. `None` when ACME issuance is
/// turned off in config, or there is no Cloudflare section to issue against.
fn build_acme(desired: &Desired, cfg: &ClusterConfig, ctx: &TaskCtx) -> Option<Group> {
    desired.acme.as_ref()?;
    let acme = acme_setup(cfg, &ctx.data_dir)?;
    let mut tasks = Supervisor::new();
    tasks.spawn(
        "proxy-acme",
        run_acme(
            acme,
            ctx.cluster.clone(),
            ctx.config_rx.clone(),
            ctx.key_rx.clone(),
            ctx.http01.clone(),
            ctx.nudges.acme.clone(),
        ),
    );
    Some(Group { tasks })
}

/// The Cloudflare DNS record reconcile loop. `None` when there is no
/// Cloudflare section at all.
fn build_dns(desired: &Desired, ctx: &TaskCtx) -> Option<Group> {
    if !desired.dns {
        return None;
    }
    let mut tasks = Supervisor::new();
    tasks.spawn(
        "proxy-dns-reconcile",
        run_dns_reconcile(
            ctx.cluster.clone(),
            ctx.config_rx.clone(),
            ctx.key_rx.clone(),
            ctx.nudges.dns.clone(),
        ),
    );
    Some(Group { tasks })
}

async fn reconcile_loop(
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    cluster: Arc<ClusterNode>,
    routes: Arc<RouteTable>,
    peers: Arc<cl_proxy::PeerPolicies>,
    certs: Arc<CertStore>,
    ready: tokio::sync::watch::Sender<bool>,
) {
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    let mut published = 0;

    loop {
        ticker.tick().await;
        let cfg = config_rx.borrow().clone();
        let state = cluster.snapshot().await;

        // Before the routes, so a request never meets a route its caller's
        // stack has not yet been restricted from.
        // Unknown state keeps the policies already in force.
        if let Some(stacks) = crate::restrict::restricted_stacks(&cfg, &state) {
            peers.replace(crate::restrict::proxy_policies(&stacks));
        }
        peers.set_internal(internal_peer_subnets(&cfg, &state));

        // Static backends need a dns lookup each. On the first pass publish
        // what is already known rather than leaving every hostname unrouted
        // until those resolve.
        if published == 0 {
            routes.reload(routes_for(&cfg, &state));
        }
        let mut built = routes_for(&cfg, &state);
        built.extend(static_backend_routes(&cfg, &state).await);
        if built.len() != published {
            info!(routes = built.len(), "reverse proxy routes changed");
            published = built.len();
        }
        routes.reload(built);

        let stored: Vec<CertificateState> = state.certificates.values().cloned().collect();
        certs.load(&stored);
        ready.send_replace(true);
    }
}

/// The routing table as replicated state describes it.
///
/// A route with no backend is still published: the proxy answering 502 for a
/// host it knows about is a better answer than 404, and it means a redeploy
/// does not make the hostname briefly look unconfigured.
fn routes_for(cfg: &ClusterConfig, state: &ClusterState) -> Vec<Route> {
    let mut routes = Vec::new();

    for placement in crate::placement::service_placements(state, cfg) {
        let instance = format!("{}.{}", placement.stack, placement.service);
        for entry in &placement.extensions.proxy {
            // A managed network without `proxy: true` only exposes its raw
            // open_ports; the proxy serves none of its hostnames.
            if cfg.is_managed_network(&entry.network) && !cfg.managed_proxy(&entry.network) {
                continue;
            }
            let domain = cfg
                .networks
                .get(&entry.network)
                .and_then(|n| n.domain.as_deref());
            routes.push(
                Route::new(
                    qualify(&entry.host, domain),
                    entry.path_prefix.clone(),
                    entry.tls,
                    entry.middlewares.clone(),
                    entry.sticky.unwrap_or(false),
                    instance.clone(),
                    placement
                        .addresses
                        .iter()
                        .map(|ip| SocketAddr::from((*ip, entry.port)))
                        .collect(),
                )
                .with_options(RouteOptions {
                    max_body_size: entry.max_body_size.map(|s| s.bytes()),
                    websockets: entry.websockets,
                    preserve_host: entry.preserve_host,
                    response_timeout: entry.response_timeout,
                    http2: entry.http2,
                    allowed_subnets: allowed_subnets(cfg, &entry.network),
                    trusted_overlay_subnets: trusted_overlay_subnets(cfg, state, &entry.network),
                    local_clients: local_clients(cfg, state, &entry.network),
                    network: Some(entry.network.clone()),
                }),
            );
        }
    }

    routes.extend(redirect_routes_for(cfg, state));

    if let Some(dashboard) = dashboard_route(cfg, state) {
        routes.push(dashboard);
    }
    routes
}

/// Routes for the UI-managed redirects, stored outside `cluster.yaml`; see
/// `cl_api::redirects`. A redirect never proxies to a backend, so this needs
/// no DNS resolution and can run synchronously alongside the placement based
/// routes above.
fn redirect_routes_for(cfg: &ClusterConfig, state: &ClusterState) -> Vec<Route> {
    RedirectStore::load(state)
        .rules
        .into_iter()
        .map(|rule| {
            let domain = cfg
                .networks
                .get(&rule.network)
                .and_then(|n| n.domain.as_deref());
            Route::new(
                qualify(&rule.host, domain),
                rule.path_prefix.clone(),
                // Always true, so the old hostname gets a certificate and can
                // answer the redirect over https too, not just http.
                true,
                Vec::new(),
                false,
                format!("clouduler.redirect.{}", rule.id),
                Vec::new(),
            )
            .with_options(RouteOptions {
                allowed_subnets: allowed_subnets(cfg, &rule.network),
                trusted_overlay_subnets: trusted_overlay_subnets(cfg, state, &rule.network),
                local_clients: local_clients(cfg, state, &rule.network),
                network: Some(rule.network.clone()),
                ..RouteOptions::default()
            })
            .with_redirect(RedirectTarget {
                url: rule.target,
                preserve_path: rule.preserve_path,
                status: rule.status,
            })
        })
        .collect()
}

/// The dashboard, published like any other route so it picks up the Let's Encrypt
/// certificate and Cloudflare record from the machinery that already runs for
/// every service.
///
/// The backend is this node's own api over loopback. Each node's proxy serves its
/// own api rather than load balancing, because the hop carries session cookies
/// over an unencrypted overlay, and a non-leader api already redirects to the
/// leader, so spreading load would only add a redirect.
///
/// `preserve_host` is on because the api builds its OIDC redirect URL from the
/// request, and `websockets` because `/api/events` and the log streams are
/// server-sent events that must not be buffered or capped.
fn dashboard_route(cfg: &ClusterConfig, state: &ClusterState) -> Option<Route> {
    if !cfg.proxy.enabled || !cfg.web.through_proxy {
        return None;
    }
    Some(
        Route::new(
            cfg.web_fqdn(),
            None,
            true,
            Vec::new(),
            false,
            "clouduler.dashboard",
            vec![SocketAddr::from(([127, 0, 0, 1], cfg.web.backend_port))],
        )
        .with_options(RouteOptions {
            preserve_host: true,
            websockets: true,
            // No body cap and no response timeout: a stack file upload can be
            // large, and the event stream never returns headers late but does
            // stay open indefinitely.
            allowed_subnets: allowed_subnets(cfg, &cfg.web.network),
            // A client (e.g. an MCP client container) on the node serving the
            // dashboard reaches the proxy with its raw overlay address, which
            // is outside `allowed_subnets`; see `trusted_overlay_subnets`.
            trusted_overlay_subnets: trusted_overlay_subnets(cfg, state, &cfg.web.network),
            local_clients: local_clients(cfg, state, &cfg.web.network),
            network: Some(cfg.web.network.clone()),
            ..RouteOptions::default()
        }),
    )
}

/// Static/external backend routes, UI-managed and stored outside
/// `cluster.yaml`; see `cl_api::static_routes`. `backend` is resolved by DNS
/// on every reconcile pass, so a hostname backend picks up a changed address
/// without touching any config; a plain IP resolves to itself immediately.
async fn static_backend_routes(cfg: &ClusterConfig, state: &ClusterState) -> Vec<Route> {
    let entries = StaticRouteStore::load(state).routes;
    // Resolved concurrently: one slow or unreachable resolver answer should
    // not delay every other route, and a restart waits on all of them.
    let mut lookups = tokio::task::JoinSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let backend = entry.backend.clone();
        lookups.spawn(async move {
            let found = tokio::net::lookup_host(backend)
                .await
                .map(|addrs| addrs.collect::<Vec<_>>());
            (index, found)
        });
    }
    let mut resolved = BTreeMap::new();
    while let Some(Ok((index, found))) = lookups.join_next().await {
        resolved.insert(index, found);
    }

    let mut routes = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let domain = cfg
            .networks
            .get(&entry.network)
            .and_then(|n| n.domain.as_deref());
        let backends = match resolved.remove(&index) {
            Some(Ok(addrs)) => addrs,
            Some(Err(e)) => {
                warn!(host = %entry.host, backend = %entry.backend, error = %e, "could not resolve static backend");
                Vec::new()
            }
            None => Vec::new(),
        };
        routes.push(
            Route::new(
                qualify(&entry.host, domain),
                entry.path_prefix.clone(),
                entry.tls,
                entry.middlewares.clone(),
                false,
                format!("clouduler.static.{}", entry.host),
                backends,
            )
            .with_options(RouteOptions {
                max_body_size: entry.max_body_size.map(|s| s.bytes()),
                websockets: entry.websockets,
                preserve_host: entry.preserve_host,
                response_timeout: entry.response_timeout,
                http2: false,
                allowed_subnets: allowed_subnets(cfg, &entry.network),
                trusted_overlay_subnets: trusted_overlay_subnets(cfg, state, &entry.network),
                local_clients: local_clients(cfg, state, &entry.network),
                network: Some(entry.network.clone()),
            }),
        );
    }
    routes
}

/// Every hostname the proxy terminates TLS for, mapped to whether it may fall
/// back to ACME HTTP-01 when no Cloudflare zone covers it.
///
/// The fallback is opt-in per hostname: the author writes the host with a
/// trailing dot (`app.example.net.`, the zone-file "absolute name" convention).
/// `qualify` strips the dot from the fqdn used, but the raw `host` checked here
/// signals the author knows no zone matches and wants HTTP-01 instead of the "no
/// cloudflare zone covers this hostname" error.
///
/// Static routes are folded in directly from the store, not through
/// `static_backend_routes`, so this never depends on DNS resolving their
/// backends.
fn tls_hosts(cfg: &ClusterConfig, state: &ClusterState) -> BTreeMap<String, bool> {
    fn insert(hosts: &mut BTreeMap<String, bool>, cfg: &ClusterConfig, host: &str, network: &str) {
        let domain = cfg.networks.get(network).and_then(|n| n.domain.as_deref());
        let fqdn = qualify(host, domain);
        let allow_http01 = host.ends_with('.');
        let entry = hosts.entry(fqdn).or_insert(false);
        *entry = *entry || allow_http01;
    }

    let mut hosts: BTreeMap<String, bool> = BTreeMap::new();
    for placement in crate::placement::service_placements(state, cfg) {
        for entry in &placement.extensions.proxy {
            if entry.tls {
                insert(&mut hosts, cfg, &entry.host, &entry.network);
            }
        }
    }
    for rule in RedirectStore::load(state).rules {
        insert(&mut hosts, cfg, &rule.host, &rule.network);
    }
    for entry in StaticRouteStore::load(state).routes {
        if entry.tls {
            insert(&mut hosts, cfg, &entry.host, &entry.network);
        }
    }
    // The ui is served by the api on its own port, but its certificate is
    // issued the same way and stored in the same place.
    hosts.entry(cfg.web_fqdn()).or_insert(false);
    // The proxy login portal is a hostname of its own, answered by the proxy.
    if let Some(auth) = &cfg.proxy.auth {
        for (_, fqdn) in auth.portals(cfg) {
            hosts.entry(fqdn).or_insert(false);
        }
    }
    hosts
}

struct Acme {
    config: AcmeConfig,
    credentials: PathBuf,
}

/// Opens the cloudflare token out of replicated secrets, decrypting with this
/// node's cluster key. Re-read on every call rather than resolved once at
/// startup: the token is set (and can be rotated) from the web ui while the
/// daemon is already running, and at the moment this node starts, replication
/// may not have caught up to a token set on another node yet.
async fn cloudflare_token(cluster: &ClusterNode, key: Option<&ClusterKey>) -> Option<String> {
    let key = key?;
    let state = cluster.snapshot().await;
    let entry = state.secrets.get(CLOUDFLARE_TOKEN_SECRET)?;
    match Sealed::from_blob(entry.sealed.clone()).open(key, CLOUDFLARE_TOKEN_SECRET) {
        Ok(plaintext) => match String::from_utf8(plaintext) {
            Ok(token) => Some(token),
            Err(_) => {
                warn!("the cloudflare token secret is not utf8");
                None
            }
        },
        Err(e) => {
            warn!(error = %e, "cannot open the cloudflare token secret");
            None
        }
    }
}

/// A Cloudflare client and the zone names it's allowed to write to, or
/// `None` with the reason logged. Shared by certificate issuance and DNS
/// record publishing: both need the same client and the same zone list, and
/// neither should run without a readable token.
async fn cloudflare_setup(
    cfg: &ClusterConfig,
    cluster: &ClusterNode,
    key: Option<&ClusterKey>,
    context: &str,
) -> Option<(Cloudflare, Vec<String>)> {
    let cloudflare = cfg.cloudflare.as_ref()?;
    let token = match cloudflare_token(cluster, key).await {
        Some(token) => token,
        None => {
            info!("no cloudflare token yet, {context} will not run");
            return None;
        }
    };
    Some((Cloudflare::new(token), cloudflare.zones.clone()))
}

/// Read the ACME setup out of the config, or say why there is none. The
/// cloudflare client itself is resolved per tick in `run_acme`, not here,
/// since the token can arrive or rotate after this runs.
fn acme_setup(cfg: &ClusterConfig, data_dir: &Path) -> Option<Acme> {
    if !cfg.proxy.acme.enabled {
        return None;
    }
    if cfg.cloudflare.is_none() {
        // Rejected by validation, so only reachable through a config that
        // was already stored when the rule did not exist.
        warn!("certificate issuance needs a cloudflare section");
        return None;
    }

    let mut config = AcmeConfig {
        contact_emails: cfg.proxy.acme.contact_email.clone().into_iter().collect(),
        ..AcmeConfig::default()
    };
    if let Some(url) = &cfg.proxy.acme.directory_url {
        config.directory_url = url.clone();
    }

    Some(Acme {
        config,
        credentials: data_dir.join("acme-account.json"),
    })
}

/// Issue what is missing, then renew what is expiring, for as long as this
/// node leads. One loop rather than two, so a certificate is never ordered
/// twice in a pass by an issuing task and a renewing one disagreeing about
/// what is stored.
async fn run_acme(
    acme: Acme,
    cluster: Arc<ClusterNode>,
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    http01: Arc<Http01Store>,
    nudge: Arc<tokio::sync::Notify>,
) {
    let account = match cl_proxy::load_or_create_account(&acme.config, &acme.credentials).await {
        Ok(account) => account,
        Err(e) => {
            warn!(error = %e, "cannot use the acme account, no certificates will be issued");
            return;
        }
    };
    if acme.config.directory_url == LETS_ENCRYPT_STAGING_URL {
        warn!("issuing against the acme staging directory, certificates will not be trusted");
    }

    let mut ticker = tokio::time::interval(ISSUANCE_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = nudge.notified() => {}
        }
        if !cluster.is_leader() {
            continue;
        }
        let cfg = config_rx.borrow().clone();
        let key = key_rx.borrow().clone();
        prune_stale_status(&cluster, &cfg, &cluster.state().await, true, false).await;
        let Some((cloudflare, zone_names)) =
            cloudflare_setup(&cfg, &cluster, key.as_deref(), "certificate issuance").await
        else {
            continue;
        };
        let zones = match Zones::resolve(&cloudflare, &zone_names).await {
            Ok(zones) => zones,
            Err(e) => {
                warn!(error = %e, "cannot resolve the cloudflare zones");
                continue;
            }
        };
        issue_missing(&cfg, &cluster, &account, &cloudflare, &zones, &http01).await;
        let hosts = tls_hosts(&cfg, &cluster.state().await);
        let allow_http01 = move |fqdn: &str| hosts.get(fqdn).copied().unwrap_or(false);
        cl_proxy::renewal_tick(
            &cluster,
            &account,
            &cloudflare,
            &zones,
            &allow_http01,
            &http01,
            DEFAULT_PROPAGATION_TIMEOUT,
        )
        .await;
    }
}

/// Order a certificate for every tls hostname that has none yet. Leader only,
/// checked by the caller.
async fn issue_missing(
    cfg: &ClusterConfig,
    cluster: &Arc<ClusterNode>,
    account: &instant_acme::Account,
    cloudflare: &Cloudflare,
    zones: &Zones,
    http01: &Http01Store,
) {
    let state = cluster.snapshot().await;
    for (host, allow_http01) in tls_hosts(cfg, &state) {
        if state.certificates.contains_key(&host) {
            continue;
        }
        let zone_id = zones.zone_id_for(&host);
        if zone_id.is_none() && !allow_http01 {
            warn!(%host, "no cloudflare zone covers this name, not issuing");
            set_cert_status(
                cluster,
                &host,
                ReconcileStatus::Error {
                    message: "no cloudflare zone covers this hostname".to_string(),
                    at: now(),
                },
            )
            .await;
            continue;
        }
        if state.cert_status.get(&host) != Some(&ReconcileStatus::Pending) {
            set_cert_status(cluster, &host, ReconcileStatus::Pending).await;
        }
        info!(%host, http01 = zone_id.is_none(), "ordering a certificate");
        let issued = match &zone_id {
            Some(zone_id) => {
                issue_certificate(
                    account,
                    cloudflare,
                    zone_id,
                    std::slice::from_ref(&host),
                    DEFAULT_PROPAGATION_TIMEOUT,
                )
                .await
            }
            None => issue_certificate_http01(account, http01, &host).await,
        };
        match issued {
            Ok(issued) => {
                let request = Request::StoreCertificate(CertificateState {
                    fqdn: host.clone(),
                    chain_pem: issued.chain_pem,
                    key_pem: issued.key_pem,
                    not_after: issued.not_after,
                    issued_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                });
                if let Err(e) = cluster.submit(request).await {
                    warn!(%host, error = %e, "could not store the issued certificate");
                }
            }
            Err(e) => {
                warn!(%host, error = %e, "certificate issuance failed");
                set_cert_status(
                    cluster,
                    &host,
                    ReconcileStatus::Error {
                        message: e.to_string(),
                        at: now(),
                    },
                )
                .await;
            }
        }
    }
}

/// Submits `Request::SetCertStatus`, logging rather than failing the caller
/// if the write doesn't land - status is best effort, never load bearing.
async fn set_cert_status(cluster: &ClusterNode, fqdn: &str, status: ReconcileStatus) {
    let request = Request::SetCertStatus {
        fqdn: fqdn.to_string(),
        status,
    };
    if let Err(e) = cluster.submit(request).await {
        warn!(%fqdn, error = %e, "could not record certificate status");
    }
}

/// Submits `Request::SetDnsStatus`, logging rather than failing the caller
/// if the write doesn't land - status is best effort, never load bearing.
async fn set_dns_status(cluster: &ClusterNode, fqdn: &str, status: ReconcileStatus) {
    let request = Request::SetDnsStatus {
        fqdn: fqdn.to_string(),
        status,
    };
    if let Err(e) = cluster.submit(request).await {
        warn!(%fqdn, error = %e, "could not record dns status");
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Cloudflare zone ids, resolved once at startup. `ZoneResolver` is
/// synchronous, so the lookup cannot happen while a certificate is being
/// ordered.
struct Zones {
    zones: Vec<(String, String)>,
}

impl Zones {
    async fn resolve(cf: &Cloudflare, names: &[String]) -> cl_dns::Result<Self> {
        let mut zones = Vec::new();
        for name in names {
            zones.push((name.clone(), cf.zone_id(name).await?));
        }
        Ok(Self { zones })
    }
}

impl ZoneResolver for Zones {
    /// Longest matching zone wins, so a delegated subdomain zone is preferred
    /// over the parent it sits under.
    fn zone_id_for(&self, fqdn: &str) -> Option<String> {
        self.zones
            .iter()
            .filter(|(name, _)| fqdn == name || fqdn.ends_with(&format!(".{name}")))
            .max_by_key(|(name, _)| name.len())
            .map(|(_, id)| id.clone())
    }
}

/// How often DNS records are reconciled against Cloudflare. Not as tight as
/// the routing table (`RECONCILE_INTERVAL`): a hostname appearing a minute
/// late is a much smaller problem than a route disappearing for a minute.
const DNS_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// Publishes A/AAAA/CNAME records for every hostname the proxy answers (stack
/// routes, static routes, redirects) plus the web ui, on every node: the proxy
/// runs everywhere and forwards internally, so any node's address is a way in.
/// Leader only, since several nodes reconciling at once would race each other's
/// writes.
async fn run_dns_reconcile(
    cluster: Arc<ClusterNode>,
    config_rx: tokio::sync::watch::Receiver<Arc<ClusterConfig>>,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    nudge: Arc<tokio::sync::Notify>,
) {
    let mut ticker = tokio::time::interval(DNS_RECONCILE_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = nudge.notified() => {}
        }
        if !cluster.is_leader() {
            continue;
        }
        let state = cluster.snapshot().await;
        let cfg = config_rx.borrow().clone();
        let key = key_rx.borrow().clone();
        prune_stale_status(&cluster, &cfg, &state, false, true).await;
        let Some((cf, zone_names)) =
            cloudflare_setup(&cfg, &cluster, key.as_deref(), "dns record publishing").await
        else {
            continue;
        };
        let zones = match Zones::resolve(&cf, &zone_names).await {
            Ok(zones) => zones,
            Err(e) => {
                warn!(error = %e, "cannot resolve the cloudflare zones, dns records will not be published");
                continue;
            }
        };
        reconcile_dns(&cfg, &cluster, &cf, &zones, &state).await;
    }
}

/// Every hostname a node's address needs to answer for, mapped to the node names
/// its DNS record(s) should name.
///
/// `DnsPublishMode::AllNodes` (default, Swarm-style mesh): every node's address
/// is published for every hostname, since any node answers any route.
///
/// `DnsPublishMode::OwningNode`: a stack route's hostname only names the node(s)
/// running that service's containers (`placement::service_nodes`), so clients go
/// straight there. Static backends and redirects have no container and keep
/// naming every node.
///
/// The web ui hostname is not here: writes only work on the leader
/// (`redirect_non_leader` in `cl_api::server`), so a follower could only 503.
/// `desired_records` gives it the leader's address alone.
pub fn desired_dns_hosts(
    cfg: &ClusterConfig,
    state: &ClusterState,
) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut hosts: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let all_nodes: BTreeSet<String> = cfg.nodes.keys().cloned().collect();
    let owning_node_only = cfg.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;

    for placement in crate::placement::service_placements(state, cfg) {
        let owners = crate::placement::service_nodes(state, &placement.stack, &placement.service);
        let healthy = healthy_nodes(state, &placement.stack, &placement.service);
        for entry in &placement.extensions.proxy {
            // A managed network names the nodes holding a healthy container,
            // whatever `proxy.dns_publish` says: its node addresses are only
            // reached through the owner (or, with `proxy: true`, any node
            // that relays to it).
            let nodes = if cfg.is_managed_network(&entry.network) {
                healthy.clone()
            } else if owning_node_only {
                owners.clone()
            } else {
                all_nodes.clone()
            };
            hosts
                .entry((entry.host.clone(), entry.network.clone()))
                .or_default()
                .extend(nodes);
        }
        // An expose port is bound on every node (or, on a managed network,
        // reached through the nodes holding a healthy container), so its
        // hostname names those whatever `proxy.dns_publish` says.
        for expose in &placement.extensions.expose {
            let Some(host) = &expose.host else { continue };
            let nodes = if cfg.is_managed_network(&expose.network) {
                healthy.clone()
            } else {
                all_nodes.clone()
            };
            hosts
                .entry((host.clone(), expose.network.clone()))
                .or_default()
                .extend(nodes);
        }
    }
    for entry in StaticRouteStore::load(state).routes {
        let nodes = if cfg.is_managed_network(&entry.network) {
            managed_entry_nodes(cfg, &entry.network)
        } else {
            match entry.node {
                Some(node) => BTreeSet::from([node]),
                None => all_nodes.clone(),
            }
        };
        hosts
            .entry((entry.host, entry.network))
            .or_default()
            .extend(nodes);
    }
    for rule in RedirectStore::load(state).rules {
        let nodes = if cfg.is_managed_network(&rule.network) {
            managed_entry_nodes(cfg, &rule.network)
        } else {
            all_nodes.clone()
        };
        hosts
            .entry((rule.host, rule.network))
            .or_default()
            .extend(nodes);
    }
    // Any node can answer the login portal, except on a managed network,
    // where a client is pointed at a node's own address there: only the nodes
    // that hold one serve it.
    if let Some(auth) = &cfg.proxy.auth {
        for network in &auth.networks {
            let nodes: BTreeSet<String> = if cfg.is_managed_network(network) {
                cfg.managed_node_addresses(network)
                    .into_keys()
                    .map(str::to_string)
                    .collect()
            } else {
                all_nodes.clone()
            };
            hosts
                .entry((auth.host.clone(), network.clone()))
                .or_default()
                .extend(nodes);
        }
    }
    hosts
}

/// The nodes holding a healthy container of `stack`/`service`: wanted
/// running and reported running by the node that owns it. A stopped or failing
/// container names no node, so its hostname gets no record at all.
pub(crate) fn healthy_nodes(state: &ClusterState, stack: &str, service: &str) -> BTreeSet<String> {
    state
        .assignments
        .iter()
        .filter(|(id, a)| {
            id.stack == stack
                && id.service == service
                && a.desired == cl_raft::state::DesiredState::Running
                && state
                    .container_status
                    .get(*id)
                    .is_some_and(|s| s.running && s.node == a.node)
        })
        .map(|(_, a)| a.node.clone())
        .collect()
}

/// The nodes a static route or redirect on managed network `network` names:
/// every node holding an address there when the proxy serves the network
/// (`proxy: true`), none otherwise since nothing would answer on 80/443.
pub(crate) fn managed_entry_nodes(cfg: &ClusterConfig, network: &str) -> BTreeSet<String> {
    if !cfg.managed_proxy(network) {
        return BTreeSet::new();
    }
    cfg.managed_node_addresses(network)
        .into_keys()
        .map(str::to_string)
        .collect()
}

/// TTL of the records published for a managed network: the owner moves with
/// its container, so resolvers must not keep an answer long.
const MANAGED_RECORD_TTL: u32 = 60;

/// `fqdn` -> the owning node's address on managed network `network`, one A
/// record. Empty when the node holds no address there.
fn managed_node_records(
    cfg: &ClusterConfig,
    host: &str,
    network: &str,
    node: &str,
) -> Vec<DesiredRecord> {
    let (Some(net), Some(ip)) = (
        cfg.networks.get(network),
        cfg.node_address_on(node, network),
    ) else {
        return Vec::new();
    };
    vec![DesiredRecord {
        fqdn: cl_config::qualify(host, net.domain.as_deref()),
        kind: cl_dns::RecordType::A,
        content: ip.to_string(),
        ttl: MANAGED_RECORD_TTL,
        proxied: false,
    }]
}

/// Fully qualified hostnames whose DNS records go out with Cloudflare's proxy
/// on: at least one route sets `cloudflare_proxy` and nothing else answering
/// that name (another stack route, a static route, a redirect, the login
/// portal) leaves it off. Keyed by the qualified name, so `app` and
/// `app.example.com` count as the same host. Only public networks qualify.
fn cloudflare_proxied_hosts(cfg: &ClusterConfig, state: &ClusterState) -> BTreeSet<String> {
    let mut on: BTreeSet<String> = BTreeSet::new();
    let mut off: BTreeSet<String> = BTreeSet::new();
    let mut mark = |host: &str, network: &str, proxied: bool| {
        let Some(net) = cfg.networks.get(network) else {
            return;
        };
        let fqdn = cl_config::qualify(host, net.domain.as_deref()).to_ascii_lowercase();
        // A name on a non-public network can never be proxied, and says
        // nothing about the same name published on a public one.
        if net.kind != cl_config::NetworkKind::Public || !proxied {
            off.insert(fqdn);
        } else {
            on.insert(fqdn);
        }
    };
    for placement in crate::placement::service_placements(state, cfg) {
        for entry in &placement.extensions.proxy {
            mark(&entry.host, &entry.network, entry.cloudflare_proxy);
        }
    }
    for entry in StaticRouteStore::load(state).routes {
        mark(&entry.host, &entry.network, entry.cloudflare_proxy);
    }
    for rule in RedirectStore::load(state).rules {
        mark(&rule.host, &rule.network, rule.cloudflare_proxy);
    }
    if let Some(auth) = &cfg.proxy.auth {
        for network in &auth.networks {
            mark(&auth.host, network, auth.cloudflare_proxy);
        }
    }
    on.retain(|fqdn| !off.contains(fqdn));
    on
}

/// `(host, network)` pairs, keyed like `desired_dns_hosts`, where a stack
/// route sets `private_shortcut: false`: the client DNS server then answers
/// every client with the route's own address instead of the node's address on
/// the client's internal or private network.
pub fn private_shortcut_denied_hosts(
    cfg: &ClusterConfig,
    state: &ClusterState,
) -> BTreeSet<(String, String)> {
    let mut denied = BTreeSet::new();
    for placement in crate::placement::service_placements(state, cfg) {
        denied.extend(
            placement
                .extensions
                .proxy
                .into_iter()
                .filter(|entry| !entry.private_shortcut)
                .map(|entry| (entry.host, entry.network)),
        );
        denied.extend(
            placement
                .extensions
                .expose
                .into_iter()
                .filter(|e| !e.private_shortcut)
                .filter_map(|e| Some((e.host?, e.network))),
        );
    }
    denied
}

/// The config node matching a raft leader id, if it is known and still in the
/// config. `None` when leadership is unsettled, in which case the caller
/// publishes nothing for the leader-only web hostname rather than guess.
fn leader_node(
    cfg: &ClusterConfig,
    leader_id: Option<cl_raft::NodeId>,
) -> Option<(&String, &cl_config::Node)> {
    let leader_id = leader_id?;
    cfg.nodes
        .iter()
        .find(|(_, node)| cl_raft::node_id(node.id) == leader_id)
}

/// Whether hostnames on `network` go to the DNS provider. A network switched
/// to `publish_dns: false` is answered only by the client-facing DNS server,
/// and a container network never has a provider to publish to.
fn publishes_dns(cfg: &ClusterConfig, network: &str) -> bool {
    cfg.networks
        .get(network)
        .is_none_or(|n| n.publish_dns && n.kind != cl_config::NetworkKind::Container)
}

/// Resolves `desired_dns_hosts` against every node, deduplicated, plus the
/// web ui hostname against the leader alone. A node with no address on a
/// given hostname's network is skipped rather than failing the whole pass: a
/// private network only some nodes sit on is normal.
fn desired_records(
    cfg: &ClusterConfig,
    leader_id: Option<cl_raft::NodeId>,
    state: &ClusterState,
) -> Vec<DesiredRecord> {
    let mut out: Vec<DesiredRecord> = Vec::new();
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();

    // A managed network's hostname names the node's address there, short
    // lived: the dashboard on the leader, everything else on the nodes holding
    // a healthy container (`desired_dns_hosts`).
    if let Some((leader_name, leader)) =
        leader_node(cfg, leader_id).filter(|_| publishes_dns(cfg, &cfg.web.network))
    {
        let records = if cfg.is_managed_network(&cfg.web.network) {
            managed_node_records(cfg, &cfg.web.address, &cfg.web.network, leader_name)
        } else {
            cl_dns::records_for(cfg, &cfg.web.address, &cfg.web.network, leader).unwrap_or_default()
        };
        push_new(&mut out, &mut seen, records);
    }

    let proxied = cloudflare_proxied_hosts(cfg, state);
    for ((host, network), nodes) in desired_dns_hosts(cfg, state) {
        if !publishes_dns(cfg, &network) {
            continue;
        }
        let managed = cfg.is_managed_network(&network);
        for node_name in &nodes {
            let Some(node) = cfg.nodes.get(node_name) else {
                continue;
            };
            if managed {
                push_new(
                    &mut out,
                    &mut seen,
                    managed_node_records(cfg, &host, &network, node_name),
                );
                continue;
            }
            let Ok(mut records) = cl_dns::records_for(cfg, &host, &network, node) else {
                continue;
            };
            for r in &mut records {
                r.proxied = proxied.contains(&r.fqdn.to_ascii_lowercase());
            }
            push_new(&mut out, &mut seen, records);
        }
    }
    // Same collapse the planner applies: a CNAME a node's address record
    // shadows can never go live, so keeping it here left its hostname
    // "pending" forever even though the A/AAAA records were published.
    cl_dns::normalize_desired(&out)
}

/// Hostnames that should be published but that no node can answer for: every
/// node meant to serve them has no address on the hostname's network. Without
/// this the hostname just never gets a record and nothing says why. Keyed by
/// the qualified name, the value is the message for its DNS status.
fn unaddressed_hosts(cfg: &ClusterConfig, state: &ClusterState) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for ((host, network), nodes) in desired_dns_hosts(cfg, state) {
        if !publishes_dns(cfg, &network) || nodes.is_empty() || cfg.is_managed_network(&network) {
            continue;
        }
        let answered = nodes.iter().any(|name| {
            cfg.nodes
                .get(name)
                .is_some_and(|node| cl_dns::records_for(cfg, &host, &network, node).is_ok())
        });
        if answered {
            continue;
        }
        let Some(net) = cfg.networks.get(&network) else {
            continue;
        };
        let fqdn = cl_config::qualify(&host, net.domain.as_deref());
        out.insert(
            fqdn,
            format!(
                "no node has an address on network {network}, so no record can be published; \
                 add one under nodes.<name>.network.{network}"
            ),
        );
    }
    // A static route or redirect on a managed network whose proxy is off has
    // nothing answering 80/443 and no container to point at.
    let stored = StaticRouteStore::load(state)
        .routes
        .into_iter()
        .map(|r| (r.host, r.network, "static route"))
        .chain(
            RedirectStore::load(state)
                .rules
                .into_iter()
                .map(|r| (r.host, r.network, "redirect")),
        );
    for (host, network, what) in stored {
        if !publishes_dns(cfg, &network)
            || !cfg.is_managed_network(&network)
            || cfg.managed_proxy(&network)
        {
            continue;
        }
        let Some(net) = cfg.networks.get(&network) else {
            continue;
        };
        out.insert(
            cl_config::qualify(&host, net.domain.as_deref()),
            format!(
                "this {what} is on the managed network {network}, which has proxy: false, so no \
                 record is published; set proxy: true on the network or move it to another network"
            ),
        );
    }
    out
}

/// Appends `records` to `out`, skipping any already seen by (type, fqdn,
/// content) - the same hostname/network combination can turn up more than
/// once (a stack route and a static route sharing a host, say).
fn push_new(
    out: &mut Vec<DesiredRecord>,
    seen: &mut BTreeSet<(String, String, String)>,
    records: Vec<DesiredRecord>,
) {
    for record in records {
        let key = (
            format!("{:?}", record.kind),
            record.fqdn.clone(),
            record.content.clone(),
        );
        if seen.insert(key) {
            out.push(record);
        }
    }
}

/// One reconcile pass: computes desired records, groups them by the
/// Cloudflare zone that covers each fqdn, and converges each zone's live
/// records to match. Failures on one zone don't stop the others.
async fn reconcile_dns(
    cfg: &ClusterConfig,
    cluster: &Arc<ClusterNode>,
    cf: &Cloudflare,
    zones: &Zones,
    state: &ClusterState,
) {
    for (fqdn, message) in unaddressed_hosts(cfg, state) {
        warn!(%fqdn, "{message}");
        let same = matches!(
            state.dns_status.get(&fqdn),
            Some(ReconcileStatus::Error { message: m, .. }) if *m == message
        );
        if !same {
            set_dns_status(
                cluster,
                &fqdn,
                ReconcileStatus::Error { message, at: now() },
            )
            .await;
        }
    }

    let mut by_zone: std::collections::HashMap<(String, String), Vec<DesiredRecord>> =
        std::collections::HashMap::new();
    for record in desired_records(cfg, cluster.quorum().leader(), state) {
        let Some(zone_id) = zones.zone_id_for(&record.fqdn) else {
            warn!(fqdn = %record.fqdn, "no cloudflare zone covers this name, not publishing a dns record");
            if !state.dns_records.contains_key(&dns_key(
                &record.fqdn,
                &format!("{:?}", record.kind),
                &record.content,
            )) {
                set_dns_status(
                    cluster,
                    &record.fqdn,
                    ReconcileStatus::Error {
                        message: "no cloudflare zone covers this hostname".to_string(),
                        at: now(),
                    },
                )
                .await;
            }
            continue;
        };
        let zone_name = zones
            .zones
            .iter()
            .find(|(_, id)| id == &zone_id)
            .map(|(name, _)| name.clone())
            .unwrap_or_default();
        by_zone
            .entry((zone_id, zone_name))
            .or_default()
            .push(record);
    }

    // A zone whose desired set became empty (its last host was removed) must
    // still be reconciled, or its A record, ownership TXT and `dns_records`
    // entries would never be cleaned up. `plan` only ever deletes records
    // carrying our ownership tag, so visiting an unrelated zone is harmless.
    for (zone_name, zone_id) in &zones.zones {
        by_zone
            .entry((zone_id.clone(), zone_name.clone()))
            .or_default();
    }

    for ((zone_id, zone_name), desired) in by_zone {
        for record in &desired {
            if !state.dns_records.contains_key(&dns_key(
                &record.fqdn,
                &format!("{:?}", record.kind),
                &record.content,
            )) && state.dns_status.get(&record.fqdn) != Some(&ReconcileStatus::Pending)
            {
                set_dns_status(cluster, &record.fqdn, ReconcileStatus::Pending).await;
            }
        }
        if let Err(e) = reconcile_zone(
            cluster,
            &cfg.cluster.name,
            cf,
            &zone_id,
            &zone_name,
            desired.clone(),
            state,
        )
        .await
        {
            warn!(zone = %zone_name, error = %e, "dns reconcile failed for this zone");
            // Attribute the failure to the specific hostname(s) it names, not
            // every hostname sharing this zone: a collision on one stack's
            // record must not paint every other stack's (unrelated, and
            // possibly fine) hostnames with the same error.
            let affected = e.affected_fqdns();
            let fqdns: Vec<String> = if affected.is_empty() {
                // A zone-wide failure (e.g. listing records failed) really
                // does affect every hostname in this batch.
                desired.iter().map(|r| r.fqdn.clone()).collect()
            } else {
                affected.into_iter().map(str::to_string).collect()
            };
            for fqdn in fqdns {
                set_dns_status(
                    cluster,
                    &fqdn,
                    ReconcileStatus::Error {
                        message: e.to_string(),
                        at: now(),
                    },
                )
                .await;
            }
        }
    }
}

/// Clears cert/dns statuses that no longer describe anything: the hostname is
/// not wanted any more (removed or typo'd in the config), or its cert/record is
/// already live (a stale `pending`). Statuses are otherwise only replaced by a
/// successful store/upsert, so these would linger in `clouduler status`.
async fn prune_stale_status(
    cluster: &Arc<ClusterNode>,
    cfg: &ClusterConfig,
    state: &ClusterState,
    check_certs: bool,
    check_dns: bool,
) {
    let mut stale = BTreeSet::new();
    if check_certs {
        let wanted = tls_hosts(cfg, state);
        for (fqdn, status) in &state.cert_status {
            // A renewal failure is recorded against a certificate that
            // exists, and stays until a renewal succeeds (which clears it
            // in `StoreCertificate`). Only a pending status is stale once
            // the certificate is there.
            let outlived = state.certificates.contains_key(fqdn)
                && !matches!(status, cl_raft::ReconcileStatus::Error { .. });
            if !wanted.contains_key(fqdn) || outlived {
                stale.insert(fqdn.clone());
            }
        }
    }
    if check_dns {
        let desired = desired_records(cfg, cluster.quorum().leader(), state);
        let unaddressed = unaddressed_hosts(cfg, state);
        for fqdn in state.dns_status.keys() {
            // No records at all would read as "all live" below.
            if unaddressed.contains_key(fqdn) {
                continue;
            }
            let mine: Vec<_> = desired.iter().filter(|r| &r.fqdn == fqdn).collect();
            let all_live = mine.iter().all(|r| {
                state.dns_records.contains_key(&dns_key(
                    &r.fqdn,
                    &format!("{:?}", r.kind),
                    &r.content,
                ))
            });
            if all_live {
                stale.insert(fqdn.clone());
            }
        }
    }
    for fqdn in stale {
        if let Err(e) = cluster
            .submit(Request::ClearReconcileStatus { fqdn: fqdn.clone() })
            .await
        {
            warn!(%fqdn, error = %e, "could not clear a stale reconcile status");
        }
    }
}

async fn reconcile_zone(
    cluster: &Arc<ClusterNode>,
    cluster_name: &str,
    cf: &Cloudflare,
    zone_id: &str,
    zone_name: &str,
    desired: Vec<DesiredRecord>,
    state: &ClusterState,
) -> cl_dns::Result<()> {
    let mut live = cf.list_records(zone_id, None, None).await?;
    let known_ids: std::collections::HashSet<String> = state
        .dns_records
        .values()
        .filter(|r| r.zone == zone_name)
        .filter_map(|r| r.provider_id.clone())
        .collect();
    let plan = cl_dns::plan_with_known(cluster_name, &desired, &live, &known_ids);
    let apply_result = if plan.is_empty() {
        Ok(())
    } else {
        let result = cl_dns::apply(cf, zone_id, &plan).await;
        // Re-list regardless of the outcome: `apply` keeps going past a
        // failing record (see its own doc comment), so whatever did succeed
        // must still be synced into replicated state below rather than
        // waiting for the next pass.
        live = cf.list_records(zone_id, None, None).await?;
        result
    };

    let mut want_keys = BTreeSet::new();
    for record in &desired {
        let record_type = format!("{:?}", record.kind);
        let provider_id = live
            .iter()
            .find(|r| {
                r.name.eq_ignore_ascii_case(&record.fqdn)
                    && format!("{:?}", r.kind) == record_type
                    && r.content == record.content
            })
            .and_then(|r| r.id.clone());
        // No live match after a (possibly partial) apply means this record's
        // create/update actually failed - recording it as published anyway
        // would both lie to the ui and clear the error status `reconcile_dns`
        // is about to set for it. Leave it out; the next pass retries it.
        let Some(provider_id) = provider_id else {
            continue;
        };
        let entry = DnsRecordState {
            fqdn: record.fqdn.clone(),
            record_type,
            content: record.content.clone(),
            zone: zone_name.to_string(),
            provider_id: Some(provider_id),
        };
        let key = dns_key(&entry.fqdn, &entry.record_type, &entry.content);
        want_keys.insert(key.clone());
        if state.dns_records.get(&key) != Some(&entry) {
            if let Err(e) = cluster.submit(Request::UpsertDnsRecord(entry)).await {
                warn!(fqdn = %record.fqdn, error = %e, "could not record the published dns entry");
            }
        }
    }

    for (key, existing) in &state.dns_records {
        if existing.zone == zone_name && !want_keys.contains(key) {
            if let Err(e) = cluster
                .submit(Request::RemoveDnsRecord { key: key.clone() })
                .await
            {
                warn!(%key, error = %e, "could not remove the stale dns entry");
            }
        }
    }

    apply_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::fixtures::{state_with, SPEC};

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml"))
            .expect("example parses")
    }

    /// The v6 listener is IPv6-only, so it shares a port with the v4 one
    /// instead of colliding with it. Skipped quietly without IPv6.
    #[test]
    fn the_v6_listener_shares_its_port_with_the_v4_one() {
        let v4 = bind_v4(0).expect("v4 binds");
        let port = v4.local_addr().unwrap().port();
        let Ok(v6) = bind_v6(port) else { return };
        assert!(v6.local_addr().unwrap().is_ipv6());
        assert_eq!(v6.local_addr().unwrap().port(), port);
    }

    /// `desired_of` is what the supervisor diffs on every pass to decide
    /// which task groups to restart; these are the changes that must (and
    /// must not) trip that diff.
    mod desired {
        use super::*;

        #[test]
        fn disabling_the_proxy_changes_the_listen_key() {
            let mut disabled = cfg();
            disabled.proxy.enabled = false;
            assert_ne!(
                desired_of(&cfg(), "").listen,
                desired_of(&disabled, "").listen
            );
            assert!(desired_of(&disabled, "").listen.is_none());
        }

        #[test]
        fn changing_a_listen_port_changes_the_listen_key() {
            let mut edited = cfg();
            edited.proxy.https_port += 1;
            assert_ne!(
                desired_of(&cfg(), "").listen,
                desired_of(&edited, "").listen
            );
        }

        #[test]
        fn an_unrelated_edit_changes_neither_key() {
            let mut edited = cfg();
            edited.cluster.name = "renamed".to_string();
            assert_eq!(desired_of(&cfg(), ""), desired_of(&edited, ""));
        }

        #[test]
        fn adding_a_cloudflare_section_flips_the_dns_key_and_offers_acme() {
            let mut without = cfg();
            without.cloudflare = None;
            let before = desired_of(&without, "");
            assert!(!before.dns);

            let after = desired_of(&cfg(), "");
            assert!(cfg().cloudflare.is_some());
            assert!(after.dns);
            assert_ne!(before.dns, after.dns);
        }

        #[test]
        fn acme_contact_email_changes_the_acme_key() {
            let mut edited = cfg();
            edited.proxy.acme.enabled = true;
            edited.proxy.acme.contact_email = Some("ops@example.com".to_string());
            let base = desired_of(&edited, "");

            edited.proxy.acme.contact_email = Some("someone-else@example.com".to_string());
            let changed = desired_of(&edited, "");
            assert_ne!(base.acme, changed.acme);
        }
    }

    /// The routes a stack published, without the dashboard the daemon adds
    /// alongside them.
    fn service_routes(cfg: &ClusterConfig, state: &ClusterState) -> Vec<Route> {
        routes_for(cfg, state)
            .into_iter()
            .filter(|r| r.instance != "clouduler.dashboard")
            .collect()
    }

    #[test]
    fn a_route_carries_every_replica_as_a_backend() {
        // `cfg()` (a real example cluster.yaml) has no network named
        // "default" - SPEC's stack network - so it can never resolve a
        // backend address for it. `placement::fixtures::cluster_config`
        // declares "default" as the internal network SPEC actually needs.
        let routes = service_routes(
            &crate::placement::fixtures::cluster_config(),
            &state_with(SPEC, 2),
        );
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].backends().len(), 2);
        assert_eq!(routes[0].backends()[0].port(), 80);
    }

    /// A bare host is qualified with the domain of the network it is
    /// published on, which is how `blog` becomes a name a browser can reach.
    #[test]
    fn a_bare_host_gets_the_network_domain() {
        let routes = service_routes(&cfg(), &state_with(SPEC, 1));
        assert_eq!(routes[0].host, "blog.example.com");
    }

    /// The table must still name the host while nothing is running, or a
    /// redeploy turns a 502 into a 404 and clients cache the wrong thing.
    #[test]
    fn a_route_with_no_backends_is_still_published() {
        let mut state = state_with(SPEC, 1);
        state.assignments.clear();
        let routes = service_routes(&cfg(), &state);
        assert_eq!(routes.len(), 1);
        assert!(routes[0].backends().is_empty());
    }

    /// The point of the dashboard route: it is in the same table as everything
    /// else, so the certificate and DNS machinery picks it up for free.
    #[test]
    fn the_dashboard_is_published_as_a_route_on_its_own_hostname() {
        let cfg = cfg();
        let routes = routes_for(&cfg, &state_with(SPEC, 1));
        let dashboard = routes
            .iter()
            .find(|r| r.instance == "clouduler.dashboard")
            .expect("the dashboard is routed");
        assert_eq!(dashboard.host, cfg.web_fqdn());
        assert!(dashboard.tls);
    }

    /// Loopback only, and pointed at the api's plain listener rather than the
    /// TLS one: the proxy speaks http to its backends and the hop must not
    /// leave the machine.
    #[test]
    fn the_dashboard_backend_is_this_nodes_own_api_over_loopback() {
        let cfg = cfg();
        let route = dashboard_route(&cfg, &state_with(SPEC, 1)).expect("routed");
        let backend = route.backends()[0];
        assert!(backend.ip().is_loopback(), "{backend}");
        assert_eq!(backend.port(), cfg.web.backend_port);
        assert_ne!(
            backend.port(),
            cfg.web.port,
            "must not point at the tls listener"
        );
    }

    /// The api derives its OIDC redirect from the request, and the event
    /// streams must not be capped or timed out.
    #[test]
    fn the_dashboard_route_preserves_host_and_allows_streams() {
        let route = dashboard_route(&cfg(), &state_with(SPEC, 1)).expect("routed");
        assert!(route.options.preserve_host);
        assert!(route.options.websockets);
        assert_eq!(route.options.max_body_size, None);
        assert_eq!(route.options.response_timeout, None);
    }

    /// `/mcp` is served through the dashboard route. An MCP client running in
    /// a stack on the node that hosts the dashboard (the leader) has its
    /// request delivered locally, so the proxy sees its raw overlay address,
    /// outside the web network's declared subnet. The dashboard route must
    /// trust that peer exactly like a service route does.
    #[test]
    fn the_dashboard_route_trusts_a_same_node_overlay_peer() {
        const LAN_SPEC: &str = "\
networks:
  lan: {}
services:
  web:
    image: nginx
    networks: [lan]
";
        let mut cfg = cfg();
        cfg.web.network = "lan".to_string();
        let mut state = state_with(LAN_SPEC, 1);
        let mut allocations =
            Allocations::decode(state.overlay.as_deref()).expect("state_with sets an overlay");
        let overlay_subnet = allocations
            .allocate_network("blog_lan", &[])
            .expect("allocates a subnet for the stack's lan overlay");
        state.overlay = Some(serde_json::to_string(&allocations).expect("encodes"));

        let route = dashboard_route(&cfg, &state).expect("routed");
        let container_peer = std::net::IpAddr::from(overlay_subnet.hosts().next().unwrap());

        assert!(route
            .options
            .allowed_subnets
            .iter()
            .all(|s| !s.contains(&container_peer)));
        assert!(route
            .options
            .trusted_overlay_subnets
            .iter()
            .any(|s| s.contains(&container_peer)));
    }

    #[test]
    fn turning_off_through_proxy_removes_the_dashboard_route() {
        let mut cfg = cfg();
        cfg.web.through_proxy = false;
        assert!(dashboard_route(&cfg, &state_with(SPEC, 1)).is_none());
        assert!(routes_for(&cfg, &state_with(SPEC, 1))
            .iter()
            .all(|r| r.instance != "clouduler.dashboard"));
    }

    /// A disabled proxy has no listeners, so a route pointing at the dashboard
    /// would be a promise nothing keeps.
    #[test]
    fn a_disabled_proxy_publishes_no_dashboard_route() {
        let mut cfg = cfg();
        cfg.proxy.enabled = false;
        assert!(dashboard_route(&cfg, &state_with(SPEC, 1)).is_none());
    }

    /// The dashboard's certificate used to be a special case bolted onto
    /// tls_hosts. It should now arrive through the route like any other name.
    #[test]
    fn the_dashboard_hostname_still_gets_a_certificate() {
        let cfg = cfg();
        let hosts = tls_hosts(&cfg, &state_with(SPEC, 1));
        assert!(hosts.contains_key(&cfg.web_fqdn()));
    }

    #[test]
    fn tls_hosts_include_the_web_ui() {
        let cfg = cfg();
        let hosts = tls_hosts(&cfg, &state_with(SPEC, 1));
        assert!(hosts.contains_key(&cfg.web_fqdn()));
        assert!(hosts.contains_key("blog.example.com"));
    }

    /// The mechanism the whole HTTP-01 fallback rests on: a stack author
    /// writing a route's host with a trailing dot is the opt-in signal that
    /// this fqdn may not belong to any configured Cloudflare zone.
    #[test]
    fn a_trailing_dot_host_is_marked_for_the_http01_fallback() {
        const SPEC_WITH_DOT: &str = "\
networks:
  default: {}
services:
  web:
    image: nginx
    networks: [default]
    x-clouduler:
      proxy:
        - host: app.elsewhere.net.
          network: wan
          port: 80
";
        let hosts = tls_hosts(&cfg(), &state_with(SPEC_WITH_DOT, 1));
        assert_eq!(hosts.get("app.elsewhere.net"), Some(&true));
    }

    #[test]
    fn a_plain_dotted_host_is_not_marked_for_the_http01_fallback() {
        let hosts = tls_hosts(&cfg(), &state_with(SPEC, 1));
        assert_eq!(hosts.get("blog.example.com"), Some(&false));
    }

    /// A route on a private network only accepts clients on that network's
    /// subnet; a route on the public `wan` network gets no such restriction.
    #[test]
    fn a_route_on_a_private_network_is_gated_to_its_subnet() {
        const LAN_SPEC: &str = "\
networks:
  default: {}
services:
  web:
    image: nginx
    networks: [default]
    x-clouduler:
      proxy:
        - host: files
          network: lan
          port: 80
";
        let routes = service_routes(&cfg(), &state_with(LAN_SPEC, 1));
        let subnet = routes[0].options.allowed_subnets[0];
        assert_eq!(routes[0].options.allowed_subnets.len(), 1);
        assert!(subnet.contains(&std::net::IpAddr::from([192, 168, 1, 50])));
        assert!(!subnet.contains(&std::net::IpAddr::from([8, 8, 8, 8])));
    }

    /// A same-node client whose request to a private-network route never
    /// actually leaves the node lands on the proxy with its raw overlay
    /// address as the peer, not one inside the private network's declared
    /// subnet - see `RouteOptions::trusted_overlay_subnets`. The route must
    /// still trust that peer, because the stack it belongs to (`blog`) is
    /// itself published on the same `lan` network.
    #[test]
    fn a_route_on_a_private_network_also_trusts_a_same_node_overlay_peer() {
        const LAN_SPEC: &str = "\
networks:
  lan: {}
services:
  web:
    image: nginx
    networks: [lan]
    x-clouduler:
      proxy:
        - host: files
          network: lan
          port: 80
";
        let mut state = state_with(LAN_SPEC, 1);
        let mut allocations =
            Allocations::decode(state.overlay.as_deref()).expect("state_with sets an overlay");
        let overlay_subnet = allocations
            .allocate_network("blog_lan", &[])
            .expect("allocates a subnet for the stack's own lan overlay");
        state.overlay = Some(serde_json::to_string(&allocations).expect("encodes"));

        let routes = service_routes(&cfg(), &state);
        let route = &routes[0];
        let container_peer = std::net::IpAddr::from(overlay_subnet.hosts().next().unwrap());

        assert!(route
            .options
            .allowed_subnets
            .iter()
            .all(|s| !s.contains(&container_peer)));
        assert!(route
            .options
            .trusted_overlay_subnets
            .iter()
            .any(|s| s.contains(&container_peer)));
    }

    /// Overlay names are `<stack>_<network>` and both halves may contain `_`,
    /// so stack `x` on network `a_lan` ("x_a_lan") also ends in `_lan`. Its
    /// subnet must not be trusted for gated routes on `lan`.
    #[test]
    fn trusted_overlays_match_the_exact_network_not_a_name_suffix() {
        let spec_on = |network: &str| format!("networks:\n  {network}: {{}}\nservices: {{}}\n");
        let mut cfg = cfg();
        let lan = cfg.networks.get("lan").expect("example has lan").clone();
        cfg.networks.insert("a_lan".to_string(), lan);

        let mut state = state_with(&spec_on("lan"), 0);
        let mut y = state.stacks.remove("blog").unwrap();
        y.name = "y".to_string();
        let mut x = y.clone();
        x.name = "x".to_string();
        x.spec = spec_on("a_lan");
        state.stacks.insert("y".to_string(), y);
        state.stacks.insert("x".to_string(), x);

        let mut allocations =
            Allocations::decode(state.overlay.as_deref()).expect("state_with sets an overlay");
        let y_subnet = allocations.allocate_network("y_lan", &[]).unwrap();
        let x_subnet = allocations.allocate_network("x_a_lan", &[]).unwrap();
        state.overlay = Some(serde_json::to_string(&allocations).expect("encodes"));

        let on_lan = trusted_overlay_subnets(&cfg, &state, "lan");
        assert!(on_lan.contains(&IpNet::V4(y_subnet)));
        assert!(!on_lan.contains(&IpNet::V4(x_subnet)));

        let on_a_lan = trusted_overlay_subnets(&cfg, &state, "a_lan");
        assert!(on_a_lan.contains(&IpNet::V4(x_subnet)));
        assert!(!on_a_lan.contains(&IpNet::V4(y_subnet)));
    }

    /// A nat-attached client's traffic is masqueraded out whatever the host's
    /// routing table picks; when its destination happens to be a route on
    /// this same node, the packet is delivered locally before it ever
    /// reaches `POSTROUTING`, so the proxy sees the container's raw address
    /// on the nat network's own subnet, unmasqueraded (see
    /// `cl_net::egress::nat_subnet`). A gated route must trust that subnet
    /// even though it has nothing to do with the route's own network.
    #[test]
    fn a_route_on_a_private_network_also_trusts_a_nat_network_peer() {
        const LAN_SPEC: &str = "\
networks:
  default: {}
services:
  web:
    image: nginx
    networks: [default]
    x-clouduler:
      proxy:
        - host: files
          network: lan
          port: 80
";
        let mut cfg = cfg();
        cfg.networks.insert(
            "home".to_string(),
            cl_config::Network {
                kind: NetworkKind::Nat,
                subnet: None,
                domain: None,
                publish_dns: true,
                access: None,
                managed: false,
                open_ports: Vec::new(),
                proxy: false,
                shortcut: Default::default(),
                container: None,
            },
        );

        let routes = service_routes(&cfg, &state_with(LAN_SPEC, 1));
        let route = &routes[0];
        let nat_peer = std::net::IpAddr::from(nat_subnet("home").hosts().next().unwrap());

        assert!(route
            .options
            .allowed_subnets
            .iter()
            .all(|s| !s.contains(&nat_peer)));
        assert!(route
            .options
            .trusted_overlay_subnets
            .iter()
            .any(|s| s.contains(&nat_peer)));
    }

    #[test]
    fn a_route_on_the_public_network_is_not_gated() {
        let routes = service_routes(&cfg(), &state_with(SPEC, 1));
        assert!(routes[0].options.allowed_subnets.is_empty());
    }

    #[test]
    fn redirect_rules_become_redirect_routes() {
        let mut state = state_with(SPEC, 0);
        let store = cl_api::redirects::RedirectStore {
            rules: vec![cl_api::redirects::RedirectRule {
                id: "r1".into(),
                host: "old".into(),
                network: "wan".into(),
                path_prefix: None,
                target: "https://new.example.com".into(),
                preserve_path: true,
                status: 301,
                cloudflare_proxy: false,
            }],
        };
        state.configs.insert(
            cl_api::redirects::REDIRECTS_CONFIG.to_string(),
            cl_raft::state::ConfigEntry {
                name: cl_api::redirects::REDIRECTS_CONFIG.to_string(),
                content: store.encode(),
                revision: 1,
                owner: None,
            },
        );

        let cfg = cfg();
        let route = redirect_routes_for(&cfg, &state)
            .into_iter()
            .find(|r| r.host == "old.example.com")
            .expect("redirect route published");
        let redirect = route.redirect.expect("carries a redirect target");
        assert_eq!(redirect.url, "https://new.example.com");
        assert!(redirect.preserve_path);
        assert_eq!(redirect.status, 301);
        assert!(route.tls);
    }

    fn static_route(
        host: &str,
        network: &str,
        backend: &str,
    ) -> cl_api::static_routes::StaticRouteRule {
        cl_api::static_routes::StaticRouteRule {
            id: "s1".into(),
            host: host.into(),
            network: network.into(),
            node: None,
            backend: backend.into(),
            path_prefix: None,
            tls: true,
            middlewares: Vec::new(),
            max_body_size: None,
            websockets: true,
            preserve_host: false,
            response_timeout: None,
            cloudflare_proxy: false,
        }
    }

    fn with_static_routes(
        mut state: ClusterState,
        routes: Vec<cl_api::static_routes::StaticRouteRule>,
    ) -> ClusterState {
        let store = cl_api::static_routes::StaticRouteStore { routes };
        state.configs.insert(
            cl_api::static_routes::STATIC_ROUTES_CONFIG.to_string(),
            cl_raft::state::ConfigEntry {
                name: cl_api::static_routes::STATIC_ROUTES_CONFIG.to_string(),
                content: store.encode(),
                revision: 1,
                owner: None,
            },
        );
        state
    }

    #[tokio::test]
    async fn a_static_route_resolves_its_backend_and_is_gated() {
        let cfg = cfg();
        let state = with_static_routes(
            state_with(SPEC, 0),
            vec![static_route("files", "lan", "127.0.0.1:9999")],
        );

        let routes = static_backend_routes(&cfg, &state).await;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].host, "files.lan.example.com");
        assert_eq!(
            routes[0].backends(),
            &[SocketAddr::from(([127, 0, 0, 1], 9999))]
        );
        assert!(!routes[0].options.allowed_subnets.is_empty());
    }

    #[test]
    fn a_static_routes_hostname_still_gets_a_certificate_without_dns() {
        let cfg = cfg();
        let state = with_static_routes(
            state_with(SPEC, 1),
            vec![static_route(
                "files",
                "wan",
                "does-not-resolve.invalid:9999",
            )],
        );
        let hosts = tls_hosts(&cfg, &state);
        assert!(hosts.contains_key("files.example.com"));
    }

    #[test]
    fn desired_dns_hosts_includes_published_routes_but_not_the_dashboard() {
        let hosts = desired_dns_hosts(&cfg(), &state_with(SPEC, 1));
        assert!(hosts.contains_key(&("blog".to_string(), "wan".to_string())));
        // The dashboard hostname is leader-only and handled separately by
        // `desired_records`, not published for every node here.
        assert!(!hosts.contains_key(&("cloud".to_string(), "tailscale".to_string())));
    }

    /// An expose block with a `host` is published on every node and, with
    /// `private_shortcut: false`, denied the client DNS shortcut.
    #[test]
    fn expose_hosts_are_published_on_every_node() {
        const EXPOSE_SPEC: &str = "\
networks:
  default: {}
services:
  dns:
    image: x
    networks: [default]
    x-clouduler:
      expose:
        - { port: 53, published: 53, network: lan, host: ns, private_shortcut: false }
        - { port: 80, published: 8080, network: lan }
";
        let cfg = cfg();
        let state = state_with(EXPOSE_SPEC, 1);
        let hosts = desired_dns_hosts(&cfg, &state);
        let key = ("ns".to_string(), "lan".to_string());
        assert_eq!(
            hosts[&key],
            cfg.nodes.keys().cloned().collect::<BTreeSet<_>>()
        );
        // The expose block without a host publishes nothing.
        assert!(!hosts.keys().any(|(h, _)| h.is_empty()));
        assert!(private_shortcut_denied_hosts(&cfg, &state).contains(&key));
    }

    #[test]
    fn desired_dns_hosts_includes_static_routes_and_redirects() {
        let mut state = with_static_routes(
            state_with(SPEC, 0),
            vec![static_route("printer", "lan", "192.168.1.50:9100")],
        );
        let store = cl_api::redirects::RedirectStore {
            rules: vec![cl_api::redirects::RedirectRule {
                id: "r1".into(),
                host: "old".into(),
                network: "wan".into(),
                path_prefix: None,
                target: "https://new.example.com".into(),
                preserve_path: true,
                status: 301,
                cloudflare_proxy: false,
            }],
        };
        state.configs.insert(
            cl_api::redirects::REDIRECTS_CONFIG.to_string(),
            cl_raft::state::ConfigEntry {
                name: cl_api::redirects::REDIRECTS_CONFIG.to_string(),
                content: store.encode(),
                revision: 1,
                owner: None,
            },
        );

        let hosts = desired_dns_hosts(&cfg(), &state);
        assert!(hosts.contains_key(&("printer".to_string(), "lan".to_string())));
        assert!(hosts.contains_key(&("old".to_string(), "wan".to_string())));
    }

    #[test]
    fn a_pinned_static_route_publishes_only_its_node() {
        let cfg = cfg();
        let node = cfg.nodes.keys().next().unwrap().clone();
        let mut route = static_route("printer", "lan", "192.168.1.50:9100");
        route.node = Some(node.clone());
        let state = with_static_routes(state_with(SPEC, 0), vec![route]);

        let hosts = desired_dns_hosts(&cfg, &state);
        let nodes = &hosts[&("printer".to_string(), "lan".to_string())];
        assert_eq!(nodes, &BTreeSet::from([node]));
    }

    /// `blog` is a proxy route on `wan`, forwarded internally by every node,
    /// so every node's address is published for it - the swarm-style default.
    #[test]
    fn desired_records_publish_every_nodes_address_for_proxied_routes() {
        let records = desired_records(&cfg(), None, &state_with(SPEC, 1));
        let blog: Vec<_> = records
            .iter()
            .filter(|r| r.fqdn == "blog.example.com")
            .collect();
        // n2 only offers a cname, which its sibling's address records shadow
        // and so never reaches the desired set.
        assert_eq!(blog.len(), 2, "{blog:?}");
        assert!(blog.iter().all(|r| r.kind != cl_dns::RecordType::CNAME));
    }

    /// `cloudflare_proxy` turns the orange cloud on only when every route
    /// behind the host sets it.
    #[test]
    fn cloudflare_proxy_needs_every_route_behind_the_host() {
        let all = SPEC.replace("port: 80", "port: 80\n          cloudflare_proxy: true");
        let on = desired_records(&cfg(), None, &state_with(&all, 1));
        let blog: Vec<_> = on.iter().filter(|r| r.fqdn == "blog.example.com").collect();
        assert!(
            !blog.is_empty() && blog.iter().all(|r| r.proxied),
            "{blog:?}"
        );

        let off = desired_records(&cfg(), None, &state_with(SPEC, 1));
        assert!(off.iter().all(|r| !r.proxied));

        let state = with_static_routes(
            state_with(&all, 1),
            vec![static_route("blog", "wan", "192.168.1.50:9100")],
        );
        let mixed = desired_records(&cfg(), None, &state);
        assert!(mixed
            .iter()
            .filter(|r| r.fqdn == "blog.example.com")
            .all(|r| !r.proxied));
    }

    /// Static routes and redirects join the same all-or-nothing rule as stack
    /// routes, and a bare label matches its qualified spelling.
    #[test]
    fn cloudflare_proxy_covers_static_routes_and_redirects() {
        let mut route = static_route("ext", "wan", "192.168.1.50:9100");
        route.cloudflare_proxy = true;
        let state = with_static_routes(state_with(SPEC, 0), vec![route]);
        let on = desired_records(&cfg(), None, &state);
        let ext: Vec<_> = on.iter().filter(|r| r.fqdn == "ext.example.com").collect();
        assert!(!ext.is_empty() && ext.iter().all(|r| r.proxied), "{ext:?}");

        // The same name spelled qualified by a route that leaves it off wins.
        let mut other = static_route("ext.example.com", "wan", "192.168.1.51:9100");
        other.cloudflare_proxy = false;
        let mut route = static_route("ext", "wan", "192.168.1.50:9100");
        route.cloudflare_proxy = true;
        let state = with_static_routes(state_with(SPEC, 0), vec![route, other]);
        let mixed = desired_records(&cfg(), None, &state);
        assert!(mixed
            .iter()
            .filter(|r| r.fqdn == "ext.example.com")
            .all(|r| !r.proxied));
    }

    fn managed_published() -> (ClusterConfig, ClusterState) {
        let (cfg, mut state) = crate::placement::fixtures::managed_cluster();
        state.stacks.get_mut("media").unwrap().spec = "networks:\n  mgd: {}\nservices:\n  solo:\n    image: nginx\n    networks: [mgd]\n    x-clouduler:\n      proxy:\n        - host: files.example.com\n          network: mgd\n          port: 80\n".into();
        (cfg, state)
    }

    fn mark_running(state: &mut ClusterState, id: &cl_raft::InstanceId, node: &str) {
        state.container_status.insert(
            id.clone(),
            cl_raft::state::ContainerStatus {
                node: node.to_string(),
                running: true,
                secret_revisions: Default::default(),
                config_revisions: Default::default(),
            },
        );
    }

    #[test]
    fn a_managed_network_hostname_publishes_the_owning_node_address() {
        let (mut cfg, mut state) = managed_published();
        let solo = cl_raft::InstanceId::new("media", "solo", 0);
        let published = |cfg: &ClusterConfig, state: &ClusterState| -> Vec<DesiredRecord> {
            desired_records(cfg, None, state)
                .into_iter()
                .filter(|r| r.fqdn == "files.example.com")
                .collect()
        };

        // Wanted running but not reported running yet: nothing is published.
        assert!(published(&cfg, &state).is_empty());

        mark_running(&mut state, &solo, "n1");
        let mine = published(&cfg, &state);
        assert_eq!(mine.len(), 1, "{mine:?}");
        assert_eq!(mine[0].kind, cl_dns::RecordType::A);
        assert_eq!(
            mine[0].content, "192.168.32.2",
            "the node address, never the container's"
        );
        assert!(
            mine[0].ttl <= 60 && mine[0].ttl > 1,
            "short ttl: {}",
            mine[0].ttl
        );
        assert!(unaddressed_hosts(&cfg, &state).is_empty());

        // The global dns_publish toggle does not apply to a managed network.
        cfg.proxy.dns_publish = cl_config::DnsPublishMode::AllNodes;
        assert_eq!(published(&cfg, &state).len(), 1);

        // The record goes once nothing runs it.
        state.assignments.get_mut(&solo).unwrap().desired = cl_raft::state::DesiredState::Stopped;
        assert!(published(&cfg, &state).is_empty());
        assert!(unaddressed_hosts(&cfg, &state).is_empty());
    }

    /// A node's auto-allocated address on a managed network is what is
    /// published when the config leaves it unset.
    #[test]
    fn an_auto_allocated_node_address_is_published() {
        let (mut cfg, mut state) = managed_published();
        cfg.nodes
            .get_mut("n1")
            .unwrap()
            .network
            .insert("mgd".into(), cl_config::NodeAddress::None);
        let solo = cl_raft::InstanceId::new("media", "solo", 0);
        mark_running(&mut state, &solo, "n1");
        let auto = cfg.node_address_on("n1", "mgd").unwrap().to_string();
        let records = desired_records(&cfg, None, &state);
        let mine: Vec<_> = records
            .iter()
            .filter(|r| r.fqdn == "files.example.com")
            .collect();
        assert_eq!(mine.len(), 1, "{records:?}");
        assert_eq!(mine[0].content, auto);
    }

    /// Without `proxy: true` a managed network's routes are not in the proxy's
    /// table; with it they are.
    #[test]
    fn the_proxy_serves_a_managed_network_only_when_asked() {
        let (mut cfg, state) = managed_published();
        let has = |cfg: &ClusterConfig| {
            routes_for(cfg, &state)
                .iter()
                .any(|r| r.host == "files.example.com")
        };
        assert!(!has(&cfg));
        cfg.networks.get_mut("mgd").unwrap().proxy = true;
        assert!(has(&cfg));
    }

    /// The login portal on a managed network is served only by the nodes that
    /// hold an address there, and published as theirs.
    #[test]
    fn the_login_portal_on_a_managed_network_names_only_addressed_nodes() {
        let (mut cfg, state) = managed_published();
        cfg.networks.get_mut("mgd").unwrap().domain = Some("m.example.com".into());
        cfg.proxy.auth = Some(
            serde_yaml::from_str(
                "{ host: login, networks: [default, mgd], issuer: 'https://id.example.com', client_id: c }",
            )
            .unwrap(),
        );
        let mut n2 = cfg.nodes["n1"].clone();
        n2.id = uuid::Uuid::new_v4();
        n2.network.remove("mgd");
        n2.network.insert(
            "default".into(),
            cl_config::NodeAddress::Ip("10.10.0.2".parse().unwrap()),
        );
        cfg.nodes.insert("n2".into(), n2);

        let hosts = desired_dns_hosts(&cfg, &state);
        assert_eq!(
            hosts[&("login".to_string(), "mgd".to_string())],
            BTreeSet::from(["n1".to_string()])
        );
        assert_eq!(
            hosts[&("login".to_string(), "default".to_string())].len(),
            cfg.nodes.len(),
            "an ordinary network keeps every node"
        );

        let records = desired_records(&cfg, None, &state);
        let login: Vec<_> = records
            .iter()
            .filter(|r| r.fqdn == "login.m.example.com")
            .collect();
        assert_eq!(login.len(), 1, "{records:?}");
        assert_eq!(login[0].content, "192.168.32.2");
        assert!(!unaddressed_hosts(&cfg, &state).contains_key("login.m.example.com"));
    }

    #[test]
    fn a_static_route_on_a_managed_network_needs_the_proxy() {
        let (mut cfg, state) = managed_published();
        let state = with_static_routes(
            state,
            vec![static_route(
                "printer.example.com",
                "mgd",
                "192.168.32.50:80",
            )],
        );
        let printer = |cfg: &ClusterConfig| -> Vec<DesiredRecord> {
            desired_records(cfg, None, &state)
                .into_iter()
                .filter(|r| r.fqdn == "printer.example.com")
                .collect()
        };
        assert!(printer(&cfg).is_empty());
        let warned = unaddressed_hosts(&cfg, &state);
        assert!(
            warned["printer.example.com"].contains("managed network mgd"),
            "{warned:?}"
        );
        cfg.networks.get_mut("mgd").unwrap().proxy = true;
        let records = printer(&cfg);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].content, "192.168.32.2");
        assert!(!unaddressed_hosts(&cfg, &state).contains_key("printer.example.com"));
    }

    /// A network switched to `publish_dns: false` keeps its hostnames out of
    /// the provider, whatever else is published.
    #[test]
    fn a_network_that_does_not_publish_dns_sends_nothing_to_the_provider() {
        let state = with_static_routes(
            state_with(SPEC, 1),
            vec![static_route("printer", "lan", "192.168.1.50:9100")],
        );
        let mut cfg = cfg();
        let on = desired_records(&cfg, None, &state);
        assert!(on.iter().any(|r| r.fqdn.starts_with("printer.")));

        cfg.networks.get_mut("lan").unwrap().publish_dns = false;
        let off = desired_records(&cfg, None, &state);
        assert!(!off.iter().any(|r| r.fqdn.starts_with("printer.")));
        assert!(off.iter().any(|r| r.fqdn == "blog.example.com"));
    }

    /// A hostname whose network no node has an address on never gets a record;
    /// it has to be reported rather than skipped silently.
    #[test]
    fn a_host_no_node_can_answer_for_is_reported() {
        let state = state_with(SPEC, 1);
        let mut cfg = cfg();
        assert!(unaddressed_hosts(&cfg, &state).is_empty());

        let network = cfg.networks.keys().next().cloned().unwrap();
        let blog_network = desired_dns_hosts(&cfg, &state)
            .into_keys()
            .find(|(h, _)| h == "blog")
            .map(|(_, n)| n)
            .unwrap_or(network);
        for node in cfg.nodes.values_mut() {
            node.network.remove(&blog_network);
        }
        let missing = unaddressed_hosts(&cfg, &state);
        assert!(missing.contains_key("blog.example.com"), "{missing:?}");
    }

    /// Under `DnsPublishMode::OwningNode`, only the node actually running
    /// `blog`'s container is published, not every node - the fixture always
    /// assigns it to n1.
    #[test]
    fn owning_node_mode_publishes_only_the_running_nodes_address() {
        let mut cfg = cfg();
        cfg.proxy.dns_publish = cl_config::DnsPublishMode::OwningNode;

        let records = desired_records(&cfg, None, &state_with(SPEC, 1));
        let blog: Vec<_> = records
            .iter()
            .filter(|r| r.fqdn == "blog.example.com")
            .collect();
        // n1 (the only node running the container) publishes both its ipv4
        // and ipv6 address; n2's cname must not appear.
        assert_eq!(blog.len(), 2, "{blog:?}");
        assert!(
            blog.iter()
                .all(|r| r.content == "203.0.113.5" || r.content == "2001:db8::5"),
            "{blog:?}"
        );
    }

    /// A static route is not backed by a container, so `OwningNode` mode
    /// still publishes it on every node.
    #[test]
    fn owning_node_mode_still_publishes_static_routes_on_every_node() {
        let mut cfg = cfg();
        cfg.proxy.dns_publish = cl_config::DnsPublishMode::OwningNode;
        let state = with_static_routes(
            state_with(SPEC, 0),
            vec![static_route("printer", "wan", "192.168.1.50:9100")],
        );

        let records = desired_records(&cfg, None, &state);
        let printer: Vec<_> = records
            .iter()
            .filter(|r| r.fqdn == "printer.example.com")
            .collect();
        assert_eq!(printer.len(), 2, "{printer:?}");
        assert!(printer.iter().all(|r| r.kind != cl_dns::RecordType::CNAME));
    }

    /// The dashboard only works on the leader (writes 503 elsewhere), so only
    /// the leader's address is published for it - never every node's.
    #[test]
    fn desired_records_publish_only_the_leaders_address_for_the_dashboard() {
        let cfg = cfg();
        let leader = cfg.node("n1").expect("n1 in the reference config");
        let leader_id = cl_raft::node_id(leader.id);

        let records = desired_records(&cfg, Some(leader_id), &state_with(SPEC, 1));
        let dashboard: Vec<_> = records
            .iter()
            .filter(|r| r.fqdn == "cloud.int.example.com")
            .collect();
        assert_eq!(dashboard.len(), 1, "{dashboard:?}");
        assert_eq!(dashboard[0].content, "10.10.0.1");
    }

    /// No known leader yet (fresh cluster, election in progress): the
    /// dashboard gets no record rather than one for every node.
    #[test]
    fn desired_records_publish_nothing_for_the_dashboard_with_no_known_leader() {
        let records = desired_records(&cfg(), None, &state_with(SPEC, 1));
        assert!(!records.iter().any(|r| r.fqdn == "cloud.int.example.com"));
    }

    #[test]
    fn the_longest_matching_zone_wins() {
        let zones = Zones {
            zones: vec![
                ("example.com".into(), "parent".into()),
                ("int.example.com".into(), "child".into()),
            ],
        };
        assert_eq!(
            zones.zone_id_for("a.int.example.com").as_deref(),
            Some("child")
        );
        assert_eq!(
            zones.zone_id_for("a.example.com").as_deref(),
            Some("parent")
        );
        assert_eq!(zones.zone_id_for("example.org"), None);
    }

    #[test]
    fn container_segment_addresses_are_clients_of_the_allowed_networks_and_their_own() {
        let cfg = cfg();
        let mut state = state_with(SPEC, 0);
        let mut allocations =
            Allocations::decode(state.overlay.as_deref()).expect("state_with sets an overlay");
        let seg = cl_net::container_net::container_docker_name("tailnet");
        allocations.allocate_network(&seg, &[]).unwrap();
        let n1 = allocations.allocate_node_ip(&seg, "n1").unwrap();
        let n2 = allocations.allocate_node_ip(&seg, "n2").unwrap();
        state.overlay = Some(serde_json::to_string(&allocations).unwrap());

        // `lan` is in the example's allowed_networks: both nodes' segment
        // addresses count as its clients, and it is not container-only.
        let lan = local_clients(&cfg, &state, "lan");
        assert_eq!(
            lan.addrs,
            vec![std::net::IpAddr::V4(n1), std::net::IpAddr::V4(n2)]
        );
        assert!(!lan.only);
        // A network the container network does not list gets nothing.
        let mut other = cfg.clone();
        other.networks.get_mut("tailnet").unwrap().access = Some(Vec::new());
        assert!(local_clients(&other, &state, "lan").addrs.is_empty());
        // A route on the container network itself is for its clients only.
        let own = local_clients(&cfg, &state, "tailnet");
        assert!(own.only);
        assert_eq!(own.addrs.len(), 2);
    }

    #[test]
    fn a_route_admits_the_subnets_of_networks_whose_access_lists_it() {
        let mut cfg = cfg();
        let lan = cfg.networks["lan"].subnet.unwrap();
        let ts = cfg.networks["tailscale"].subnet.unwrap();
        assert_eq!(allowed_subnets(&cfg, "lan"), vec![lan]);

        cfg.networks.get_mut("tailscale").unwrap().access = Some(vec!["lan".into()]);
        assert_eq!(allowed_subnets(&cfg, "lan"), vec![lan, ts]);
        // The grant is one way: tailscale's own route does not admit lan.
        assert_eq!(allowed_subnets(&cfg, "tailscale"), vec![ts]);
        // A public route is ungated.
        assert!(allowed_subnets(&cfg, "wan").is_empty());
    }
}
