//! Kernel VXLAN overlay: one bridge and one VXLAN device per stack network,
//! point to point FDB entries per peer node instead of multicast (a small
//! cluster's peer set is known from the config, and multicast often just
//! doesn't work between cloud hosts or over a WireGuard mesh).
//!
//! The pure logic (naming, VNI derivation, MTU, FDB diffing) lives at the top
//! of this file and has no netlink dependency, so it is unit tested without
//! privileges. The netlink glue at the bottom needs CAP_NET_ADMIN and is only
//! exercised by ignored tests.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use futures::{StreamExt, TryStreamExt};
use netlink_packet_core::{
    NetlinkMessage, NetlinkPayload, NLM_F_ACK, NLM_F_APPEND, NLM_F_CREATE, NLM_F_REQUEST,
};
use rtnetlink::packet_route::neighbour::{
    NeighbourAddress, NeighbourAttribute, NeighbourFlags, NeighbourMessage, NeighbourState,
};
use rtnetlink::packet_route::AddressFamily;
use rtnetlink::packet_route::RouteNetlinkMessage;
use rtnetlink::{Handle, LinkBridge, LinkUnspec, LinkVeth, LinkVxlan};

use tracing::warn;

use crate::error::{NetError, Result};

/// Everything `ensure_vxlan` needs to create or verify a VXLAN device.
#[derive(Debug, Clone, Copy)]
pub struct VxlanSpec<'a> {
    pub name: &'a str,
    pub vni: u32,
    pub underlay_index: u32,
    pub underlay_ip: Ipv4Addr,
    pub bridge_index: u32,
    pub mtu: u16,
}

/// UDP port VXLAN traffic is sent on. The IANA-assigned port; Linux defaults
/// to the old 8472 unless told otherwise, so we set it explicitly everywhere.
pub const VXLAN_PORT: u16 = 4789;

const IFNAMSIZ_USABLE: usize = 15;

/// Bytes of overhead a VXLAN encapsulation adds over an IPv4 underlay:
/// outer IPv4 (20) + outer UDP (8) + VXLAN header (8) + inner Ethernet (14).
pub const VXLAN_OVERHEAD_IPV4: u16 = 50;

/// Same, over an IPv6 underlay (outer IPv6 header is 40 bytes).
pub const VXLAN_OVERHEAD_IPV6: u16 = 70;

/// Used when the kernel reports no underlay MTU, and for a single-node overlay bridge (no underlay link).
pub const DEFAULT_UNDERLAY_MTU: u16 = 1500;

pub fn bridge_name(network: &str) -> String {
    short_name("clbr-", network)
}

pub fn vxlan_name(network: &str) -> String {
    short_name("clvx-", network)
}

/// Names are capped at 15 bytes. See `egress::short_name`.
fn short_name(prefix: &str, name: &str) -> String {
    crate::egress::short_name(prefix, name, IFNAMSIZ_USABLE)
}

/// FNV-1a: spreads names across the output space for interface suffixes and VNIs.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// The MAC every node gives this network's bridge.
///
/// The bridge is an anycast gateway: every node holds the same address on its
/// own bridge, all in one flooded VXLAN L2 domain. With per-node MACs a
/// container could cache a remote node's bridge MAC from the ARP race, and
/// every packet to its "gateway" would be tunnelled to a node with no
/// conntrack entry and reset: intermittent, per-container, invisible to
/// direct health checks. One shared MAC makes the race harmless.
///
/// Derived from the name so nodes agree without coordinating (like
/// `allocate_vni`). `0x02` marks it locally administered, multicast bit clear.
/// Setting it also pins it: a bridge otherwise adopts its lowest port MAC.
pub fn gateway_mac(name: &str) -> [u8; 6] {
    let hash = fnv1a(name.as_bytes()).to_be_bytes();
    [0x02, hash[1], hash[2], hash[3], hash[4], hash[5]]
}

