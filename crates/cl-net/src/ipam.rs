//! Overlay address management.
//!
//! Everything in this module is pure: no netlink, no IO. `Allocations` is the
//! piece of state the scheduler would replicate through raft (see the crate
//! docs for which `cl_raft::Request` variants that needs), so it has to be
//! deterministic, idempotent and serde-friendly.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::Ipv4Addr;

use cl_raft::InstanceId;
use ipnet::{IpNet, Ipv4Net};
use serde::{Deserialize, Serialize};

use crate::error::{NetError, Result};

/// Default overlay pool, carved into per-network `/24`s. Away from common RFC1918 LAN ranges so the default never needs touching.
pub const DEFAULT_OVERLAY_POOL: &str = "10.244.0.0/16";

pub const DEFAULT_OVERLAY_NET: Ipv4Net = Ipv4Net::new_assert(Ipv4Addr::new(10, 244, 0, 0), 16);

pub const DEFAULT_SUBNET_PREFIX: u8 = 24;

/// Prefix of the allocation key of a managed cluster network: `@<network>`.
/// Stack overlays are `<stack>_<network>` and container segments `CTN_...`, so
/// the three key spaces never collide, and a managed network's subnet is its
/// own rather than a slice of the pool.
pub const MANAGED_KEY_PREFIX: char = '@';

/// The allocation key of managed cluster network `network`.
pub fn managed_key(network: &str) -> String {
    format!("{MANAGED_KEY_PREFIX}{network}")
}

/// The docker network name of managed cluster network `network` (docker names
/// cannot start with `@`). Never starts like a stack overlay (`<stack>_..`
/// is lowercase), so it cannot collide with one, and is exempt from the
/// orphan sweep.
pub fn managed_docker_name(network: &str) -> String {
    format!("{MANAGED_DOCKER_PREFIX}{network}")
}

/// Prefix of [`managed_docker_name`].
pub const MANAGED_DOCKER_PREFIX: &str = "MNG_";

/// Whether an allocation key names a managed cluster network.
pub fn is_managed_key(key: &str) -> bool {
    key.starts_with(MANAGED_KEY_PREFIX)
}

/// What [`Allocations::sync_managed_network`] changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ManagedSync {
    /// Whether the book changed at all (so it needs publishing).
    pub changed: bool,
    /// Instances that lost their address because it is no longer usable (the
    /// subnet changed or a node took the address); they get a new one on the
    /// next `allocate_ip`.
    pub evicted: Vec<InstanceId>,
}

/// Address book for the container overlay: one subnet per stack network, one
/// IP per instance within it. Replicated through raft so two nodes can never
/// hand out the same address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Allocations {
    pool: Ipv4Net,
    subnet_prefix: u8,
    networks: BTreeMap<String, NetworkAllocation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct NetworkAllocation {
    subnet: Ipv4Net,
    instances: BTreeMap<InstanceId, Ipv4Addr>,
    reserved: BTreeSet<Ipv4Addr>,
    /// Released addresses, oldest first. Kept out of rotation while any
    /// address that was never used is still available, so a restarting
    /// container doesn't immediately reuse an address that may still have
    /// live conntrack or ARP cache entries elsewhere on the overlay.
    cooldown: VecDeque<Ipv4Addr>,
    /// Per-node addresses on this network (container-kind segments give each
    /// node a host port). They live in `reserved`/`cooldown` like instance
    /// addresses, so the two kinds can never collide. Absent in books written
    /// before this field existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    nodes: BTreeMap<String, Ipv4Addr>,
    /// Addresses that are never handed out (a managed network's node
    /// addresses). Absent in books written before this field existed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    excluded: BTreeSet<Ipv4Addr>,
}

impl Allocations {
    /// A fresh allocator over `pool`, sliced into `subnet_prefix`-bit subnets (>= the pool prefix, <= 32).
    pub fn new(pool: Ipv4Net, subnet_prefix: u8) -> Self {
        Self {
            pool,
            subnet_prefix,
            networks: BTreeMap::new(),
        }
    }

    /// Decode a replicated address book.
    ///
    /// `None` covers both "nothing allocated yet" and "the stored value is
    /// from an incompatible version". Callers treat the two the same way:
    /// there is no address to hand out yet, so wait for the leader to write
    /// one rather than guessing.
    pub fn decode(stored: Option<&str>) -> Option<Self> {
        serde_json::from_str(stored?).ok()
    }

