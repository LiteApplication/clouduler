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

use anyhow::{Context, Result};
use cl_api::wireguard::{wireguard_secret_name, WireguardIngressStore};
use cl_config::{ClusterConfig, NetworkKind, NodeAddress};
use cl_docker::{Docker, NetworkSpec};
use cl_net::egress::{
    apply_egress_iptables, egress_docker_name, egress_gateway, egress_iptables_plan, egress_subnet,
    ensure_egress_bridge,
};
use cl_net::vxlan::Overlay;
use cl_net::wireguard::{
    apply_wireguard_ingress_iptables, ensure_wireguard_interface, parse_wg_quick,
    wireguard_ingress_iptables_plan, IngressRule, WireguardConfig,
};
use cl_raft::node::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tracing::{info, warn};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Docker network label for cluster infrastructure that isn't owned by any
/// one stack, so a future per-stack sweep never mistakes it for orphaned.
const OWNER: &str = "_cluster";

/// Every egress or wireguard network this node implements, network name ->
/// (kind, host interface). For a wireguard network the interface name is
/// still the one stored in `node.network`, but it names a tunnel clouduler
/// manages rather than one the admin set up.
pub fn interfaces_of(cfg: &ClusterConfig, node_name: &str) -> BTreeMap<String, (NetworkKind, String)> {
    let Some(node) = cfg.nodes.get(node_name) else {
        return BTreeMap::new();
    };
    cfg.networks
        .iter()
        .filter(|(_, net)| matches!(net.kind, NetworkKind::Egress | NetworkKind::Wireguard))
        .filter_map(|(name, net)| match node.network.get(name) {
            Some(NodeAddress::Interface(iface)) => Some((name.clone(), (net.kind, iface.interface.clone()))),
            _ => None,
        })
        .collect()
}

pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    docker: Option<Docker>,
) -> Option<tokio::task::JoinHandle<()>> {
    let interfaces = interfaces_of(cfg, node_name);
    if interfaces.is_empty() {
        return None;
    }
    let docker = docker?;
    let cluster = cluster.clone();
    let node_name = node_name.to_string();
    // Only wireguard networks need it, to open their sealed config; loaded
    // once, like the materializer's own key, not reloaded every tick.
    let key = interfaces
        .values()
        .any(|(kind, _)| *kind == NetworkKind::Wireguard)
        .then(|| cfg.nodes.get(&node_name).and_then(|node| crate::daemon::load_cluster_key(cfg, node)))
        .flatten();
    let internal_addr = cfg.nodes.get(&node_name).and_then(|node| node.ip_on(cfg.internal_network().0));

    Some(tokio::spawn(async move {
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
        loop {
            ticker.tick().await;
            for (network, (kind, iface)) in &interfaces {
                if *kind == NetworkKind::Wireguard {
                    if let Err(e) = ensure_tunnel(&cluster, key.as_ref(), &node_name, network, iface).await {
                        warn!(network = %network, iface = %iface, error = %e, "cannot bring up the wireguard tunnel");
                        continue;
                    }
                }
                if let Err(e) = ensure_one(netlink.as_ref(), &docker, network, iface).await {
                    warn!(network = %network, iface = %iface, error = %e, "cannot bring up the egress network");
                }
            }

            if let Some(addr) = internal_addr {
                let cluster_state = cluster.state().await;
                let ingress_store = WireguardIngressStore::load(&cluster_state);
                for (network, (kind, iface)) in &interfaces {
                    if *kind != NetworkKind::Wireguard {
                        continue;
                    }
                    if let Err(e) = apply_ingress(iface, addr, network, &ingress_store).await {
                        warn!(network = %network, iface = %iface, error = %e, "cannot apply wireguard ingress rules");
                    }
                }
            }
        }
    }))
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
    let key = key.with_context(|| format!("this node holds no cluster key, cannot open the {network} config"))?;
    let plaintext = Sealed::from_blob(entry.sealed.clone())
        .open(key, &secret_name)
        .with_context(|| format!("cannot open the {network} wireguard config"))?;
    let text = String::from_utf8(plaintext).with_context(|| format!("{network} wireguard config is not utf8"))?;
    let wg_cfg: WireguardConfig =
        parse_wg_quick(&text).with_context(|| format!("invalid wireguard config for {network}"))?;

    let iface = iface.to_string();
    tokio::task::spawn_blocking(move || ensure_wireguard_interface(&iface, &wg_cfg))
        .await
        .context("wireguard interface task panicked")?
        .with_context(|| format!("bringing up the {network} tunnel"))?;
    Ok(())
}

