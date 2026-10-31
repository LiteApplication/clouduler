//! Wiring egress and wireguard networks into the daemon: for every such
//! network this node has a host interface address for, keep its bridge, its
//! kill-switch iptables rules and its docker network up.
//!
//! Unlike `overlay`, this needs no leader-side allocation and no per-instance
//! bookkeeping: the bridge's subnet is derived from the network's name alone
//! (`cl_net::egress::egress_subnet`), so every node implementing the network
//! computes it independently, and the network is brought up whether or not
//! anything is currently scheduled on it — cheap and idempotent, and it means
//! a container placed here on the very next sweep already has somewhere to
//! attach.
//!
//! The reconcile loop re-reads the cluster config from raft every tick
//! (`RECONCILE_INTERVAL`) instead of working off the snapshot the daemon
//! booted with, so an egress network an operator just added comes up on this
//! node's next tick rather than needing a restart — a bridge nothing is
//! forwarding through yet has no cost to bringing up a tick late. See
//! `docs/live-config.md` for the rest of what applies live and what doesn't.
//!
//! A wireguard-kind network is an egress network whose interface clouduler
//! itself creates and configures, from a config imported through the api and
//! stored as a sealed secret (`cl_api::wireguard::wireguard_secret_name`),
//! instead of one the admin brought up out of band. Once that interface is
//! up, it is indistinguishable from a plain egress interface to everything
//! below: the same bridge, kill-switch and docker network logic applies. A
//! wireguard network additionally gets an ingress ruleset for ports let in
//! through the tunnel and forwarded to this node's own address, where an
//! already-published docker port bind picks them up.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::restrict::{names_to_resolve, restricted_stacks, restrictions, HostResolver};
use anyhow::{Context, Result};
use cl_api::wireguard::{wireguard_secret_name, WireguardIngressStore};
use cl_config::{ClusterConfig, InterfaceAddress, NetworkKind, NodeAddress};
use cl_docker::{Docker, NetworkSpec};
use cl_net::egress::{
    container_input_plan, container_pools, egress_bridge_name, egress_docker_name, egress_gateway,
    egress_iptables_plan, egress_subnet, ensure_egress_bridge, nat_docker_name, nat_subnet,
    overlay_pool, overlay_snat_plan, HostServices, Wanted,
};
use cl_net::iptables::apply_plan;
use cl_net::restrict::{
    forward_plan_with_wan, redirect_plan, Restriction, WanRestriction, FORWARD_CHAIN,
    REDIRECT_CHAIN,
};
use cl_net::vxlan::Overlay;
use cl_net::wireguard::{
    ensure_wireguard_interface, parse_wg_quick, wireguard_ingress_iptables_plan, IngressRule,
    WireguardConfig,
};
use cl_raft::node::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tracing::{debug, info, warn};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Docker network label for cluster infrastructure that isn't owned by any
/// one stack, so a future per-stack sweep never mistakes it for orphaned.
pub const OWNER: &str = "_cluster";

/// Every egress or wireguard network this node implements, network name ->
/// (kind, address). For a wireguard network the interface name inside the
/// address is still the one stored in `node.network`, but it names a tunnel
/// clouduler manages rather than one the admin set up.
pub fn interfaces_of(
    cfg: &ClusterConfig,
    node_name: &str,
) -> BTreeMap<String, (NetworkKind, InterfaceAddress)> {
    let Some(node) = cfg.nodes.get(node_name) else {
        return BTreeMap::new();
    };
    cfg.networks
        .iter()
        .filter(|(_, net)| matches!(net.kind, NetworkKind::Egress | NetworkKind::Wireguard))
        .filter_map(|(name, net)| match node.network.get(name) {
            Some(NodeAddress::Interface(addr)) => Some((name.clone(), (net.kind, addr.clone()))),
            _ => None,
        })
        .collect()
}

