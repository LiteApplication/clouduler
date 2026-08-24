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

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_docker::{Docker, NetworkSpec};
use cl_net::vxlan::{allocate_vni, bridge_name, overlay_mtu, vxlan_name, Overlay};
use cl_net::ipam::{DEFAULT_OVERLAY_POOL, DEFAULT_SUBNET_PREFIX};
use cl_net::Allocations;
use cl_raft::ids::InstanceId;
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
) -> tokio::task::JoinHandle<()> {
    let ctx = Context_ {
        node: node_name.to_string(),
        cluster: cluster.clone(),
        docker,
        boot_cfg: cfg.clone(),
    };

    tokio::spawn(async move { ctx.run().await })
}

struct Context_ {
    node: String,
    cluster: Arc<ClusterNode>,
    docker: Option<Docker>,
    boot_cfg: ClusterConfig,
}

/// Every other node's address on the internal network, used to fill in the
/// VXLAN FDB. Recomputed on every reconcile tick from whatever config is
/// currently live, so a node that joins after this one started still gets an
/// FDB entry without a restart.
fn peers_of(cfg: &ClusterConfig, node_name: &str) -> BTreeSet<Ipv4Addr> {
    cfg.nodes
        .iter()
        .filter(|(name, _)| name.as_str() != node_name)
        .filter_map(|(_, node)| match cfg.internal_ip(node) {
            Some(IpAddr::V4(ip)) => Some(ip),
            _ => None,
        })
        .collect()
}