async fn apply_ingress(iface: &str, addr: IpAddr, network: &str, store: &WireguardIngressStore) -> Result<()> {
    let rules: Vec<IngressRule> = store
        .rules
        .iter()
        .filter(|r| r.network == network)
        .map(|r| IngressRule { port: r.port, protocol: r.protocol, target_port: r.target_port })
        .collect();
    let plan = wireguard_ingress_iptables_plan(iface, addr, &rules);
    tokio::task::spawn_blocking(move || apply_wireguard_ingress_iptables(&plan))
        .await
        .context("iptables task panicked")??;
    Ok(())
}

async fn ensure_one(netlink: Option<&Overlay>, docker: &Docker, network: &str, iface: &str) -> Result<()> {
    let Some(netlink) = netlink else { return Ok(()) };

    let subnet = egress_subnet(network);
    let gateway = egress_gateway(subnet);
    let bridge = ensure_egress_bridge(netlink, network, subnet)
        .await
        .with_context(|| format!("ensuring the {network} egress bridge"))?;

    let plan = egress_iptables_plan(&bridge, subnet, iface);
    let bridge_for_blocking = bridge.clone();
    tokio::task::spawn_blocking(move || apply_egress_iptables(&plan))
        .await
        .context("iptables task panicked")?
        .with_context(|| format!("applying the kill-switch for {bridge_for_blocking}"))?;

    let created = docker
        .ensure_network(&NetworkSpec {
            name: egress_docker_name(network),
            bridge: bridge.clone(),
            subnet: subnet.to_string(),
            gateway,
            mtu: 1500,
            stack: OWNER.to_string(),
        })
        .await
        .with_context(|| format!("ensuring the {network} docker network"))?;
    if created {
        info!(network = %network, %subnet, %bridge, iface = %iface, "egress network up");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::{ClusterConfig, Network};

    fn cfg() -> ClusterConfig {
        let mut cfg =
            ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).expect("example parses");
        cfg.networks.insert(
            "vpn".to_string(),
            Network { kind: NetworkKind::Egress, subnet: None, domain: None },
        );
        cfg.networks.insert(
            "wg".to_string(),
            Network { kind: NetworkKind::Wireguard, subnet: None, domain: None },
        );
        cfg
    }

    #[test]
    fn a_node_with_an_interface_address_implements_the_network() {
        let mut cfg = cfg();
        cfg.nodes
            .get_mut("n1")
            .unwrap()
            .network
            .insert("vpn".to_string(), NodeAddress::Interface(cl_config::InterfaceAddress::new("wg0")));
        let interfaces = interfaces_of(&cfg, "n1");
        let (kind, iface) = interfaces.get("vpn").expect("vpn implemented");
        assert_eq!(*kind, NetworkKind::Egress);
        assert_eq!(iface, "wg0");
    }

    #[test]
    fn a_node_with_a_wireguard_address_implements_it_as_wireguard_kind() {
        let mut cfg = cfg();
        cfg.nodes
            .get_mut("n1")
            .unwrap()
            .network
            .insert("wg".to_string(), NodeAddress::Interface(cl_config::InterfaceAddress::new("wg-wg")));
        let interfaces = interfaces_of(&cfg, "n1");
        let (kind, iface) = interfaces.get("wg").expect("wg implemented");
        assert_eq!(*kind, NetworkKind::Wireguard);
        assert_eq!(iface, "wg-wg");
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
}
