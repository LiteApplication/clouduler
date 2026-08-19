//! Wiring egress networks into the daemon: for every egress-kind cluster
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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, NetworkKind, NodeAddress};
use cl_docker::{Docker, NetworkSpec};
use cl_net::egress::{
    apply_egress_iptables, egress_docker_name, egress_gateway, egress_iptables_plan, egress_subnet,
    ensure_egress_bridge,
};
use cl_net::vxlan::Overlay;
use cl_raft::node::ClusterNode;
use tracing::{info, warn};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Docker network label for cluster infrastructure that isn't owned by any
/// one stack, so a future per-stack sweep never mistakes it for orphaned.
const OWNER: &str = "_cluster";

/// Every egress network this node implements, network name -> host interface.
pub fn interfaces_of(cfg: &ClusterConfig, node_name: &str) -> BTreeMap<String, String> {
    let Some(node) = cfg.nodes.get(node_name) else {
        return BTreeMap::new();
    };
    cfg.networks
        .iter()
        .filter(|(_, net)| net.kind == NetworkKind::Egress)
        .filter_map(|(name, _)| match node.network.get(name) {
            Some(NodeAddress::Interface(iface)) => Some((name.clone(), iface.clone())),
            _ => None,
        })
        .collect()
}

pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    _cluster: &Arc<ClusterNode>,
    docker: Option<Docker>,
) -> Option<tokio::task::JoinHandle<()>> {
    let interfaces = interfaces_of(cfg, node_name);
    if interfaces.is_empty() {
        return None;
    }
    let docker = docker?;

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
            for (network, iface) in &interfaces {
                if let Err(e) = ensure_one(netlink.as_ref(), &docker, network, iface).await {
                    warn!(network = %network, iface = %iface, error = %e, "cannot bring up the egress network");
                }
            }
        }
    }))
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
        cfg
    }

    #[test]
    fn a_node_with_an_interface_address_implements_the_network() {
        let mut cfg = cfg();
        cfg.nodes
            .get_mut("n1")
            .unwrap()
            .network
            .insert("vpn".to_string(), NodeAddress::Interface("wg0".into()));
        let interfaces = interfaces_of(&cfg, "n1");
        assert_eq!(interfaces.get("vpn").map(String::as_str), Some("wg0"));
    }

    #[test]
    fn a_node_without_an_address_on_the_network_implements_nothing() {
        // n3 only has an address on tailscale in the example.
        assert!(interfaces_of(&cfg(), "n3").is_empty());
    }

    #[test]
    fn only_egress_kind_networks_are_considered() {
        // n2 has addresses on tailscale/lan/wan in the example, none egress.
        assert!(interfaces_of(&cfg(), "n2").is_empty());
    }

    #[test]
    fn an_unknown_node_implements_nothing() {
        assert!(interfaces_of(&cfg(), "does-not-exist").is_empty());
    }
}