/// `tc` arguments for the VXLAN egress filter dropping every frame whose
/// source is `mac`, the bridge's anycast gateway MAC (see
/// `Overlay::keep_gateway_frames_local`). A fixed pref and handle make
/// `replace` a no-op when the filter is already there, so the reconcile loop
/// can re-apply it every pass.
fn gateway_filter_args(vxlan: &str, mac: [u8; 6]) -> Vec<String> {
    let mac = mac
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    [
        "filter", "replace", "dev", vxlan, "egress", "pref", "1", "handle", "1", "protocol", "all",
        "flower", "src_mac", &mac, "action", "drop",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect()
}

/// VXLAN VNIs are 24 bit. Derived from the network name so every node agrees;
/// on a collision with `taken` (or reserved 0) the input is perturbed with a
/// counter and rehashed.
///
/// Only as good as `taken`: every node must pass the same set. Nothing
/// replicates one today, so the daemon passes none and refuses a colliding
/// network (`vni_collides`) rather than renumbering a live tunnel.
pub fn allocate_vni(name: &str, taken: &BTreeSet<u32>) -> u32 {
    const VNI_SPACE: u64 = 1 << 24;
    let mut attempt = 0u32;
    loop {
        let input = if attempt == 0 {
            name.to_string()
        } else {
            format!("{name}#{attempt}")
        };
        let vni = (fnv1a(input.as_bytes()) % VNI_SPACE) as u32;
        // VNI 0 is reserved by convention; skip it like any other collision.
        if vni != 0 && !taken.contains(&vni) {
            return vni;
        }
        attempt += 1;
    }
}

/// True when `network` and some *other* name in `others` derive the same VNI
/// and `network` is not the one that should keep it. Two stacks sharing a VNI
/// would be one flooded L2 segment, so the loser must not be brought up. The
/// winner is the lexicographically smallest name: every node orders the same
/// way, so they all pick the same loser, and a name that was already running
/// is only displaced by a newcomer that sorts before it.
pub fn vni_collides<'a>(
    network: &str,
    others: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    vni_collides_with(network, others, |n| allocate_vni(n, &BTreeSet::new()))
}

fn vni_collides_with<'a>(
    network: &str,
    others: impl IntoIterator<Item = &'a str>,
    vni_of: impl Fn(&str) -> u32,
) -> Option<String> {
    let mine = vni_of(network);
    others
        .into_iter()
        .filter(|other| *other < network && vni_of(other) == mine)
        .min()
        .map(str::to_string)
}

/// Overlay device MTU: underlay MTU minus encapsulation overhead. Set on the
/// bridge and VXLAN device both, or large packets vanish (the classic vxlan
/// black hole: no ICMP, TCP stalls past a payload size).
pub fn overlay_mtu(underlay_mtu: u16, underlay_is_ipv6: bool) -> u16 {
    let overhead = if underlay_is_ipv6 {
        VXLAN_OVERHEAD_IPV6
    } else {
        VXLAN_OVERHEAD_IPV4
    };
    underlay_mtu.saturating_sub(overhead)
}

/// What the FDB should look like versus what add/remove calls are needed to
/// get there. Point to point mode uses the all-zero wildcard MAC per peer
/// entry (the same trick `bridge fdb append 00:00:00:00:00:00 dst <peer>`
/// uses): with no multicast group, every entry the kernel doesn't have a
/// specific MAC for floods to all wildcard destinations, giving the same
/// head-end replication multicast would have provided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FdbDiff {
    pub add: Vec<Ipv4Addr>,
    pub remove: Vec<Ipv4Addr>,
}

pub fn diff_fdb(desired: &BTreeSet<Ipv4Addr>, actual: &BTreeSet<Ipv4Addr>) -> FdbDiff {
    FdbDiff {
        add: desired.difference(actual).copied().collect(),
        remove: actual.difference(desired).copied().collect(),
    }
}

const WILDCARD_MAC: [u8; 6] = [0; 6];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Underlay {
    pub index: u32,
    pub name: String,
    pub mtu: u16,
}

#[derive(Debug, Default)]
struct VxlanParams {
    vni: u32,
    local: Option<Ipv4Addr>,
    link: Option<u32>,
}

/// Netlink side of the overlay. Every method is idempotent; reconcile calls them every pass.
pub struct Overlay {
    handle: Handle,
}