/// The interface a network's traffic should actually leave through right
/// now. Only an `egress`-kind address with a `fallback` configured ever
/// probes anything; everything else (wireguard tunnels, egress addresses
/// with no fallback) just uses its configured interface unconditionally, so
/// a cluster that never opted into this costs nothing extra per tick.
async fn effective_interface(kind: NetworkKind, addr: &InterfaceAddress, network: &str) -> String {
    if kind != NetworkKind::Egress {
        return addr.interface.clone();
    }
    let Some(fallback) = &addr.fallback else {
        return addr.interface.clone();
    };
    let primary = addr.interface.clone();
    let target = fallback.health_check_ip;
    let primary_iface = primary.clone();
    let reachable = tokio::task::spawn_blocking(move || {
        cl_net::egress::probe_reachable(&primary_iface, target)
    })
    .await
    .unwrap_or(false);
    if reachable {
        primary
    } else {
        warn!(
            network = %network, primary = %primary, fallback = %fallback.interface, target = %target,
            "primary egress interface did not answer a probe, routing out the fallback interface instead",
        );
        fallback.interface.clone()
    }
}

/// Reads the cluster config raft is currently carrying, parsed the same way
/// the daemon parses it off disk at boot. Falls back to `previous` — the
/// last config this loop successfully parsed, starting from the one the
/// daemon booted with — if raft has nothing yet (a brand new node, mid
/// bootstrap) or the stored yaml fails to parse (should not happen, since
/// nothing accepts a config into raft without validating it first, but a
/// reconcile loop should never crash over it either way).
async fn live_config(cluster: &ClusterNode, previous: &ClusterConfig) -> ClusterConfig {
    let Some(entry) = cluster.state().await.cluster_config else {
        return previous.clone();
    };
    match ClusterConfig::parse(&entry.yaml) {
        Ok(cfg) => cfg,
        Err(e) => {
            warn!(error = %e, "cluster config in raft failed to parse, reconciling egress networks with the last good config instead");
            previous.clone()
        }
    }
}

pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    docker: Arc<tokio::sync::RwLock<Option<Docker>>>,
    key_rx: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    wan: Arc<crate::wan::WanFirewall>,
) -> tokio::task::JoinHandle<()> {
    let cluster = cluster.clone();
    let node_name = node_name.to_string();
    let boot_cfg = cfg.clone();

    tokio::spawn(async move {
        // Netlink needs CAP_NET_ADMIN, same requirement as the overlay.
        let netlink = match rtnetlink::new_connection() {
            Ok((connection, handle, _)) => {
                tokio::spawn(connection);
                Some(Overlay::new(handle))
            }
            Err(e) => {
                warn!(error = %e, "no netlink access, egress networks will not come up");
                None
            }
        };

        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        let mut cfg = boot_cfg;
        let resolved = spawn_resolver(cluster.clone(), cfg.clone());
        let mut restricted: Vec<Restriction> = Vec::new();
        let mut firewall_clean = false;
        let mut wan_wake = wan.subscribe();
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                // A lookup (or a new container) changed what a wan list
                // allows. Nothing else is worth redoing for that, and a
                // lookup is waiting on it.
                Ok(()) = wan_wake.changed() => {
                    let version = wan.version();
                    let wan_rules = wan.restrictions(std::time::Instant::now());
                    match ensure_restrictions(&cfg, &node_name, &restricted, &wan_rules, &mut firewall_clean).await {
                        Ok(()) => wan.mark_applied(version),
                        Err(e) => warn!(error = %e, "cannot apply the wan allowlists"),
                    }
                    continue;
                }
            }
            cfg = live_config(&cluster, &cfg).await;

            let state = cluster.state().await;
            // Unknown state keeps the restrictions applied so far; see
            // `restricted_stacks`.
            if let Some(stacks) = restricted_stacks(&cfg, &state) {
                restricted = restrictions(&stacks, &resolved.borrow());
            }

            // Before the docker check on purpose: the filter protects the host
            // from whatever containers exist, and must not wait for docker to
            // (re)connect to be in place.
            if let Err(e) = ensure_container_input(&state, &cfg, &restricted).await {
                warn!(error = %e, "cannot apply the container input filter");
            }
            let wan_version = wan.version();
            let wan_rules = wan.restrictions(std::time::Instant::now());
            match ensure_restrictions(
                &cfg,
                &node_name,
                &restricted,
                &wan_rules,
                &mut firewall_clean,
            )
            .await
            {
                Ok(()) => wan.mark_applied(wan_version),
                Err(e) => warn!(error = %e, "cannot apply the per-stack network restrictions"),
            }

            let Some(docker) = docker.read().await.clone() else {
                continue;
            };

            // Nat networks need no per-node interface at all: every node
            // implements them the same way, so this runs regardless of
            // whether the node has any egress/wireguard interfaces below.
            for (network, net) in &cfg.networks {
                if net.kind != NetworkKind::Nat {
                    continue;
                }
                if let Err(e) = ensure_nat(&docker, network).await {
                    warn!(network = %network, error = %e, "cannot bring up the nat network");
                }
            }

            if let Err(e) = ensure_overlay_snat(&cfg, &node_name).await {
                warn!(error = %e, "cannot apply the overlay source NAT towards the other nodes");
            }

            let interfaces = interfaces_of(&cfg, &node_name);
            // Only once raft has handed us a config and it names this node: a
            // node missing from a stale or fallback config would otherwise
            // look like "implements nothing" and have everything swept.
            if cluster.state().await.cluster_config.is_some() && cfg.nodes.contains_key(&node_name)
            {
                sweep_orphans(&cfg, &docker, &interfaces).await;
            }
            if interfaces.is_empty() {
                continue;
            }
            // Read fresh every pass rather than cached: a node started with
            // no key still brings wireguard tunnels up once one is delivered
            // to it, without a restart.
            let key = key_rx.borrow().clone();
            let internal_addr = cfg
                .nodes
                .get(&node_name)
                .and_then(|node| node.ip_on(cfg.internal_network().0));

            for (network, (kind, addr)) in &interfaces {
                let iface = &addr.interface;
                if *kind == NetworkKind::Wireguard {
                    if let Err(e) =
                        ensure_tunnel(&cluster, key.as_deref(), &node_name, network, iface).await
                    {
                        warn!(network = %network, iface = %iface, error = %e, "cannot bring up the wireguard tunnel");
                        continue;
                    }
                }
                let effective = effective_interface(*kind, addr, network).await;
                if let Err(e) =
                    ensure_one(netlink.as_ref(), &docker, network, &effective, *kind).await
                {
                    warn!(network = %network, iface = %effective, error = %e, "cannot bring up the egress network");
                }
            }

            // Applied whether or not the node has an internal address yet: the
            // default deny must not depend on it, only the DNAT target does.
            let cluster_state = cluster.state().await;
            let ingress_store = WireguardIngressStore::load(&cluster_state);
            for (network, (kind, iface_addr)) in &interfaces {
                if *kind != NetworkKind::Wireguard {
                    continue;
                }
                let iface = &iface_addr.interface;
                if let Err(e) =
                    apply_ingress(iface, internal_addr, network, iface_addr, &ingress_store).await
                {
                    warn!(network = %network, iface = %iface, error = %e, "cannot apply wireguard ingress rules");
                }
            }
        }
    })
}