    pub fn instances(&self, network: &str) -> impl Iterator<Item = (&InstanceId, Ipv4Addr)> {
        self.networks
            .get(network)
            .into_iter()
            .flat_map(|n| n.instances.iter().map(|(id, ip)| (id, *ip)))
    }

    pub fn gateways(&self) -> impl Iterator<Item = (&str, Ipv4Addr)> {
        self.networks
            .iter()
            .map(|(name, n)| (name.as_str(), gateway_of(n.subnet)))
    }

    pub fn subnet(&self, network: &str) -> Option<Ipv4Net> {
        self.networks.get(network).map(|n| n.subnet)
    }

    pub fn network_names(&self) -> impl Iterator<Item = &str> {
        self.networks.keys().map(|s| s.as_str())
    }

    /// Subnets of the given overlays, by exact name (an overlay is named
    /// `<stack>_<cluster network>`, see `cl_sched::run::overlay_name`). Names
    /// with no allocation are skipped.
    ///
    /// The caller derives the overlays from cluster state rather than parsing
    /// names: both halves may contain `_`, so a suffix match cannot tell stack `x`
    /// on network `a_b` from stack `x_a` on network `b`.
    ///
    /// A client and a same-node backend on one cluster network can be delivered
    /// locally by the kernel, so the proxy sees the client's raw overlay address
    /// instead of one in the cluster network's subnet. Containers on these subnets
    /// were placed by clouduler and are trusted.
    pub fn subnets_of_overlays<'a>(
        &'a self,
        overlays: impl IntoIterator<Item = &'a str> + 'a,
    ) -> impl Iterator<Item = Ipv4Net> + 'a {
        overlays.into_iter().filter_map(|name| self.subnet(name))
    }

    pub fn gateway(&self, network: &str) -> Option<Ipv4Addr> {
        self.subnet(network).map(gateway_of)
    }

    pub fn instance_ip(&self, network: &str, instance: &InstanceId) -> Option<Ipv4Addr> {
        self.networks.get(network)?.instances.get(instance).copied()
    }

    /// Claim a subnet out of the pool, skipping candidates that overlap another stack network or a declared cluster network (so the overlay never shadows the internal or a private one). Idempotent.
    pub fn allocate_network(
        &mut self,
        network: &str,
        cluster_subnets: &[IpNet],
    ) -> Result<Ipv4Net> {
        self.allocate_network_in(network, None, cluster_subnets)
    }

    /// Like [`allocate_network`](Self::allocate_network) but claims exactly `requested`, failing on overlap instead of picking another.
    pub fn allocate_network_in(
        &mut self,
        network: &str,
        requested: Option<Ipv4Net>,
        cluster_subnets: &[IpNet],
    ) -> Result<Ipv4Net> {
        if let Some(existing) = self.subnet(network) {
            return Ok(existing);
        }

        let claimed: Vec<Ipv4Net> = self.networks.values().map(|n| n.subnet).collect();
        let cluster_v4: Vec<Ipv4Net> = cluster_subnets
            .iter()
            .filter_map(|n| match n {
                IpNet::V4(v4) => Some(*v4),
                IpNet::V6(_) => None,
            })
            .collect();

        if let Some(want) = requested {
            let conflict = claimed
                .iter()
                .chain(cluster_v4.iter())
                .find(|c| nets_overlap(**c, want));
            if let Some(conflict) = conflict {
                return Err(NetError::SubnetOverlap {
                    network: network.to_string(),
                    subnet: want.to_string(),
                    conflicts: conflict.to_string(),
                });
            }
            self.networks.insert(
                network.to_string(),
                NetworkAllocation {
                    subnet: want,
                    instances: BTreeMap::new(),
                    reserved: BTreeSet::new(),
                    cooldown: VecDeque::new(),
                    nodes: BTreeMap::new(),
                    excluded: BTreeSet::new(),
                },
            );
            return Ok(want);
        }

        let candidates =
            self.pool
                .subnets(self.subnet_prefix)
                .map_err(|_| NetError::SubnetExhausted {
                    pool: self.pool.to_string(),
                })?;

        for candidate in candidates {
            let overlaps_claimed = claimed.iter().any(|c| nets_overlap(*c, candidate));
            let overlaps_cluster = cluster_v4.iter().find(|c| nets_overlap(**c, candidate));
            if overlaps_claimed {
                continue;
            }
            if let Some(conflict) = overlaps_cluster {
                // Skip past this one and keep looking; it isn't a fatal
                // error unless the whole pool is unusable.
                let _ = conflict;
                continue;
            }
            self.networks.insert(
                network.to_string(),
                NetworkAllocation {
                    subnet: candidate,
                    instances: BTreeMap::new(),
                    reserved: BTreeSet::new(),
                    cooldown: VecDeque::new(),
                    nodes: BTreeMap::new(),
                    excluded: BTreeSet::new(),
                },
            );
            return Ok(candidate);
        }

        Err(NetError::SubnetExhausted {
            pool: self.pool.to_string(),
        })
    }

    /// Make the book hold managed network `key` (see [`managed_key`]) over its
    /// own `subnet`, never handing out any of `excluded` (node addresses; the
    /// network, broadcast and gateway addresses are always excluded). Creates the
    /// entry, or when the subnet changed replaces it, or when `excluded`
    /// changed updates it and evicts any instance sitting on a now excluded
    /// address. Idempotent.
    pub fn sync_managed_network(
        &mut self,
        key: &str,
        subnet: Ipv4Net,
        excluded: &BTreeSet<Ipv4Addr>,
    ) -> ManagedSync {
        let mut out = ManagedSync::default();
        let fresh = |excluded: &BTreeSet<Ipv4Addr>| NetworkAllocation {
            subnet,
            instances: BTreeMap::new(),
            reserved: BTreeSet::new(),
            cooldown: VecDeque::new(),
            nodes: BTreeMap::new(),
            excluded: excluded.clone(),
        };
        match self.networks.get_mut(key) {
            None => {
                self.networks.insert(key.to_string(), fresh(excluded));
                out.changed = true;
            }
            Some(net) if net.subnet != subnet => {
                out.evicted = net.instances.keys().cloned().collect();
                *net = fresh(excluded);
                out.changed = true;
            }
            Some(net) if net.excluded != *excluded => {
                net.excluded = excluded.clone();
                let hit: Vec<InstanceId> = net
                    .instances
                    .iter()
                    .filter(|(_, ip)| excluded.contains(ip))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in hit {
                    if let Some(ip) = net.instances.remove(&id) {
                        net.reserved.remove(&ip);
                    }
                    out.evicted.push(id);
                }
                net.cooldown.retain(|ip| !excluded.contains(ip));
                out.changed = true;
            }
            Some(_) => {}
        }
        out
    }

    /// Move a managed network's allocation to a new key (the network was
    /// renamed), keeping its subnet and every instance address. False, and
    /// nothing changed, when `from` is unknown or `to` already exists.
    pub fn rename_managed_network(&mut self, from: &str, to: &str) -> bool {
        if from == to || self.networks.contains_key(to) || !self.networks.contains_key(from) {
            return false;
        }
        let allocation = self.networks.remove(from).expect("checked above");
        self.networks.insert(to.to_string(), allocation);
        true
    }

    /// Drop a network's allocation, freeing its subnet and every instance address.
    pub fn release_network(&mut self, network: &str) {
        self.networks.remove(network);
    }

    /// Allocate an instance address in the network's subnet. Idempotent; `.1` is the gateway.
    pub fn allocate_ip(&mut self, network: &str, instance: &InstanceId) -> Result<Ipv4Addr> {
        let net = self
            .networks
            .get_mut(network)
            .ok_or_else(|| NetError::UnknownNetwork(network.to_string()))?;

        if let Some(existing) = net.instances.get(instance) {
            return Ok(*existing);
        }

        let chosen = net.pick_address(network)?;
        net.instances.insert(instance.clone(), chosen);
        Ok(chosen)
    }

    pub fn node_ip(&self, network: &str, node: &str) -> Option<Ipv4Addr> {
        self.networks.get(network)?.nodes.get(node).copied()
    }

    pub fn node_ips(&self, network: &str) -> impl Iterator<Item = (&str, Ipv4Addr)> {
        self.networks
            .get(network)
            .into_iter()
            .flat_map(|n| n.nodes.iter().map(|(node, ip)| (node.as_str(), *ip)))
    }

    /// Allocate a node's address on a network; same rules as `allocate_ip`.
    pub fn allocate_node_ip(&mut self, network: &str, node: &str) -> Result<Ipv4Addr> {
        let net = self
            .networks
            .get_mut(network)
            .ok_or_else(|| NetError::UnknownNetwork(network.to_string()))?;
        if let Some(existing) = net.nodes.get(node) {
            return Ok(*existing);
        }
        let chosen = net.pick_address(network)?;
        net.nodes.insert(node.to_string(), chosen);
        Ok(chosen)
    }

    /// Release a node's address to the back of the cooldown queue; no-op if none.
    pub fn release_node_ip(&mut self, network: &str, node: &str) {
        let Some(net) = self.networks.get_mut(network) else {
            return;
        };
        let Some(ip) = net.nodes.remove(node) else {
            return;
        };
        net.reserved.remove(&ip);
        net.cooldown.push_back(ip);
    }

    /// Release an instance's address to the back of the cooldown queue; no-op if none.
    pub fn release_ip(&mut self, network: &str, instance: &InstanceId) {
        let Some(net) = self.networks.get_mut(network) else {
            return;
        };
        let Some(ip) = net.instances.remove(instance) else {
            return;
        };
        net.reserved.remove(&ip);
        net.cooldown.push_back(ip);
    }
}

