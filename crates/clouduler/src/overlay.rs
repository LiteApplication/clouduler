//! Wiring the container overlay into the daemon.
//!
//! What has to be true before a container can start on a stack network:
//!
//! 1. the network has a subnet, allocated once for the whole cluster;
//! 2. this node has a bridge holding that subnet's gateway address, with a
//!    VXLAN device enslaved to it and an FDB entry per peer node;
//! 3. docker has a network attached to that bridge, so a container it places
//!    lands on the overlay rather than on a node-local island of its own.
//!
//! Step 1 is the leader's job, through raft, because two nodes allocating
//! independently would hand out the same subnet. Steps 2 and 3 are every
//! node's own job, and are idempotent: the loop re-runs them on a timer and on
//! restart, and doing them twice changes nothing.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_docker::{Docker, NetworkSpec};
use cl_net::vxlan::{allocate_vni, bridge_name, overlay_mtu, vxlan_name, Overlay};
use cl_net::ipam::{DEFAULT_OVERLAY_POOL, DEFAULT_SUBNET_PREFIX};
use cl_net::Allocations;
use cl_raft::node::ClusterNode;
use cl_raft::state::Request;
use cl_sched::run::overlay_name;
use ipnet::{IpNet, Ipv4Net};
use tracing::{debug, info, warn};

/// How often the loop re-checks. The overlay only changes when a stack is
/// deployed or a node joins, so this is a safety net behind those, not the
/// primary path.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

pub fn start(
    cfg: &ClusterConfig,
    node_name: &str,
    cluster: &Arc<ClusterNode>,
    docker: Option<Docker>,
) -> Option<tokio::task::JoinHandle<()>> {
    let internal_ip = match cfg.internal_ip(cfg.node(node_name)?) {
        Some(IpAddr::V4(ip)) => ip,
        Some(IpAddr::V6(_)) => {
            // The VXLAN device is created with an IPv4 local address. An IPv6
            // internal network is a real setup, just not one this supports yet,
            // and saying so beats failing later with a netlink error.
            warn!("the internal network is ipv6, the overlay needs an ipv4 underlay");
            return None;
        }
        None => return None,
    };

    let peers: BTreeSet<Ipv4Addr> = cfg
        .nodes
        .iter()
        .filter(|(name, _)| name.as_str() != node_name)
        .filter_map(|(_, node)| match cfg.internal_ip(node) {
            Some(IpAddr::V4(ip)) => Some(ip),
            _ => None,
        })
        .collect();

    // The overlay pool must not overlap anything the cluster already addresses,
    // or a container's route to a peer node collides with its route to another
    // container.
    let cluster_subnets: Vec<IpNet> = cfg.networks.values().filter_map(|n| n.subnet).collect();

    let ctx = Context_ {
        node: node_name.to_string(),
        cluster: cluster.clone(),
        docker,
        internal_ip,
        peers,
        cluster_subnets,
    };

    Some(tokio::spawn(async move { ctx.run().await }))
}

struct Context_ {
    node: String,
    cluster: Arc<ClusterNode>,
    docker: Option<Docker>,
    internal_ip: Ipv4Addr,
    peers: BTreeSet<Ipv4Addr>,
    cluster_subnets: Vec<IpNet>,
}