/// Keeps containers from reaching the host's own listeners. See
/// `cl_net::egress::container_input_plan` for exactly what is let through.
async fn ensure_container_input(
    state: &cl_raft::state::ClusterState,
    cfg: &ClusterConfig,
    restricted: &[Restriction],
) -> Result<()> {
    let services = HostServices {
        proxy_ports: if cfg.proxy.enabled {
            vec![cfg.proxy.http_port, cfg.proxy.https_port]
        } else {
            Vec::new()
        },
        published: crate::expose::published_ports(state, cfg),
        restricted: restricted.to_vec(),
    };
    let plan = container_input_plan(overlay_pool(), &container_pools(), &services);
    tokio::task::spawn_blocking(move || apply_plan(&plan))
        .await
        .context("iptables task panicked")?
        .context("applying the container input filter")
}

/// Resolves the names of `host:port` entries on its own task, so a slow or
/// dead resolver cannot hold up the egress reconcile that applies the rules.
fn spawn_resolver(
    cluster: Arc<ClusterNode>,
    boot_cfg: ClusterConfig,
) -> tokio::sync::watch::Receiver<BTreeMap<String, Vec<std::net::Ipv4Addr>>> {
    let (tx, rx) = tokio::sync::watch::channel(BTreeMap::new());
    tokio::spawn(async move {
        let mut resolver = HostResolver::default();
        let mut cfg = boot_cfg;
        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            ticker.tick().await;
            cfg = live_config(&cluster, &cfg).await;
            let state = cluster.state().await;
            if let Some(stacks) = restricted_stacks(&cfg, &state) {
                resolver.refresh(&names_to_resolve(&stacks)).await;
                tx.send_replace(resolver.resolved().clone());
            }
        }
    });
    rx
}

