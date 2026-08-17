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

/// Default overlay pool, carved into per-network `/24`s. Chosen well away from
/// RFC1918 ranges people commonly use for their LAN or the cluster's internal
/// network, so a default cluster config never needs to touch it.
pub const DEFAULT_OVERLAY_POOL: &str = "10.244.0.0/16";

/// Prefix length handed to each stack network's subnet.
pub const DEFAULT_SUBNET_PREFIX: u8 = 24;

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
    /// Addresses currently handed out.
    reserved: BTreeSet<Ipv4Addr>,
    /// Released addresses, oldest first. Kept out of rotation while any
    /// address that was never used is still available, so a restarting
    /// container doesn't immediately reuse an address that may still have
    /// live conntrack or ARP cache entries elsewhere on the overlay.
    cooldown: VecDeque<Ipv4Addr>,
}

impl Allocations {
    /// A fresh allocator over the given pool, sliced into subnets of
    /// `subnet_prefix` bits (must be >= the pool's own prefix and <= 32).
    pub fn new(pool: Ipv4Net, subnet_prefix: u8) -> Self {
        Self {
            pool,
            subnet_prefix,
            networks: BTreeMap::new(),
        }
    }

    /// Subnet the network already holds, if any.
    pub fn subnet(&self, network: &str) -> Option<Ipv4Net> {
        self.networks.get(network).map(|n| n.subnet)
    }

    /// Gateway address for a network's subnet: always the first host address.
    pub fn gateway(&self, network: &str) -> Option<Ipv4Addr> {
        self.subnet(network).map(gateway_of)
    }

    /// Address already held by an instance, if any.
    pub fn instance_ip(&self, network: &str, instance: &InstanceId) -> Option<Ipv4Addr> {
        self.networks.get(network)?.instances.get(instance).copied()
    }

    /// Claim a subnet for a stack network out of the pool, skipping any
    /// candidate that overlaps a subnet already claimed by another stack
    /// network or by a declared cluster network (so the overlay can never
    /// shadow the internal network or a private one). Idempotent: a network
    /// that already has a subnet keeps it.
    pub fn allocate_network(
        &mut self,
        network: &str,
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

        let candidates = self
            .pool
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
                },
            );
            return Ok(candidate);
        }

        Err(NetError::SubnetExhausted {
            pool: self.pool.to_string(),
        })
    }

    /// Drop a network's whole allocation, freeing its subnet and every
    /// instance address in it.
    pub fn release_network(&mut self, network: &str) {
        self.networks.remove(network);
    }

    /// Allocate an address for an instance inside a network's subnet.
    /// Idempotent: re-allocating for an instance that already has an address
    /// returns the same one. `.1` is reserved for the bridge gateway.
    pub fn allocate_ip(&mut self, network: &str, instance: &InstanceId) -> Result<Ipv4Addr> {
        let net = self
            .networks
            .get_mut(network)
            .ok_or_else(|| NetError::UnknownNetwork(network.to_string()))?;

        if let Some(existing) = net.instances.get(instance) {
            return Ok(*existing);
        }

        let subnet = net.subnet;
        let gateway = gateway_of(subnet);

        // Prefer an address nobody has ever held: walk the host range in
        // order, skipping the gateway, anything reserved right now, and
        // anything sitting in the cooldown queue.
        let fresh = subnet.hosts().find(|ip| {
            *ip != gateway && !net.reserved.contains(ip) && !net.cooldown.contains(ip)
        });

        let chosen = if let Some(ip) = fresh {
            ip
        } else if let Some(ip) = net.cooldown.pop_front() {
            // Nothing fresh left: fall back to the oldest released address.
            ip
        } else {
            return Err(NetError::AddressExhausted {
                network: network.to_string(),
                subnet: subnet.to_string(),
            });
        };

        net.reserved.insert(chosen);
        net.instances.insert(instance.clone(), chosen);
        Ok(chosen)
    }

    /// Release an instance's address. A no-op if the instance holds none.
    /// The address goes to the back of the cooldown queue rather than
    /// becoming immediately available again.
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

impl Default for Allocations {
    fn default() -> Self {
        Self::new(
            DEFAULT_OVERLAY_POOL.parse().expect("valid default pool"),
            DEFAULT_SUBNET_PREFIX,
        )
    }
}

/// First host address of a subnet, reserved for the bridge.
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
    use super::*;

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
        assert_ne!(first, second, "must not immediately hand back a just-released address");
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
}