impl Context_ {
    async fn run(self) {
        // Netlink needs CAP_NET_ADMIN. Without it the overlay cannot be built
        // at all, so say so once and clearly rather than warning every tick.
        let netlink = match rtnetlink::new_connection() {
            Ok((connection, handle, _)) => {
                tokio::spawn(connection);
                Some(Overlay::new(handle))
            }
            Err(e) => {
                warn!(error = %e, "no netlink access, containers needing an overlay network will not start");
                None
            }
        };

        let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(e) = self.reconcile(netlink.as_ref()).await {
                warn!(error = %e, "overlay reconcile failed");
            }
        }
    }

    async fn reconcile(&self, netlink: Option<&Overlay>) -> Result<()> {
        let state = self.cluster.state().await;

        // Every stack network that has an instance assigned to this node, and
        // separately every one the cluster knows about, since the leader
        // allocates for all of them and not just its own.
        let mut all = BTreeSet::new();
        let mut mine = BTreeSet::new();

        for (name, stack) in &state.stacks {
            let Ok(file) = StackFile::parse(&stack.spec) else {
                continue;
            };
            // Matches how the scheduler picks a network for a container.
            let Some(network) = file.networks.keys().next() else {
                continue;
            };
            let overlay = overlay_name(name, network);
            all.insert(overlay.clone());

            let runs_here = state
                .assignments
                .values()
                .any(|a| a.instance.stack == *name && a.node == self.node);
            if runs_here {
                mine.insert(overlay);
            }
        }

        if all.is_empty() {
            return Ok(());
        }

        let mut allocations = load_allocations(state.overlay.as_deref());
        if self.cluster.is_leader() {
            self.allocate_missing(&mut allocations, &all).await?;
        }

        for overlay in &mine {
            if let Err(e) = self.ensure_network(netlink, &allocations, overlay).await {
                // One broken network must not stop the others being built.
                warn!(network = %overlay, error = %e, "cannot bring up the overlay network");
            }
        }
        Ok(())
    }

    /// Give every known stack network a subnet, and publish the result.
    async fn allocate_missing(
        &self,
        allocations: &mut Allocations,
        networks: &BTreeSet<String>,
    ) -> Result<()> {
        let mut changed = false;
        for network in networks {
            if allocations.subnet(network).is_some() {
                continue;
            }
            allocations
                .allocate_network(network, &self.cluster_subnets)
                .with_context(|| format!("allocating a subnet for {network}"))?;
            changed = true;
            info!(
                network = %network,
                subnet = ?allocations.subnet(network),
                "allocated an overlay subnet"
            );
        }

        if !changed {
            return Ok(());
        }

        let encoded = serde_json::to_string(allocations).context("encoding the overlay allocations")?;
        let response = self
            .cluster
            .submit(Request::SetOverlayAllocations { allocations: encoded })
            .await?;
        if let Some(error) = response.error {
            warn!(%error, "could not publish the overlay allocations");
        }
        Ok(())
    }

    /// Bring one network up on this node: bridge, vxlan, fdb, docker network.
    async fn ensure_network(
        &self,
        netlink: Option<&Overlay>,
        allocations: &Allocations,
        overlay: &str,
    ) -> Result<()> {
        // Nothing to do until the leader has allocated. On a follower that just
        // means waiting a tick.
        let (Some(subnet), Some(gateway)) =
            (allocations.subnet(overlay), allocations.gateway(overlay))
        else {
            debug!(network = %overlay, "no subnet allocated yet");
            return Ok(());
        };

        let bridge = bridge_name(overlay);
        let vxlan = vxlan_name(overlay);

        let Some(netlink) = netlink else {
            return Ok(());
        };

        let underlay = netlink
            .underlay_for(self.internal_ip)
            .await?
            .with_context(|| format!("no interface holds the internal address {}", self.internal_ip))?;
        let mtu = overlay_mtu(underlay.mtu, false);

        let bridge_index = netlink
            .ensure_bridge(&bridge, gateway, subnet.prefix_len(), mtu)
            .await?;

        // The VNI is derived from the network name rather than allocated, so
        // every node computes the same one without another round of consensus.
        let vni = allocate_vni(overlay, &BTreeSet::new());
        let vxlan_index = netlink
            .ensure_vxlan(&vxlan, vni, underlay.index, self.internal_ip, bridge_index, mtu)
            .await?;
        netlink.reconcile_fdb(&vxlan, vxlan_index, &self.peers).await?;

        let Some(docker) = &self.docker else {
            return Ok(());
        };
        let stack = overlay.split('_').next().unwrap_or(overlay).to_string();
        let created = docker
            .ensure_network(&NetworkSpec {
                name: overlay.to_string(),
                bridge: bridge.clone(),
                subnet: subnet.to_string(),
                gateway,
                mtu,
                stack,
            })
            .await?;
        if created {
            info!(network = %overlay, %subnet, %bridge, vni, mtu, "overlay network up");
        }
        Ok(())
    }
}

/// Decode the replicated address book, falling back to a fresh one.
///
/// A decode failure means the stored value is from an incompatible version.
/// Starting over is the only option, and it is survivable: the addresses are
/// re-derived, and containers pick them up on their next restart.
fn load_allocations(stored: Option<&str>) -> Allocations {
    let pool: Ipv4Net = DEFAULT_OVERLAY_POOL.parse().expect("the default pool parses");
    match stored {
        Some(raw) => serde_json::from_str(raw).unwrap_or_else(|e| {
            warn!(error = %e, "overlay allocations could not be decoded, starting a fresh address book");
            Allocations::new(pool, DEFAULT_SUBNET_PREFIX)
        }),
        None => Allocations::new(pool, DEFAULT_SUBNET_PREFIX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_address_book_starts_empty_rather_than_failing() {
        let allocations = load_allocations(None);
        assert!(allocations.subnet("blog_default").is_none());
    }

    #[test]
    fn a_stored_address_book_round_trips() {
        let mut original = load_allocations(None);
        original.allocate_network("blog_default", &[]).expect("allocates");
        let encoded = serde_json::to_string(&original).unwrap();

        let restored = load_allocations(Some(&encoded));
        assert_eq!(restored.subnet("blog_default"), original.subnet("blog_default"));
    }

    /// A corrupt or incompatible stored value must not wedge the daemon: the
    /// addresses can be re-derived, a node that refuses to start cannot.
    #[test]
    fn an_undecodable_address_book_falls_back_instead_of_panicking() {
        let allocations = load_allocations(Some("{not json"));
        assert!(allocations.subnet("blog_default").is_none());
    }

    /// Every node has to derive the same VNI for a network, or their VXLAN
    /// devices are on different segments and nothing reaches anything.
    #[test]
    fn the_vni_is_derived_from_the_name_so_every_node_agrees() {
        let a = allocate_vni("blog_default", &BTreeSet::new());
        let b = allocate_vni("blog_default", &BTreeSet::new());
        assert_eq!(a, b);
        assert_ne!(a, allocate_vni("shop_default", &BTreeSet::new()));
    }
}