impl NetworkAllocation {
    /// Reserve a never-used address if any is left, else the oldest released one. Never the gateway.
    fn pick_address(&mut self, network: &str) -> Result<Ipv4Addr> {
        let subnet = self.subnet;
        let gateway = gateway_of(subnet);
        // Prefer an address nobody has ever held: walk the host range in
        // order, skipping the gateway, anything reserved right now, and
        // anything sitting in the cooldown queue.
        let fresh = subnet.hosts().find(|ip| {
            *ip != gateway
                && !self.excluded.contains(ip)
                && !self.reserved.contains(ip)
                && !self.cooldown.contains(ip)
        });
        let chosen = if let Some(ip) = fresh {
            ip
        } else if let Some(ip) = self.cooldown.pop_front() {
            // Nothing fresh left: fall back to the oldest released address.
            ip
        } else {
            return Err(NetError::AddressExhausted {
                network: network.to_string(),
                subnet: subnet.to_string(),
            });
        };
        self.reserved.insert(chosen);
        Ok(chosen)
    }
}

impl Default for Allocations {
    fn default() -> Self {
        Self::new(DEFAULT_OVERLAY_NET, DEFAULT_SUBNET_PREFIX)
    }
}

fn gateway_of(subnet: Ipv4Net) -> Ipv4Addr {
    let net_addr: u32 = subnet.network().into();
    Ipv4Addr::from(net_addr.wrapping_add(1))
}