/// The subnets the overlay pool must not overlap, or a container's route to a
/// peer node collides with its route to another container.
fn cluster_subnets_of(cfg: &ClusterConfig) -> Vec<IpNet> {
    cfg.networks.values().filter_map(|n| n.subnet).collect()
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
        let cfg = crate::daemon::live_config(&state, &self.boot_cfg);

        // Recomputed from the live config every tick, not just once at boot,
        // so a node that gains an internal-network address, or a peer that
        // joins, is picked up without a restart.
        let internal_ip = match cfg.node(&self.node).and_then(|node| cfg.internal_ip(node)) {
            Some(IpAddr::V4(ip)) => ip,
            Some(IpAddr::V6(_)) => {
                // The VXLAN device is created with an IPv4 local address. An
                // IPv6 internal network is a real setup, just not one this
                // supports yet, and saying so beats failing later with a
                // netlink error.
                warn!("the internal network is ipv6, the overlay needs an ipv4 underlay");
                return Ok(());
            }
            None => {
                debug!("this node has no address on the internal network yet, skipping this pass");
                return Ok(());
            }
        };
        let peers = peers_of(&cfg, &self.node);
        let cluster_subnets = cluster_subnets_of(&cfg);

        // Every stack network the cluster knows about, with the instances on
        // it, and separately the ones with a container on this node. The
        // leader allocates for all of them, not just its own.
        let mut all: BTreeMap<String, BTreeSet<InstanceId>> = BTreeMap::new();
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
            let instances = all.entry(overlay.clone()).or_default();

            for assignment in state.assignments.values() {
                if assignment.instance.stack != *name {
                    continue;
                }
                instances.insert(assignment.instance.clone());
                if assignment.node == self.node {
                    mine.insert(overlay.clone());
                }
            }
        }

        let mut allocations = load_allocations(state.overlay.as_deref());
        if self.cluster.is_leader() {
            self.allocate_missing(&mut allocations, &all, &cluster_subnets).await?;
        }

        if let Err(e) = self.sweep_orphans(netlink, &all).await {
            // A leftover network is a slow-burn problem (it only bites the
            // next stack unlucky enough to be handed its subnet), not a
            // reason to skip bringing up the networks that are wanted now.
            warn!(error = %e, "sweeping orphaned overlay networks failed");
        }

        for overlay in &mine {
            if let Err(e) = self
                .ensure_network(netlink, &allocations, overlay, internal_ip, &peers)
                .await
            {
                // One broken network must not stop the others being built.
                warn!(network = %overlay, error = %e, "cannot bring up the overlay network");
            }
        }
        Ok(())
    }

    /// Tear down any network this daemon still has that no longer belongs to
    /// a live stack, on both docker's side and ours (bridge + VXLAN device).
    ///
    /// Left behind, an orphan keeps its subnet in docker's own IPAM forever;
    /// nothing else in the cluster remembers it, so the allocator can and
    /// eventually will hand that same subnet to a new stack network, which
    /// then fails to come up with a docker pool-overlap error.
    async fn sweep_orphans(&self, netlink: Option<&Overlay>, wanted: &BTreeMap<String, BTreeSet<InstanceId>>) -> Result<()> {
        let Some(docker) = &self.docker else {
            return Ok(());
        };
        for name in docker.list_managed_networks().await? {
            if wanted.contains_key(&name) {
                continue;
            }
            if let Err(e) = docker.remove_network(&name).await {
                // Still has containers attached, most likely: they haven't
                // been torn down yet, or this stale entry belongs to a
                // network name a live stack coincidentally reuses. Either
                // way, next tick is soon enough to try again.
                debug!(network = %name, error = %e, "orphaned overlay network not removed yet");
                continue;
            }
            if let Some(netlink) = netlink {
                netlink.teardown(&bridge_name(&name), &vxlan_name(&name)).await?;
            }
            info!(network = %name, "removed orphaned overlay network");
        }
        Ok(())
    }

    /// Give every known stack network a subnet and every instance on it an
    /// address, and publish the result.
    ///
    /// Addresses are decided here rather than left to docker because the rest
    /// of the cluster reads them out of this book: internal DNS answers,
    /// proxy backends and published ports all need to name a container that
    /// runs on some other node.
    async fn allocate_missing(
        &self,
        allocations: &mut Allocations,
        networks: &BTreeMap<String, BTreeSet<InstanceId>>,
        cluster_subnets: &[IpNet],
    ) -> Result<()> {
        let mut changed = false;

        // A network no longer belonging to any live stack frees its subnet
        // back to the pool, so it can be handed to a new stack instead of
        // sitting reserved forever for a name nothing will ever reconcile
        // again.
        let stale_networks: Vec<String> = allocations
            .network_names()
            .filter(|name| !networks.contains_key(*name))
            .map(str::to_string)
            .collect();
        for network in stale_networks {
            allocations.release_network(&network);
            changed = true;
            info!(network = %network, "released an overlay subnet no longer in use");
        }

        for (network, instances) in networks {
            if allocations.subnet(network).is_none() {
                allocations
                    .allocate_network(network, cluster_subnets)
                    .with_context(|| format!("allocating a subnet for {network}"))?;
                changed = true;
                info!(
                    network = %network,
                    subnet = ?allocations.subnet(network),
                    "allocated an overlay subnet"
                );
            }

            // An instance that is gone gives its address back, so a stack
            // redeployed often does not exhaust the subnet.
            let stale: Vec<InstanceId> = allocations
                .instances(network)
                .filter(|(id, _)| !instances.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();
            for instance in stale {
                allocations.release_ip(network, &instance);
                changed = true;
            }

            for instance in instances {
                if allocations.instance_ip(network, instance).is_some() {
                    continue;
                }
                let ip = allocations
                    .allocate_ip(network, instance)
                    .with_context(|| format!("allocating an address for {instance}"))?;
                changed = true;
                info!(network = %network, %instance, %ip, "allocated an overlay address");
            }
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
        internal_ip: Ipv4Addr,
        peers: &BTreeSet<Ipv4Addr>,
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
            .underlay_for(internal_ip)
            .await?
            .with_context(|| format!("no interface holds the internal address {internal_ip}"))?;
        let mtu = overlay_mtu(underlay.mtu, false);

        let bridge_index = netlink
            .ensure_bridge(&bridge, gateway, subnet.prefix_len(), mtu)
            .await?;

        // The VNI is derived from the network name rather than allocated, so
        // every node computes the same one without another round of consensus.
        let vni = allocate_vni(overlay, &BTreeSet::new());
        let vxlan_index = netlink
            .ensure_vxlan(&vxlan, vni, underlay.index, internal_ip, bridge_index, mtu)
            .await?;
        netlink.reconcile_fdb(&vxlan, vxlan_index, peers).await?;

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

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml")).expect("example parses")
    }

    /// `peers_of`/`cluster_subnets_of` are recomputed every reconcile tick from
    /// whatever config is currently live (see `Context_::reconcile`), rather
    /// than frozen once at boot - this is what lets a node added after this
    /// one started get an FDB entry without a restart. Confirms a node absent
    /// from one config but present in another is picked up as soon as it's in
    /// the config handed in.
    #[test]
    fn peers_of_reflects_a_newly_added_node() {
        let mut without_n3 = cfg();
        without_n3.nodes.remove("n3");
        let before = peers_of(&without_n3, "n1");
        assert!(!before.contains(&Ipv4Addr::new(10, 10, 0, 3)));

        let after = peers_of(&cfg(), "n1");
        assert!(after.contains(&Ipv4Addr::new(10, 10, 0, 3)));
    }

    #[test]
    fn peers_of_excludes_the_node_itself() {
        let peers = peers_of(&cfg(), "n1");
        assert!(!peers.contains(&Ipv4Addr::new(10, 10, 0, 1)));
        assert!(peers.contains(&Ipv4Addr::new(10, 10, 0, 2)));
    }

    #[test]
    fn cluster_subnets_of_lists_every_networks_subnet() {
        let subnets = cluster_subnets_of(&cfg());
        assert!(subnets.contains(&"10.10.0.0/24".parse().unwrap()));
        assert!(subnets.contains(&"192.168.1.0/24".parse().unwrap()));
    }
}