/// Applies the routed-traffic filter and the proxy redirect for stacks that
/// declared an `allowed` list, or removes both chains once none does, so a
/// cluster that never uses the feature carries no extra rule at all.
///
/// `clean` remembers that the chains are known to be absent, so the idle case
/// costs nothing per tick; it starts false to clear what a previous daemon run
/// left behind.
async fn ensure_restrictions(
    cfg: &ClusterConfig,
    node_name: &str,
    restricted: &[Restriction],
    wan: &[WanRestriction],
    clean: &mut bool,
) -> Result<()> {
    if restricted.is_empty() && wan.is_empty() {
        if *clean {
            return Ok(());
        }
        tokio::task::spawn_blocking(|| {
            for (table, chain) in [("filter", FORWARD_CHAIN), ("nat", REDIRECT_CHAIN)] {
                if cl_net::iptables::list_chains(table)?
                    .iter()
                    .any(|c| c == chain)
                {
                    cl_net::iptables::remove_chain(table, chain)?;
                }
            }
            Ok::<_, cl_net::NetError>(())
        })
        .await
        .context("iptables task panicked")?
        .context("removing the per-stack restriction chains")?;
        *clean = true;
        return Ok(());
    }
    *clean = false;

    let local = cfg
        .nodes
        .get(node_name)
        .and_then(|node| match cfg.internal_ip(node) {
            Some(IpAddr::V4(ip)) => Some(ip),
            _ => None,
        });
    let others: Vec<std::net::Ipv4Addr> = cfg
        .nodes
        .iter()
        .filter(|(name, _)| name.as_str() != node_name)
        .filter_map(|(_, node)| match cfg.internal_ip(node) {
            Some(IpAddr::V4(ip)) => Some(ip),
            _ => None,
        })
        .collect();
    let proxy_ports: Vec<u16> = if cfg.proxy.enabled {
        vec![cfg.proxy.http_port, cfg.proxy.https_port]
    } else {
        Vec::new()
    };
    let forward = forward_plan_with_wan(restricted, wan);
    // Without this node's own internal address there is nowhere to redirect
    // to; the forward filter alone still keeps the stack in.
    let redirect = local.map(|local| redirect_plan(restricted, &others, local, &proxy_ports));
    tokio::task::spawn_blocking(move || {
        apply_plan(&forward)?;
        if let Some(redirect) = redirect {
            apply_plan(&redirect)?;
        }
        Ok::<_, cl_net::NetError>(())
    })
    .await
    .context("iptables task panicked")?
    .context("applying the per-stack network restrictions")
}

/// Keeps an overlay container able to reach the other nodes' addresses on
/// the internal network, where each node's reverse proxy answers. See
/// `cl_net::egress::overlay_snat_plan` for why this needs a source NAT.
async fn ensure_overlay_snat(cfg: &ClusterConfig, node_name: &str) -> Result<()> {
    let others: Vec<std::net::Ipv4Addr> = cfg
        .nodes
        .iter()
        .filter(|(name, _)| name.as_str() != node_name)
        .filter_map(|(_, node)| match cfg.internal_ip(node) {
            Some(IpAddr::V4(ip)) => Some(ip),
            _ => None,
        })
        .collect();
    // Only the proxy answers on another node for a container, so only its
    // ports (and pings) are worth laundering through this node's address.
    let proxy_ports: Vec<u16> = if cfg.proxy.enabled {
        vec![cfg.proxy.http_port, cfg.proxy.https_port]
    } else {
        Vec::new()
    };
    let plan = overlay_snat_plan(overlay_pool(), &others, &proxy_ports);
    tokio::task::spawn_blocking(move || apply_plan(&plan))
        .await
        .context("iptables task panicked")?
        .context("applying the overlay source NAT")
}