/// Two CIDR blocks overlap iff one contains the other's network address:
/// aligned blocks are always either disjoint or nested.
fn nets_overlap(a: Ipv4Net, b: Ipv4Net) -> bool {
    a.contains(&b.network()) || b.contains(&a.network())
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_overlay_net_matches_its_string_form() {
        assert_eq!(
            super::DEFAULT_OVERLAY_POOL
                .parse::<ipnet::Ipv4Net>()
                .unwrap(),
            super::DEFAULT_OVERLAY_NET
        );
    }

    use super::*;

    #[test]
    fn a_stored_book_decodes_and_a_broken_one_does_not() {
        let mut a = Allocations::new("10.244.0.0/16".parse().unwrap(), 24);
        a.allocate_network("gitlab", &[]).unwrap();
        let encoded = serde_json::to_string(&a).unwrap();

        assert_eq!(Allocations::decode(Some(&encoded)), Some(a));
        assert_eq!(Allocations::decode(Some("{not json")), None);
        assert_eq!(Allocations::decode(None), None);
    }

    #[test]
    fn instances_and_gateways_are_readable_back_out() {
        let mut a = Allocations::new("10.244.0.0/16".parse().unwrap(), 24);
        a.allocate_network("gitlab", &[]).unwrap();
        let id = InstanceId::new("gitlab", "web", 0);
        let ip = a.allocate_ip("gitlab", &id).unwrap();

        assert_eq!(a.instances("gitlab").collect::<Vec<_>>(), vec![(&id, ip)]);
        assert_eq!(
            a.gateways().collect::<Vec<_>>(),
            vec![("gitlab", a.gateway("gitlab").unwrap())]
        );
        assert_eq!(a.instances("nothing").count(), 0);
    }

    fn instance(n: u32) -> InstanceId {
        InstanceId::new("gitlab", "web", n)
    }

    #[test]
    fn allocates_a_subnet_per_network() {
        let mut a = Allocations::default();
        let s1 = a.allocate_network("gitlab", &[]).unwrap();
        let s2 = a.allocate_network("monitoring", &[]).unwrap();
        assert_ne!(s1, s2);
        assert_eq!(s1.prefix_len(), DEFAULT_SUBNET_PREFIX);
    }

    #[test]
    fn a_requested_subnet_is_claimed_and_collisions_are_refused() {
        let mut a = Allocations::default();
        let want: Ipv4Net = "172.30.5.0/24".parse().unwrap();
        assert_eq!(a.allocate_network_in("x", Some(want), &[]).unwrap(), want);
        let inner: Ipv4Net = "172.30.5.128/25".parse().unwrap();
        assert!(matches!(
            a.allocate_network_in("y", Some(inner), &[]),
            Err(NetError::SubnetOverlap { .. })
        ));
        let cluster: IpNet = "192.168.1.0/24".parse().unwrap();
        let clash: Ipv4Net = "192.168.1.0/25".parse().unwrap();
        assert!(a.allocate_network_in("z", Some(clash), &[cluster]).is_err());
        // Auto allocation steers clear of the pinned one.
        let auto = a.allocate_network("w", &[]).unwrap();
        assert!(!nets_overlap(auto, want));
    }

    #[test]
    fn network_allocation_is_idempotent() {
        let mut a = Allocations::default();
        let s1 = a.allocate_network("gitlab", &[]).unwrap();
        let s2 = a.allocate_network("gitlab", &[]).unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn gateway_is_first_host_address() {
        let mut a = Allocations::default();
        let subnet = a.allocate_network("gitlab", &[]).unwrap();
        let gw = a.gateway("gitlab").unwrap();
        assert_eq!(gw, gateway_of(subnet));
        assert_eq!(gw.octets()[3], 1);
    }

    #[test]
    fn subnets_of_overlays_looks_up_exact_names() {
        let mut a = Allocations::default();
        let uptime = a.allocate_network("uptime-kuma_tailscale", &[]).unwrap();
        let obsurwatch = a.allocate_network("obsurwatch_tailscale", &[]).unwrap();
        a.allocate_network("gitlab_backups", &[]).unwrap();

        let got: Vec<Ipv4Net> = a
            .subnets_of_overlays(["uptime-kuma_tailscale", "obsurwatch_tailscale"])
            .collect();
        assert_eq!(got, vec![uptime, obsurwatch]);
    }

    #[test]
    fn subnets_of_overlays_skips_unallocated_and_does_not_suffix_match() {
        let mut a = Allocations::default();
        a.allocate_network("x_a_b", &[]).unwrap();
        // Stack `x` on network `a_b` must not be found when asking for
        // stack `x_a` on network `b`'s overlay, or anything shorter.
        assert_eq!(a.subnets_of_overlays(["x_a_b"]).count(), 1);
        assert_eq!(a.subnets_of_overlays(["_b", "b", "a_b"]).count(), 0);
    }

    #[test]
    fn subnet_allocation_skips_cluster_overlap() {
        // Pool of exactly two /25s; the first collides with a declared
        // cluster network, so the network must land on the second.
        let pool: Ipv4Net = "10.244.0.0/24".parse().unwrap();
        let mut a = Allocations::new(pool, 25);
        let cluster: IpNet = "10.244.0.0/25".parse().unwrap();
        let subnet = a.allocate_network("gitlab", &[cluster]).unwrap();
        assert_eq!(subnet, "10.244.0.128/25".parse().unwrap());
    }

    #[test]
    fn subnet_pool_exhaustion_is_reported() {
        let pool: Ipv4Net = "10.244.0.0/24".parse().unwrap();
        let mut a = Allocations::new(pool, 25);
        a.allocate_network("one", &[]).unwrap();
        a.allocate_network("two", &[]).unwrap();
        let err = a.allocate_network("three", &[]).unwrap_err();
        assert!(matches!(err, NetError::SubnetExhausted { .. }));
    }

    #[test]
    fn releasing_a_network_frees_its_subnet_for_reuse() {
        let pool: Ipv4Net = "10.244.0.0/24".parse().unwrap();
        let mut a = Allocations::new(pool, 25);
        let s1 = a.allocate_network("one", &[]).unwrap();
        a.release_network("one");
        let s2 = a.allocate_network("two", &[]).unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn network_names_lists_every_allocated_network() {
        let mut a = Allocations::default();
        a.allocate_network("one", &[]).unwrap();
        a.allocate_network("two", &[]).unwrap();
        let names: BTreeSet<&str> = a.network_names().collect();
        assert_eq!(names, BTreeSet::from(["one", "two"]));
    }

    #[test]
    fn ip_allocation_is_idempotent() {
        let mut a = Allocations::default();
        a.allocate_network("gitlab", &[]).unwrap();
        let ip1 = a.allocate_ip("gitlab", &instance(0)).unwrap();
        let ip2 = a.allocate_ip("gitlab", &instance(0)).unwrap();
        assert_eq!(ip1, ip2);
    }

    #[test]
    fn ip_allocation_skips_the_gateway() {
        let mut a = Allocations::default();
        let gw = {
            a.allocate_network("gitlab", &[]).unwrap();
            a.gateway("gitlab").unwrap()
        };
        for i in 0..20 {
            let ip = a.allocate_ip("gitlab", &instance(i)).unwrap();
            assert_ne!(ip, gw);
        }
    }

    #[test]
    fn different_instances_get_different_addresses() {
        let mut a = Allocations::default();
        a.allocate_network("gitlab", &[]).unwrap();
        let ip1 = a.allocate_ip("gitlab", &instance(0)).unwrap();
        let ip2 = a.allocate_ip("gitlab", &instance(1)).unwrap();
        assert_ne!(ip1, ip2);
    }

    #[test]
    fn allocating_on_unknown_network_fails() {
        let mut a = Allocations::default();
        let err = a.allocate_ip("ghost", &instance(0)).unwrap_err();
        assert!(matches!(err, NetError::UnknownNetwork(n) if n == "ghost"));
    }

    #[test]
    fn address_exhaustion_is_reported() {
        // /30 has exactly one usable host address once the gateway is reserved.
        let pool: Ipv4Net = "10.244.0.0/30".parse().unwrap();
        let mut a = Allocations::new(pool, 30);
        a.allocate_network("gitlab", &[]).unwrap();
        a.allocate_ip("gitlab", &instance(0)).unwrap();
        let err = a.allocate_ip("gitlab", &instance(1)).unwrap_err();
        assert!(matches!(err, NetError::AddressExhausted { .. }));
    }

    #[test]
    fn released_address_is_not_reused_while_fresh_ones_remain() {
        // /29: hosts .1 (gateway) through .6, five usable after the gateway.
        let pool: Ipv4Net = "10.244.0.0/29".parse().unwrap();
        let mut a = Allocations::new(pool, 29);
        a.allocate_network("gitlab", &[]).unwrap();

        let first = a.allocate_ip("gitlab", &instance(0)).unwrap();
        a.release_ip("gitlab", &instance(0));

        let second = a.allocate_ip("gitlab", &instance(1)).unwrap();
        assert_ne!(
            first, second,
            "must not immediately hand back a just-released address"
        );
    }

    #[test]
    fn cooldown_address_is_reused_once_the_subnet_is_otherwise_full() {
        let pool: Ipv4Net = "10.244.0.0/29".parse().unwrap();
        let mut a = Allocations::new(pool, 29);
        a.allocate_network("gitlab", &[]).unwrap();

        // Fill every usable address (.2 through .6).
        for i in 0..5 {
            a.allocate_ip("gitlab", &instance(i)).unwrap();
        }
        let released = a.instance_ip("gitlab", &instance(0)).unwrap();
        a.release_ip("gitlab", &instance(0));

        // Nothing fresh is left, so the next allocation must fall back to
        // the address that just cooled down.
        let reused = a.allocate_ip("gitlab", &instance(5)).unwrap();
        assert_eq!(reused, released);
    }

    #[test]
    fn releasing_an_unknown_instance_is_a_no_op() {
        let mut a = Allocations::default();
        a.allocate_network("gitlab", &[]).unwrap();
        a.release_ip("gitlab", &instance(0)); // never allocated
        a.release_ip("ghost-network", &instance(0)); // unknown network
    }

    #[test]
    fn allocations_round_trip_through_json() {
        let mut a = Allocations::default();
        a.allocate_network("gitlab", &[]).unwrap();
        a.allocate_ip("gitlab", &instance(0)).unwrap();
        a.allocate_ip("gitlab", &instance(1)).unwrap();
        a.release_ip("gitlab", &instance(0));

        let encoded = serde_json::to_vec(&a).unwrap();
        let decoded: Allocations = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(a, decoded);
    }

    #[test]
    fn node_addresses_never_collide_with_instances_or_the_gateway() {
        let mut a = Allocations::default();
        a.allocate_network("CTN_tn", &[]).unwrap();
        let gw = a.gateway("CTN_tn").unwrap();
        let n1 = a.allocate_node_ip("CTN_tn", "n1").unwrap();
        let i0 = a.allocate_ip("CTN_tn", &instance(0)).unwrap();
        let n2 = a.allocate_node_ip("CTN_tn", "n2").unwrap();
        let i1 = a.allocate_ip("CTN_tn", &instance(1)).unwrap();
        let all = [n1, i0, n2, i1];
        assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), 4);
        assert!(!all.contains(&gw));
        assert_eq!(a.node_ip("CTN_tn", "n1"), Some(n1));
        assert_eq!(
            a.node_ips("CTN_tn").collect::<Vec<_>>(),
            vec![("n1", n1), ("n2", n2)]
        );
    }

    #[test]
    fn node_allocation_is_idempotent_and_unknown_networks_fail() {
        let mut a = Allocations::default();
        a.allocate_network("tn", &[]).unwrap();
        let ip = a.allocate_node_ip("tn", "n1").unwrap();
        assert_eq!(a.allocate_node_ip("tn", "n1").unwrap(), ip);
        assert!(matches!(
            a.allocate_node_ip("ghost", "n1"),
            Err(NetError::UnknownNetwork(_))
        ));
        assert_eq!(a.node_ip("ghost", "n1"), None);
        assert_eq!(a.node_ips("ghost").count(), 0);
    }

    #[test]
    fn node_addresses_share_cooldown_and_exhaustion_with_instances() {
        // /30: one usable address besides the gateway.
        let pool: Ipv4Net = "10.244.0.0/30".parse().unwrap();
        let mut a = Allocations::new(pool, 30);
        a.allocate_network("tn", &[]).unwrap();
        let ip = a.allocate_node_ip("tn", "n1").unwrap();
        assert!(matches!(
            a.allocate_ip("tn", &instance(0)),
            Err(NetError::AddressExhausted { .. })
        ));
        assert!(matches!(
            a.allocate_node_ip("tn", "n2"),
            Err(NetError::AddressExhausted { .. })
        ));
        // Released, it cools down and is reused once nothing fresh is left.
        a.release_node_ip("tn", "n1");
        assert_eq!(a.node_ip("tn", "n1"), None);
        assert_eq!(a.allocate_ip("tn", &instance(0)).unwrap(), ip);
        a.release_node_ip("tn", "never-allocated");
    }

    #[test]
    fn a_released_node_address_is_not_reused_while_fresh_ones_remain() {
        let mut a = Allocations::default();
        a.allocate_network("tn", &[]).unwrap();
        let first = a.allocate_node_ip("tn", "n1").unwrap();
        a.release_node_ip("tn", "n1");
        assert_ne!(a.allocate_node_ip("tn", "n2").unwrap(), first);
    }

    #[test]
    fn releasing_a_network_releases_its_node_addresses() {
        let mut a = Allocations::default();
        a.allocate_network("tn", &[]).unwrap();
        a.allocate_node_ip("tn", "n1").unwrap();
        a.release_network("tn");
        assert_eq!(a.node_ip("tn", "n1"), None);
        a.allocate_network("tn", &[]).unwrap();
        assert_eq!(a.node_ips("tn").count(), 0);
    }

    #[test]
    fn a_book_without_node_addresses_decodes_and_encodes_unchanged() {
        let old = r#"{"pool":"10.244.0.0/16","subnet_prefix":24,"networks":{"gitlab":{"subnet":"10.244.0.0/24","instances":{},"reserved":[],"cooldown":[]}}}"#;
        let mut a = Allocations::decode(Some(old)).expect("old book decodes");
        assert_eq!(a.node_ips("gitlab").count(), 0);
        let reencoded = serde_json::to_string(&a).unwrap();
        assert!(!reencoded.contains("nodes"), "{reencoded}");

        a.allocate_node_ip("gitlab", "n1").unwrap();
        let encoded = serde_json::to_string(&a).unwrap();
        assert!(encoded.contains("nodes"));
        assert_eq!(Allocations::decode(Some(&encoded)), Some(a));
    }

    fn managed_excluded(ips: &[&str]) -> BTreeSet<Ipv4Addr> {
        ips.iter().map(|ip| ip.parse().unwrap()).collect()
    }

    #[test]
    fn managed_keys_stay_out_of_the_other_key_spaces() {
        assert_eq!(managed_key("lan"), "@lan");
        assert!(is_managed_key("@lan"));
        assert!(!is_managed_key("stack_lan"));
        assert!(!is_managed_key("CTN_x"));
    }

    #[test]
    fn managed_addresses_come_from_the_networks_own_subnet() {
        let mut a = Allocations::default();
        let subnet: Ipv4Net = "192.168.32.0/29".parse().unwrap();
        let key = managed_key("mgd");
        let excluded = managed_excluded(&["192.168.32.2", "192.168.32.3"]);
        let sync = a.sync_managed_network(&key, subnet, &excluded);
        assert!(sync.changed && sync.evicted.is_empty());
        assert_eq!(a.subnet(&key), Some(subnet));
        assert_eq!(a.gateway(&key), Some("192.168.32.1".parse().unwrap()));
        // Hosts are .1-.6: gateway .1 and nodes .2 .3 are excluded, leaving .4 .5 .6.
        let got: Vec<Ipv4Addr> = (0..3)
            .map(|i| a.allocate_ip(&key, &instance(i)).unwrap())
            .collect();
        let want: Vec<Ipv4Addr> = ["192.168.32.4", "192.168.32.5", "192.168.32.6"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(got, want);
        let err = a.allocate_ip(&key, &instance(3)).unwrap_err();
        assert!(matches!(err, NetError::AddressExhausted { .. }));
        // Idempotent.
        assert_eq!(a.allocate_ip(&key, &instance(1)).unwrap(), want[1]);
        assert_eq!(
            a.sync_managed_network(&key, subnet, &excluded),
            ManagedSync::default()
        );
    }

    #[test]
    fn a_node_taking_an_instance_address_evicts_that_instance() {
        let mut a = Allocations::default();
        let subnet: Ipv4Net = "192.168.32.0/24".parse().unwrap();
        let key = managed_key("mgd");
        a.sync_managed_network(&key, subnet, &managed_excluded(&["192.168.32.2"]));
        let ip = a.allocate_ip(&key, &instance(0)).unwrap();
        assert_eq!(ip, "192.168.32.3".parse::<Ipv4Addr>().unwrap());
        let sync = a.sync_managed_network(
            &key,
            subnet,
            &managed_excluded(&["192.168.32.2", "192.168.32.3"]),
        );
        assert_eq!(sync.evicted, vec![instance(0)]);
        assert_eq!(a.instance_ip(&key, &instance(0)), None);
        assert_eq!(
            a.allocate_ip(&key, &instance(0)).unwrap(),
            "192.168.32.4".parse::<Ipv4Addr>().unwrap()
        );
    }

    #[test]
    fn changing_a_managed_subnet_replaces_the_allocation() {
        let mut a = Allocations::default();
        let key = managed_key("mgd");
        let none = BTreeSet::new();
        a.sync_managed_network(&key, "192.168.32.0/24".parse().unwrap(), &none);
        a.allocate_ip(&key, &instance(0)).unwrap();
        let sync = a.sync_managed_network(&key, "192.168.40.0/24".parse().unwrap(), &none);
        assert!(sync.changed);
        assert_eq!(sync.evicted, vec![instance(0)]);
        assert_eq!(
            a.allocate_ip(&key, &instance(0)).unwrap(),
            "192.168.40.2".parse::<Ipv4Addr>().unwrap()
        );
    }

    #[test]
    fn a_released_managed_address_is_not_reused_while_fresh_ones_remain() {
        let mut a = Allocations::default();
        let key = managed_key("mgd");
        a.sync_managed_network(&key, "192.168.32.0/24".parse().unwrap(), &BTreeSet::new());
        let first = a.allocate_ip(&key, &instance(0)).unwrap();
        a.release_ip(&key, &instance(0));
        assert_ne!(a.allocate_ip(&key, &instance(1)).unwrap(), first);
    }

    #[test]
    fn a_book_without_excluded_addresses_decodes_and_encodes_unchanged() {
        let old = r#"{"pool":"10.244.0.0/16","subnet_prefix":24,"networks":{"gitlab":{"subnet":"10.244.0.0/24","instances":{},"reserved":[],"cooldown":[]}}}"#;
        let a = Allocations::decode(Some(old)).expect("old book decodes");
        assert!(!serde_json::to_string(&a).unwrap().contains("excluded"));
    }

    #[test]
    fn renaming_a_managed_network_keeps_its_subnet_and_addresses() {
        let mut a = Allocations::default();
        let (old, new) = (managed_key("old"), managed_key("new"));
        a.sync_managed_network(&old, "192.168.32.0/24".parse().unwrap(), &BTreeSet::new());
        let ip = a.allocate_ip(&old, &instance(0)).unwrap();
        assert!(a.rename_managed_network(&old, &new));
        assert!(a.subnet(&old).is_none());
        assert_eq!(a.instance_ip(&new, &instance(0)), Some(ip));
        assert_eq!(a.subnet(&new).unwrap().to_string(), "192.168.32.0/24");
        // Unknown source and taken target change nothing.
        assert!(!a.rename_managed_network(&old, &new));
        a.sync_managed_network(&old, "192.168.40.0/24".parse().unwrap(), &BTreeSet::new());
        assert!(!a.rename_managed_network(&old, &new));
    }
}