impl Overlay {
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }

    /// The interface carrying `address`, as (index, name, mtu). Keyed off the
    /// node's own internal address, the one thing the config already pins down,
    /// so nobody writes an interface name into config.
    pub async fn underlay_for(&self, address: Ipv4Addr) -> Result<Option<Underlay>> {
        let mut addrs = self.handle.address().get().execute();
        let mut index = None;

        while let Some(addr) = addrs
            .try_next()
            .await
            .map_err(|e| NetError::from_netlink("addresses", e))?
        {
            for attr in &addr.attributes {
                if let rtnetlink::packet_route::address::AddressAttribute::Address(
                    std::net::IpAddr::V4(existing),
                ) = attr
                {
                    if *existing == address {
                        index = Some(addr.header.index);
                    }
                }
            }
        }

        let Some(index) = index else {
            return Ok(None);
        };

        let mut links = self.handle.link().get().match_index(index).execute();
        let Some(link) = links
            .try_next()
            .await
            .map_err(|e| NetError::from_netlink("link", e))?
        else {
            return Ok(None);
        };

        let mut name = String::new();
        let mut mtu = DEFAULT_UNDERLAY_MTU;
        for attr in &link.attributes {
            match attr {
                rtnetlink::packet_route::link::LinkAttribute::IfName(n) => name = n.clone(),
                rtnetlink::packet_route::link::LinkAttribute::Mtu(m) => {
                    mtu = (*m).min(u16::MAX as u32) as u16
                }
                _ => {}
            }
        }

        Ok(Some(Underlay { index, name, mtu }))
    }

    async fn find_link(&self, name: &str) -> Result<Option<u32>> {
        let mut links = self
            .handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute();
        match links.try_next().await {
            Ok(Some(link)) => Ok(Some(link.header.index)),
            Ok(None) => Ok(None),
            // "no such device" surfaces as a netlink error from the kernel,
            // not an empty stream, when matching by name.
            Err(_) => Ok(None),
        }
    }

    /// Ensure the bridge exists with gateway address, MTU, up. Returns its index.
    pub async fn ensure_bridge(
        &self,
        name: &str,
        gateway: Ipv4Addr,
        prefix_len: u8,
        mtu: u16,
    ) -> Result<u32> {
        let index = match self.find_link(name).await? {
            Some(index) => index,
            None => {
                self.handle
                    .link()
                    .add(LinkBridge::new(name).build())
                    .execute()
                    .await
                    .map_err(|e| NetError::from_netlink(name, e))?;
                self.find_link(name)
                    .await?
                    .ok_or_else(|| NetError::UnknownNetwork(name.to_string()))?
            }
        };

        self.ensure_mtu(name, index, mtu).await?;
        // Before the address: the gateway is only safely anycast once every
        // node answers ARP for it with the same MAC. See `gateway_mac`.
        self.ensure_mac(name, index, gateway_mac(name)).await?;
        self.ensure_address(name, index, gateway, prefix_len)
            .await?;
        self.ensure_up(name, index).await?;
        disable_tx_checksum_offload(name)?;
        Ok(index)
    }

    /// Make `extras` the only IPv4 addresses on interface `name` besides
    /// `gateway`: adds the missing ones as secondary addresses of the subnet
    /// and removes any other, such as the address of a node that left the
    /// network. Idempotent.
    pub async fn reconcile_bridge_addresses(
        &self,
        name: &str,
        gateway: Ipv4Addr,
        extras: &BTreeSet<Ipv4Addr>,
        prefix_len: u8,
    ) -> Result<()> {
        let index = self
            .find_link(name)
            .await?
            .ok_or_else(|| NetError::UnknownNetwork(name.to_string()))?;
        let mut stale = Vec::new();
        let mut addrs = self
            .handle
            .address()
            .get()
            .set_link_index_filter(index)
            .execute();
        while let Some(addr) = addrs
            .try_next()
            .await
            .map_err(|e| NetError::from_netlink(name, e))?
        {
            for attr in &addr.attributes {
                if let rtnetlink::packet_route::address::AddressAttribute::Address(
                    std::net::IpAddr::V4(existing),
                ) = attr
                {
                    if *existing != gateway && !extras.contains(existing) {
                        stale.push(addr.clone());
                    }
                }
            }
        }
        for addr in stale {
            self.handle
                .address()
                .del(addr)
                .execute()
                .await
                .map_err(|e| NetError::from_netlink(name, e))?;
        }
        for extra in extras {
            self.ensure_address(name, index, *extra, prefix_len).await?;
        }
        Ok(())
    }

    /// VNI, local address and underlay link a VXLAN device was created with.
    async fn vxlan_params(&self, index: u32) -> Result<Option<VxlanParams>> {
        use rtnetlink::packet_route::link::{InfoData, InfoVxlan, LinkAttribute, LinkInfo};
        let mut links = self.handle.link().get().match_index(index).execute();
        let Some(link) = links
            .try_next()
            .await
            .map_err(|e| NetError::from_netlink("link", e))?
        else {
            return Ok(None);
        };
        let mut found = None;
        for attr in &link.attributes {
            let LinkAttribute::LinkInfo(infos) = attr else {
                continue;
            };
            for info in infos {
                let LinkInfo::Data(InfoData::Vxlan(settings)) = info else {
                    continue;
                };
                let mut params = VxlanParams::default();
                for setting in settings {
                    match setting {
                        InfoVxlan::Id(id) => params.vni = *id,
                        InfoVxlan::Local(ip) => params.local = Some(*ip),
                        InfoVxlan::Link(l) => params.link = Some(*l),
                        _ => {}
                    }
                }
                found = Some(params);
            }
        }
        Ok(found)
    }

    /// Ensure the VXLAN device exists, enslaved to `spec.bridge_index`. Returns its index.
    pub async fn ensure_vxlan(&self, spec: &VxlanSpec<'_>) -> Result<u32> {
        let VxlanSpec {
            name,
            vni,
            underlay_index,
            underlay_ip,
            bridge_index,
            mtu,
        } = *spec;
        let mut existing = self.find_link(name).await?;
        if let Some(index) = existing {
            // `learning` and the rest are fixed at creation, but the VNI and
            // the local address are what put this segment on the right wire:
            // a device left over from another config (a renumbered node, a
            // different VNI) silently joins the wrong segment, and the kernel
            // cannot change either in place. Replace it.
            if let Some(found) = self.vxlan_params(index).await? {
                if found.vni != vni
                    || found.local != Some(underlay_ip)
                    || found.link.is_some_and(|l| l != underlay_index)
                {
                    warn!(
                        device = name, found_vni = found.vni, wanted_vni = vni,
                        found_local = ?found.local, wanted_local = %underlay_ip,
                        "vxlan device does not match the config, recreating it",
                    );
                    self.handle
                        .link()
                        .del(index)
                        .execute()
                        .await
                        .map_err(|e| NetError::from_netlink(format!("{name} (replace)"), e))?;
                    existing = None;
                }
            }
        }
        let index = match existing {
            Some(index) => index,
            None => {
                // Created down and brought up afterwards, the same order the
                // bridge uses. Asking the kernel to create and open a vxlan in
                // one message fails with EOPNOTSUPP: it opens the device before
                // the device is enslaved to its bridge.
                let message = LinkVxlan::new(name, vni)
                    .dev(underlay_index)
                    .port(VXLAN_PORT)
                    .local(underlay_ip)
                    // Static FDB only: never learn from traffic, we manage
                    // every remote explicitly.
                    .learning(false)
                    .build();
                self.handle
                    .link()
                    .add(message)
                    .execute()
                    .await
                    .map_err(|e| NetError::from_netlink(format!("{name} (create)"), e))?;
                self.find_link(name)
                    .await?
                    .ok_or_else(|| NetError::UnknownNetwork(name.to_string()))?
            }
        };

        self.ensure_mtu(&format!("{name} (mtu)"), index, mtu)
            .await?;
        self.ensure_controller(&format!("{name} (enslave)"), index, bridge_index)
            .await?;
        self.ensure_up(&format!("{name} (up)"), index).await?;
        Ok(index)
    }

    /// Pin an existing bridge's MAC to the shared anycast value; no-op if absent.
    ///
    /// Covers bridges this node merely still has (left behind after the last
    /// instance moved away, swept only once the stack is gone): they still answer
    /// ARP for the anycast gateway, and with the wrong MAC hijack containers on
    /// other nodes. See `gateway_mac`.
    pub async fn ensure_existing_bridge_mac(&self, name: &str) -> Result<()> {
        let Some(index) = self.find_link(name).await? else {
            return Ok(());
        };
        self.ensure_mac(name, index, gateway_mac(name)).await
    }

    /// Drop every frame this node's own bridge sends out through the VXLAN
    /// device. No-op if the device does not exist.
    ///
    /// The host's own ARP traffic on the shared-MAC bridge reaches other nodes
    /// with that bridge's own MAC as source; the kernel logs each one ("received
    /// packet ... with own address as source address"), thousands an hour. None
    /// is useful there: containers only need the gateway on their own node, and
    /// the host reaches remote containers through the relay (`crate::relay`).
    /// Container-to-container frames carry their own MACs and are untouched.
    pub async fn keep_gateway_frames_local(&self, bridge: &str, vxlan: &str) -> Result<()> {
        if self.find_link(vxlan).await?.is_none() {
            return Ok(());
        }
        run_tc(&["qdisc", "replace", "dev", vxlan, "clsact"].map(String::from))?;
        run_tc(&gateway_filter_args(vxlan, gateway_mac(bridge)))
    }

    async fn ensure_mac(&self, name: &str, index: u32, mac: [u8; 6]) -> Result<()> {
        self.handle
            .link()
            .set(
                LinkUnspec::new_with_index(index)
                    .address(mac.to_vec())
                    .build(),
            )
            .execute()
            .await
            .map_err(|e| NetError::from_netlink(format!("{name} (mac)"), e))
    }

    async fn ensure_mtu(&self, name: &str, index: u32, mtu: u16) -> Result<()> {
        self.handle
            .link()
            .set(LinkUnspec::new_with_index(index).mtu(mtu as u32).build())
            .execute()
            .await
            .map_err(|e| NetError::from_netlink(name, e))
    }

    async fn ensure_up(&self, name: &str, index: u32) -> Result<()> {
        self.handle
            .link()
            .set(LinkUnspec::new_with_index(index).up().build())
            .execute()
            .await
            .map_err(|e| NetError::from_netlink(name, e))
    }

    async fn ensure_controller(&self, name: &str, index: u32, bridge_index: u32) -> Result<()> {
        self.handle
            .link()
            .set(
                LinkUnspec::new_with_index(index)
                    .controller(bridge_index)
                    .build(),
            )
            .execute()
            .await
            .map_err(|e| NetError::from_netlink(name, e))
    }

    /// Checks first so a permission-denied dry run does not misreport.
    async fn ensure_address(
        &self,
        name: &str,
        index: u32,
        gateway: Ipv4Addr,
        prefix_len: u8,
    ) -> Result<()> {
        let mut addrs = self
            .handle
            .address()
            .get()
            .set_link_index_filter(index)
            .execute();
        while let Some(addr) = addrs
            .try_next()
            .await
            .map_err(|e| NetError::from_netlink(name, e))?
        {
            for attr in &addr.attributes {
                if let rtnetlink::packet_route::address::AddressAttribute::Address(
                    std::net::IpAddr::V4(existing),
                ) = attr
                {
                    if *existing == gateway {
                        return Ok(());
                    }
                }
            }
        }

        self.handle
            .address()
            .add(index, std::net::IpAddr::V4(gateway), prefix_len)
            .execute()
            .await
            .map_err(|e| NetError::from_netlink(name, e))
    }

    async fn current_fdb(&self, vxlan_name: &str, vxlan_index: u32) -> Result<BTreeSet<Ipv4Addr>> {
        let mut current = BTreeSet::new();
        let mut entries = self
            .handle
            .neighbours()
            .get()
            .set_address_family(AddressFamily::Bridge)
            .execute();
        while let Some(entry) = entries
            .try_next()
            .await
            .map_err(|e| NetError::from_netlink(format!("{vxlan_name} (fdb dump)"), e))?
        {
            if entry.header.ifindex != vxlan_index {
                continue;
            }
            if !is_wildcard_entry(&entry) {
                continue;
            }
            for attr in &entry.attributes {
                if let NeighbourAttribute::Destination(NeighbourAddress::Inet(ip)) = attr {
                    current.insert(*ip);
                }
            }
        }
        Ok(current)
    }

    pub async fn reconcile_fdb(
        &self,
        vxlan_name: &str,
        vxlan_index: u32,
        peers: &BTreeSet<Ipv4Addr>,
    ) -> Result<()> {
        let current = self.current_fdb(vxlan_name, vxlan_index).await?;
        let diff = diff_fdb(peers, &current);

        for peer in diff.add {
            self.append_fdb(vxlan_name, vxlan_index, peer).await?;
        }

        for peer in diff.remove {
            let message = wildcard_fdb_message(vxlan_index, peer);
            self.handle
                .neighbours()
                .del(message)
                .execute()
                .await
                .map_err(|e| NetError::from_netlink(format!("{vxlan_name} (fdb del {peer})"), e))?;
        }

        Ok(())
    }

    /// Give this node a host port on a segment bridge: a veth pair whose peer is
    /// enslaved to `bridge`, with `mac` on the host end and `addr`/32 on it.
    ///
    /// The unique MAC is what lets a router reach this node over VXLAN (the
    /// anycast gateway MAC never leaves its node). `arp_ignore=1` on veth and
    /// bridge stops either answering ARP for the other's address; `rp_filter=2`
    /// is loose reverse-path filtering, as for egress bridges. Idempotent.
    pub async fn ensure_host_port(
        &self,
        bridge: &str,
        veth: &str,
        peer: &str,
        mac: [u8; 6],
        addr: Ipv4Addr,
    ) -> Result<()> {
        let bridge_index = self
            .find_link(bridge)
            .await?
            .ok_or_else(|| NetError::UnknownNetwork(bridge.to_string()))?;
        let veth_index = match self.find_link(veth).await? {
            Some(index) => index,
            None => {
                self.handle
                    .link()
                    .add(LinkVeth::new(veth, peer).build())
                    .execute()
                    .await
                    .map_err(|e| NetError::from_netlink(format!("{veth} (create)"), e))?;
                self.find_link(veth)
                    .await?
                    .ok_or_else(|| NetError::UnknownNetwork(veth.to_string()))?
            }
        };
        let peer_index = self
            .find_link(peer)
            .await?
            .ok_or_else(|| NetError::UnknownNetwork(peer.to_string()))?;

        let mtu = std::fs::read_to_string(format!("/sys/class/net/{bridge}/mtu"))
            .ok()
            .and_then(|m| m.trim().parse::<u16>().ok())
            .unwrap_or(DEFAULT_UNDERLAY_MTU);
        self.ensure_mtu(&format!("{veth} (mtu)"), veth_index, mtu)
            .await?;
        self.ensure_mtu(&format!("{peer} (mtu)"), peer_index, mtu)
            .await?;
        self.ensure_mac(veth, veth_index, mac).await?;
        self.ensure_controller(&format!("{peer} (enslave)"), peer_index, bridge_index)
            .await?;
        self.ensure_address(veth, veth_index, addr, 32).await?;
        set_sysctl(&format!("net/ipv4/conf/{veth}/arp_ignore"), "1")?;
        set_sysctl(&format!("net/ipv4/conf/{bridge}/arp_ignore"), "1")?;
        set_sysctl(&format!("net/ipv4/conf/{veth}/rp_filter"), "2")?;
        self.ensure_up(peer, peer_index).await?;
        self.ensure_up(veth, veth_index).await
    }

    /// The node address of a managed network on its own port: the host port
    /// of [`Self::ensure_host_port`] with `addr` as its only address, and
    /// `arp_announce=2` so ARP for another node's address is sent from it.
    pub async fn ensure_node_port(
        &self,
        bridge: &str,
        veth: &str,
        peer: &str,
        mac: [u8; 6],
        addr: Ipv4Addr,
    ) -> Result<()> {
        self.ensure_host_port(bridge, veth, peer, mac, addr).await?;
        self.reconcile_bridge_addresses(veth, addr, &BTreeSet::new(), 32)
            .await?;
        set_sysctl(&format!("net/ipv4/conf/{veth}/arp_announce"), "2")
    }

    pub async fn teardown_host_port(&self, veth: &str) -> Result<()> {
        if let Some(index) = self.find_link(veth).await? {
            self.handle
                .link()
                .del(index)
                .execute()
                .await
                .map_err(|e| NetError::from_netlink(veth, e))?;
        }
        Ok(())
    }

    /// Remove a network's bridge and VXLAN device; missing links are fine.
    pub async fn teardown(&self, bridge: &str, vxlan: &str) -> Result<()> {
        for name in [vxlan, bridge] {
            if let Some(index) = self.find_link(name).await? {
                self.handle
                    .link()
                    .del(index)
                    .execute()
                    .await
                    .map_err(|e| NetError::from_netlink(name, e))?;
            }
        }
        Ok(())
    }
}