/// Remove what a network that is gone from this node left behind: its docker
/// network, kill-switch and ingress chains with their jumps, policy route and
/// bridge, and a wireguard tunnel (which would otherwise stay up holding its
/// key, with its ingress ports open).
///
/// The docker network goes first and gates the rest. One that still has
/// containers attached refuses removal, and then its bridge and kill-switch
/// are kept: tearing the drop rule out from under a running container would
/// let it out the host's main route. Everything is retried next tick.
async fn sweep_orphans(
    cfg: &ClusterConfig,
    docker: &Docker,
    interfaces: &BTreeMap<String, (NetworkKind, InterfaceAddress)>,
) {
    let mut wanted = Wanted::default();
    for (network, (kind, addr)) in interfaces {
        wanted.bridges.insert(egress_bridge_name(network));
        if *kind == NetworkKind::Wireguard {
            wanted.wg_ifaces.insert(addr.interface.clone());
        }
    }

    match docker.list_networks_of_stack(OWNER).await {
        Ok(names) => {
            for name in names {
                if let Some(network) = name.strip_prefix("egress_") {
                    if interfaces.contains_key(network) {
                        continue;
                    }
                    match docker.remove_network(&name).await {
                        Ok(()) => {
                            info!(network = %network, "removed the docker network of an egress network this node no longer implements")
                        }
                        Err(e) => {
                            debug!(network = %network, error = %e, "cannot remove the docker network yet, keeping its bridge and kill-switch");
                            wanted.bridges.insert(egress_bridge_name(network));
                        }
                    }
                } else if let Some(network) = name.strip_prefix("nat_") {
                    let still_nat = cfg
                        .networks
                        .get(network)
                        .is_some_and(|net| net.kind == NetworkKind::Nat);
                    if !still_nat {
                        if let Err(e) = docker.remove_network(&name).await {
                            debug!(network = %network, error = %e, "cannot remove the nat docker network yet");
                        } else {
                            info!(network = %network, "removed the docker network of a nat network that no longer exists");
                        }
                    }
                }
            }
        }
        Err(e) => {
            // Without the list there is no way to tell which bridges are still
            // pinned by a docker network, so sweep nothing this tick.
            debug!(error = %e, "cannot list cluster docker networks, skipping the orphan sweep");
            return;
        }
    }

    let swept = tokio::task::spawn_blocking(move || {
        let host = cl_net::egress::scan_host()?;
        let actions = cl_net::egress::plan_sweep(&host, &wanted);
        Ok::<_, cl_net::NetError>(cl_net::egress::run_sweep(&actions))
    })
    .await;
    match swept {
        Ok(Ok(report)) => {
            for removed in &report.removed {
                info!(%removed, "removed a leftover of a network this node no longer implements");
            }
            for failed in &report.failed {
                warn!(%failed, "cannot remove a leftover of a removed network, will retry");
            }
        }
        Ok(Err(e)) => warn!(error = %e, "cannot scan the host for leftover network objects"),
        Err(e) => warn!(error = %e, "orphan sweep task panicked"),
    }
}

/// Open this node's sealed config for `network` and bring the tunnel
/// interface up from it. A missing secret (nothing imported yet) or missing
/// key both fail closed: no interface, so `ensure_one` below has nothing to
/// route the bridge out of and its kill-switch keeps everything in.
async fn ensure_tunnel(
    cluster: &Arc<ClusterNode>,
    key: Option<&ClusterKey>,
    node_name: &str,
    network: &str,
    iface: &str,
) -> Result<()> {
    let secret_name = wireguard_secret_name(network, node_name);
    let state = cluster.state().await;
    let entry = state
        .secrets
        .get(&secret_name)
        .with_context(|| format!("no wireguard config imported yet for {network}"))?;
    let key = key.with_context(|| {
        format!("this node holds no cluster key, cannot open the {network} config")
    })?;
    let plaintext = Sealed::from_blob(entry.sealed.clone())
        .open(key, &secret_name)
        .with_context(|| format!("cannot open the {network} wireguard config"))?;
    let text = String::from_utf8(plaintext)
        .with_context(|| format!("{network} wireguard config is not utf8"))?;
    let wg_cfg: WireguardConfig =
        parse_wg_quick(&text).with_context(|| format!("invalid wireguard config for {network}"))?;

    let iface = iface.to_string();
    tokio::task::spawn_blocking(move || ensure_wireguard_interface(&iface, &wg_cfg))
        .await
        .context("wireguard interface task panicked")?
        .with_context(|| format!("bringing up the {network} tunnel"))?;
    Ok(())
}

async fn apply_ingress(
    iface: &str,
    addr: Option<IpAddr>,
    network: &str,
    iface_addr: &InterfaceAddress,
    store: &WireguardIngressStore,
) -> Result<()> {
    let mut rules: Vec<IngressRule> = store
        .rules
        .iter()
        .filter(|r| r.network == network)
        .map(|r| IngressRule {
            port: r.port,
            protocol: r.protocol,
            target_port: r.target_port,
        })
        .collect();
    // Ports the node's config marks open on this tunnel (a VPN provider's port
    // forwarding, say) arrive unrewritten at a published port; they are as
    // open as an explicit ingress rule, or the default deny would eat them.
    rules.extend(iface_addr.open_ports.iter().map(|open| IngressRule {
        port: open.port,
        protocol: open.protocol,
        target_port: open.port,
    }));
    let plan = wireguard_ingress_iptables_plan(iface, addr, &rules);
    tokio::task::spawn_blocking(move || apply_plan(&plan))
        .await
        .context("iptables task panicked")??;
    Ok(())
}

async fn ensure_one(
    netlink: Option<&Overlay>,
    docker: &Docker,
    network: &str,
    iface: &str,
    kind: NetworkKind,
) -> Result<()> {
    let Some(netlink) = netlink else {
        return Ok(());
    };
    let subnet = egress_subnet(network);
    let gateway = egress_gateway(subnet);
    let bridge = ensure_egress_bridge(netlink, network, subnet)
        .await
        .with_context(|| format!("ensuring the {network} egress bridge"))?;

    let iface_owned = iface.to_string();
    let bridge_owned = bridge.clone();
    // Loose reverse-path filtering (2) rather than strict (1): a reply can
    // arrive on the tunnel while the policy route for the bridge, not the main
    // table, is what would route its source, and strict mode drops that
    // asymmetric path. The last write sets the host-wide `all` value too, and
    // the kernel applies the larger of `all` and the per-interface value, so
    // this is a deliberate host-wide change: strict becomes loose (and off
    // becomes loose) on every interface. Left as is; it is the documented
    // price of policy routing here.
    tokio::task::spawn_blocking(move || -> Result<()> {
        std::fs::write(
            format!("/proc/sys/net/ipv4/conf/{bridge_owned}/rp_filter"),
            b"2",
        )?;
        std::fs::write(
            format!("/proc/sys/net/ipv4/conf/{iface_owned}/rp_filter"),
            b"2",
        )?;
        std::fs::write("/proc/sys/net/ipv4/conf/all/rp_filter", b"2")?;
        Ok(())
    })
    .await
    .context("rp_filter task panicked")?
    .context("writing rp_filter sysctls")?;

    // Shared with config validation, which refuses two networks that land on
    // the same table.
    let table_id = cl_config::egress_table_id(network);

    let iface_for_route = iface.to_string();
    let bridge_for_rule = bridge.clone();
    let network_owned = network.to_string();
    let routed = tokio::task::spawn_blocking(move || -> Result<bool> {
        let table_str = table_id.to_string();
        let rule_pattern = format!("iif {bridge_for_rule} lookup {table_str}");
        let existing_rules = String::from_utf8_lossy(
            &std::process::Command::new("ip")
                .args(["rule", "show"])
                .output()?
                .stdout,
        )
        .to_string();

        // Rules left on this bridge by an older table id would match before
        // the current one; see `plan_stale_rules`.
        let stale_rules = cl_net::egress::plan_stale_rules(
            &cl_net::egress::parse_policy_rules(&existing_rules),
            &bridge_for_rule,
            table_id,
        );
        let remove_stale = |actions: &[cl_net::egress::SweepAction]| {
            for failed in cl_net::egress::run_sweep(actions).failed {
                warn!(network = %network_owned, "removing stale egress routing: {failed}");
            }
        };

        if kind == NetworkKind::Wireguard {
            // A wireguard tunnel does cryptokey routing: there is no
            // next-hop gateway address the way a plain ethernet egress
            // interface has, so the table-scoped default route here is a
            // bare device route, not a `via` route. The interface is
            // clouduler's own (`ensure_tunnel` brought it up this same
            // tick), so unlike the egress case below there is no "is this
            // operator-managed interface actually up" check to do first.
            let status = std::process::Command::new("ip")
                .args([
                    "route",
                    "replace",
                    "default",
                    "dev",
                    &iface_for_route,
                    "table",
                    &table_str,
                ])
                .status()?;
            if !status.success() {
                anyhow::bail!("ip route replace failed");
            }
        } else {
            // `iface`'s own default route, if it has one: this must come from a
            // route that already names `iface` as its device, never assumed from
            // the host's overall default route, since on a multi-homed node the
            // interface this egress network is pinned to may not be the one the
            // host itself prefers (different metric, or not a default route
            // candidate at all).
            let route_out = std::process::Command::new("ip")
                .args(["route", "show", "default", "dev", &iface_for_route])
                .output()?;
            let route_str = String::from_utf8_lossy(&route_out.stdout);
            let parts: Vec<&str> = route_str.split_whitespace().collect();
            let via = parts
                .iter()
                .position(|&x| x == "via")
                .and_then(|i| parts.get(i + 1))
                .copied();

            // No gateway found for `iface`: it has no working route out (down,
            // no DHCP lease, cable unplugged, ...). Installing a `dev`-only
            // default route here would silently blackhole every packet from
            // this bridge ("No route to host") instead of the kill-switch's
            // intended DROP, and would look like a healthy path while nothing
            // actually reaches it. So install no policy route at all and let
            // the kill-switch's plain `-o iface` FORWARD rule fail to match, as
            // it should when the network's egress interface is down.
            let Some(via) = via else {
                if existing_rules.contains(&rule_pattern) {
                    std::process::Command::new("ip")
                        .args([
                            "rule",
                            "del",
                            "iif",
                            &bridge_for_rule,
                            "lookup",
                            &table_str,
                            "priority",
                            "5100",
                        ])
                        .status()
                        .ok();
                }
                remove_stale(&stale_rules);
                warn!(
                    network = %network_owned, iface = %iface_for_route,
                    "no default route found on this interface, egress is blocked until it has one",
                );
                // Not an error, but not routed either: the caller locks the
                // bridge down with a plain drop. Without the policy rule the
                // bridge's traffic would take the main routing table out some
                // other interface.
                return Ok(false);
            };

            let status = std::process::Command::new("ip")
                .args([
                    "route",
                    "replace",
                    "default",
                    "via",
                    via,
                    "dev",
                    &iface_for_route,
                    "table",
                    &table_str,
                ])
                .status()?;
            if !status.success() {
                anyhow::bail!("ip route replace failed");
            }
        }

        if !existing_rules.contains(&rule_pattern) {
            let status = std::process::Command::new("ip")
                .args([
                    "rule",
                    "add",
                    "iif",
                    &bridge_for_rule,
                    "lookup",
                    &table_str,
                    "priority",
                    "5100",
                ])
                .status()?;
            if !status.success() {
                anyhow::bail!("ip rule add failed");
            }
        }
        // Only after the current rule is in place, so the bridge is never
        // left without one.
        remove_stale(&stale_rules);
        Ok(true)
    })
    .await
    .context("policy routing task panicked")??;

    // With no route out the plan has no accept at all, only the bridge-wide
    // drop; see `egress_iptables_plan`.
    let plan = egress_iptables_plan(&bridge, subnet, routed.then_some(iface));
    let bridge_for_blocking = bridge.clone();
    tokio::task::spawn_blocking(move || apply_plan(&plan))
        .await
        .context("iptables task panicked")?
        .with_context(|| format!("applying the kill-switch for {bridge_for_blocking}"))?;

    let mtu = std::fs::read_to_string(format!("/sys/class/net/{iface}/mtu"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1500);

    let created = docker
        .ensure_network(&NetworkSpec {
            name: egress_docker_name(network),
            bridge: bridge.clone(),
            subnet: subnet.to_string(),
            gateway,
            mtu,
            stack: OWNER.to_string(),
        })
        .await
        .with_context(|| format!("ensuring the {network} docker network"))?;

    if created {
        info!(network = %network, %subnet, %bridge, iface = %iface, %mtu, "egress network up");
    }

    Ok(())
}

/// Bring up a nat-kind network's docker network: no interface and no
/// iptables of clouduler's own — docker builds the bridge and NATs it out
/// whatever the host's routing table picks, exactly like any bridge network
/// docker would make on its own. The subnet itself is still ours to pick
/// (see `cl_net::egress::nat_subnet`), so a same-node hairpin back into a
/// gated route can be recognized and trusted (`proxy::trusted_overlay_subnets`).
async fn ensure_nat(docker: &Docker, network: &str) -> Result<()> {
    let name = nat_docker_name(network);
    let subnet = nat_subnet(network);
    let created = docker
        .ensure_nat_network(&name, OWNER, &subnet.to_string(), egress_gateway(subnet))
        .await
        .with_context(|| format!("ensuring the {network} nat docker network"))?;
    if created {
        info!(network = %network, docker_name = %name, %subnet, "nat network up");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::{ClusterConfig, Network};

    fn cfg() -> ClusterConfig {
        let mut cfg = ClusterConfig::parse(include_str!("../../../examples/cluster.yaml"))
            .expect("example parses");
        cfg.networks.insert(
            "vpn".to_string(),
            Network {
                kind: NetworkKind::Egress,
                subnet: None,
                domain: None,
                publish_dns: true,
            },
        );
        cfg.networks.insert(
            "wg".to_string(),
            Network {
                kind: NetworkKind::Wireguard,
                subnet: None,
                domain: None,
                publish_dns: true,
            },
        );
        cfg
    }

    #[test]
    fn a_node_with_an_interface_address_implements_the_network() {
        let mut cfg = cfg();
        cfg.nodes.get_mut("n1").unwrap().network.insert(
            "vpn".to_string(),
            NodeAddress::Interface(cl_config::InterfaceAddress::new("wg0")),
        );
        let interfaces = interfaces_of(&cfg, "n1");
        let (kind, addr) = interfaces.get("vpn").expect("vpn implemented");
        assert_eq!(*kind, NetworkKind::Egress);
        assert_eq!(addr.interface, "wg0");
    }

    #[test]
    fn a_node_with_a_wireguard_address_implements_it_as_wireguard_kind() {
        let mut cfg = cfg();
        cfg.nodes.get_mut("n1").unwrap().network.insert(
            "wg".to_string(),
            NodeAddress::Interface(cl_config::InterfaceAddress::new("wg-wg")),
        );
        let interfaces = interfaces_of(&cfg, "n1");
        let (kind, addr) = interfaces.get("wg").expect("wg implemented");
        assert_eq!(*kind, NetworkKind::Wireguard);
        assert_eq!(addr.interface, "wg-wg");
    }

    #[test]
    fn a_node_without_an_address_on_the_network_implements_nothing() {
        // n3 only has an address on tailscale in the example.
        assert!(interfaces_of(&cfg(), "n3").is_empty());
    }

    #[test]
    fn only_egress_and_wireguard_kind_networks_are_considered() {
        // n2 has addresses on tailscale/lan/wan in the example, neither kind.
        assert!(interfaces_of(&cfg(), "n2").is_empty());
    }

    #[test]
    fn an_unknown_node_implements_nothing() {
        assert!(interfaces_of(&cfg(), "does-not-exist").is_empty());
    }

    #[tokio::test]
    async fn an_egress_address_with_no_fallback_never_probes_and_uses_its_interface() {
        let addr = cl_config::InterfaceAddress::new("wg0");
        assert_eq!(
            effective_interface(NetworkKind::Egress, &addr, "vpn").await,
            "wg0"
        );
    }

    #[tokio::test]
    async fn a_wireguard_address_ignores_any_fallback_and_uses_its_own_interface() {
        let mut addr = cl_config::InterfaceAddress::new("wg-wg");
        addr.fallback = Some(cl_config::EgressFallback {
            interface: "end0".to_string(),
            health_check_ip: "1.1.1.1".parse().unwrap(),
        });
        assert_eq!(
            effective_interface(NetworkKind::Wireguard, &addr, "wg").await,
            "wg-wg"
        );
    }

    #[tokio::test]
    async fn an_egress_address_falls_back_once_the_primary_stops_answering() {
        // "does-not-exist0" can never be a real interface, so the probe
        // behind it always fails closed the same way a genuinely dead
        // interface would, without needing root or a live network.
        let mut addr = cl_config::InterfaceAddress::new("does-not-exist0");
        addr.fallback = Some(cl_config::EgressFallback {
            interface: "end0".to_string(),
            health_check_ip: "1.1.1.1".parse().unwrap(),
        });
        assert_eq!(
            effective_interface(NetworkKind::Egress, &addr, "vpn").await,
            "end0"
        );
    }
}