fn set_sysctl(path: &str, value: &str) -> Result<()> {
    std::fs::write(format!("/proc/sys/{path}"), value).map_err(|e| NetError::Io {
        context: format!("sysctl {path}"),
        source: e,
    })
}

/// Turn off TX checksum offload (and TSO) on a bridge. Some kernel/NIC
/// combinations leave the checksum unfilled for host-originated traffic on a
/// bridge the host has an address on, so the container drops the segment:
/// handshake works, first payload vanishes, proxy calls time out. Confirmed
/// live with `ethtool -K <bridge> tx off`.
/// Whether `ethtool` is on `PATH`. Checked at startup and by `clouduler init`:
/// a missing binary silently leaves bridges dropping payloads (see
/// `disable_tx_checksum_offload`), a hard symptom to trace back.
pub fn ethtool_available() -> bool {
    std::process::Command::new("ethtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn disable_tx_checksum_offload(name: &str) -> Result<()> {
    let output = std::process::Command::new("ethtool")
        .args(["-K", name, "tx", "off"])
        .output()
        .map_err(|e| NetError::Io {
            context: format!("ethtool -K {name} tx off"),
            source: e,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(NetError::Io {
            context: format!(
                "ethtool -K {name} tx off: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            source: std::io::Error::other("ethtool exited non-zero"),
        })
    }
}

fn run_tc(args: &[String]) -> Result<()> {
    let output = std::process::Command::new("tc")
        .args(args)
        .output()
        .map_err(|e| NetError::Io {
            context: format!("tc {}", args.join(" ")),
            source: e,
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(NetError::Io {
            context: format!(
                "tc {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            source: std::io::Error::other("tc exited non-zero"),
        })
    }
}

fn is_wildcard_entry(entry: &NeighbourMessage) -> bool {
    entry.attributes.iter().any(|attr| {
        matches!(attr, NeighbourAttribute::LinkLayerAddress(lla) if lla.as_slice() == WILDCARD_MAC)
    })
}

impl Overlay {
    /// Add one wildcard FDB entry, with NLM_F_APPEND.
    ///
    /// Head-end replication means one entry per peer, all sharing the all-zero
    /// MAC. Only APPEND builds that list: CREATE|EXCL refuses the second peer
    /// as a duplicate, and CREATE|REPLACE is rejected outright by the vxlan
    /// driver with EOPNOTSUPP. rtnetlink's neighbour builder offers only those
    /// two, so the message goes out through the raw request path instead.
    async fn append_fdb(&self, vxlan_name: &str, index: u32, peer: Ipv4Addr) -> Result<()> {
        let mut request = NetlinkMessage::from(RouteNetlinkMessage::NewNeighbour(
            wildcard_fdb_message(index, peer),
        ));
        request.header.flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_APPEND;

        let mut response = self
            .handle
            .clone()
            .request(request)
            .map_err(|e| NetError::from_netlink(vxlan_name, e))?;
        while let Some(message) = response.next().await {
            if let NetlinkPayload::Error(err) = message.payload {
                // An error payload with no code is the ACK, not a failure.
                if err.code.is_some() {
                    return Err(NetError::from_netlink(
                        format!("{vxlan_name} (fdb append {peer})"),
                        rtnetlink::Error::NetlinkError(err),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn wildcard_fdb_message(index: u32, dest: Ipv4Addr) -> NeighbourMessage {
    let mut message = NeighbourMessage::default();
    message.header.family = AddressFamily::Bridge;
    message.header.ifindex = index;
    // NTF_SELF sends this to the vxlan driver's own FDB rather than to a
    // bridge's. Without it the kernel hands the request to the bridge code,
    // which has no idea what to do with it and answers EINVAL.
    message.header.flags = NeighbourFlags::Own;
    // Static entry: we manage the peer list, nothing should age it out.
    message.header.state = NeighbourState::Permanent;
    message
        .attributes
        .push(NeighbourAttribute::LinkLayerAddress(WILDCARD_MAC.to_vec()));
    message
        .attributes
        .push(NeighbourAttribute::Destination(NeighbourAddress::Inet(
            dest,
        )));
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_and_vxlan_names_stay_within_ifnamsiz() {
        for name in ["gitlab", "a-very-long-stack-network-name", "x", ""] {
            let bridge = bridge_name(name);
            let vxlan = vxlan_name(name);
            assert!(bridge.len() <= IFNAMSIZ_USABLE, "{bridge} too long");
            assert!(vxlan.len() <= IFNAMSIZ_USABLE, "{vxlan} too long");
        }
    }

    #[test]
    fn short_names_pass_through() {
        assert_eq!(bridge_name("gitlab"), "clbr-gitlab");
        assert_eq!(vxlan_name("gitlab"), "clvx-gitlab");
    }

    #[test]
    fn names_are_deterministic() {
        assert_eq!(bridge_name("gitlab"), bridge_name("gitlab"));
        assert_eq!(vxlan_name("monitoring"), vxlan_name("monitoring"));
    }

    /// Every node must derive the same MAC from the name alone, or a container can cache a remote bridge and get reset.
    #[test]
    fn the_gateway_mac_is_the_same_on_every_node() {
        assert_eq!(gateway_mac("clbr-gitlab"), gateway_mac("clbr-gitlab"));
        assert_ne!(gateway_mac("clbr-gitlab"), gateway_mac("clbr-monitoring"));
    }

    /// Locally administered (bit 1 of the first octet) and unicast (bit 0
    /// clear). A multicast bit here would make the kernel reject the address.
    #[test]
    fn the_gateway_mac_is_locally_administered_and_unicast() {
        for name in ["clbr-gitlab", "clbr-a", "", "clbr-immichf731"] {
            let mac = gateway_mac(name);
            assert_eq!(mac[0] & 0x01, 0, "{name}: multicast bit set");
            assert_eq!(mac[0] & 0x02, 0x02, "{name}: not locally administered");
        }
    }

    /// The filter must name the gateway MAC in the zero-padded form `tc`
    /// parses and drop on the tunnel's egress: anything else either matches
    /// nothing or leaves every other node logging its own MAC coming back.
    #[test]
    fn the_gateway_filter_drops_the_gateway_mac_leaving_through_the_tunnel() {
        let args = gateway_filter_args("clvx-mediasbf43", [0x02, 0x00, 0x0a, 0x87, 0x30, 0x05]);
        assert_eq!(
            args.join(" "),
            "filter replace dev clvx-mediasbf43 egress pref 1 handle 1 protocol all \
             flower src_mac 02:00:0a:87:30:05 action drop"
        );
    }

    #[test]
    fn long_names_still_differ_after_truncation() {
        let a = bridge_name("a-very-long-stack-network-name-one");
        let b = bridge_name("a-very-long-stack-network-name-two");
        assert_ne!(a, b);
    }

    #[test]
    fn vni_is_deterministic_and_nonzero() {
        let vni1 = allocate_vni("gitlab", &BTreeSet::new());
        let vni2 = allocate_vni("gitlab", &BTreeSet::new());
        assert_eq!(vni1, vni2);
        assert_ne!(vni1, 0);
        assert!(vni1 < (1 << 24));
    }

    #[test]
    fn vni_collision_is_resolved_by_perturbation() {
        let first = allocate_vni("gitlab", &BTreeSet::new());
        let mut taken = BTreeSet::new();
        taken.insert(first);
        // Force a collision by taking the VNI a network would otherwise get,
        // and check the allocator picks a different one instead of failing.
        let second = allocate_vni("gitlab", &taken);
        assert_ne!(first, second);
    }

    #[test]
    fn a_vni_collision_is_detected_and_only_the_later_name_loses() {
        let same = |_: &str| 7u32;
        // The smaller name keeps the VNI, the larger one is refused, on every
        // node alike.
        assert_eq!(
            vni_collides_with("web_default", ["db_default"], same),
            Some("db_default".to_string())
        );
        assert_eq!(vni_collides_with("db_default", ["web_default"], same), None);
        // A network never collides with itself, and a different VNI is fine.
        assert_eq!(vni_collides_with("a", ["a"], same), None);
        assert_eq!(
            vni_collides_with("b", ["a"], |n| if n == "a" { 1 } else { 2 }),
            None
        );
    }

    #[test]
    fn different_networks_usually_get_different_vnis() {
        let vni1 = allocate_vni("gitlab", &BTreeSet::new());
        let vni2 = allocate_vni("monitoring", &BTreeSet::new());
        assert_ne!(vni1, vni2);
    }

    #[test]
    fn mtu_accounts_for_ipv4_overhead() {
        assert_eq!(overlay_mtu(1500, false), 1450);
    }

    #[test]
    fn mtu_accounts_for_ipv6_overhead() {
        assert_eq!(overlay_mtu(1500, true), 1430);
    }

    #[test]
    fn mtu_never_underflows_on_a_tiny_underlay() {
        assert_eq!(overlay_mtu(40, false), 0);
    }

    #[test]
    fn fdb_diff_is_empty_when_in_sync() {
        let peers: BTreeSet<Ipv4Addr> =
            ["10.10.0.1".parse().unwrap(), "10.10.0.2".parse().unwrap()]
                .into_iter()
                .collect();
        let diff = diff_fdb(&peers, &peers);
        assert!(diff.add.is_empty());
        assert!(diff.remove.is_empty());
    }

    #[test]
    fn fdb_diff_finds_additions_and_removals() {
        let desired: BTreeSet<Ipv4Addr> =
            ["10.10.0.1".parse().unwrap(), "10.10.0.3".parse().unwrap()]
                .into_iter()
                .collect();
        let actual: BTreeSet<Ipv4Addr> =
            ["10.10.0.1".parse().unwrap(), "10.10.0.2".parse().unwrap()]
                .into_iter()
                .collect();
        let diff = diff_fdb(&desired, &actual);
        assert_eq!(diff.add, vec!["10.10.0.3".parse::<Ipv4Addr>().unwrap()]);
        assert_eq!(diff.remove, vec!["10.10.0.2".parse::<Ipv4Addr>().unwrap()]);
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN and a real netlink socket"]
    fn ensure_bridge_is_idempotent_against_a_real_kernel() {
        // Exercised manually / in the qemu harness.
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN and a real netlink socket"]
    fn reconcile_fdb_converges_against_a_real_kernel() {
        // Exercised manually / in the qemu harness.
    }
}
