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
use rtnetlink::{Handle, LinkBridge, LinkUnspec, LinkVxlan};

use tracing::warn;

use crate::error::{NetError, Result};

/// UDP port VXLAN traffic is sent on. The IANA-assigned port; Linux defaults
/// to the old 8472 unless told otherwise, so we set it explicitly everywhere.
pub const VXLAN_PORT: u16 = 4789;

/// Linux interface names are capped at `IFNAMSIZ - 1` usable bytes.
const IFNAMSIZ_USABLE: usize = 15;

/// Bytes of overhead a VXLAN encapsulation adds over an IPv4 underlay:
/// outer IPv4 (20) + outer UDP (8) + VXLAN header (8) + inner Ethernet (14).
pub const VXLAN_OVERHEAD_IPV4: u16 = 50;

/// Same, over an IPv6 underlay: outer IPv6 (40) is bigger than IPv4.
pub const VXLAN_OVERHEAD_IPV6: u16 = 70;

/// Assumed when the kernel does not report an MTU for the underlay link,
/// which should not happen but must not be a panic if it does. Also the plain
/// (non-vxlan) MTU used for a single-node overlay's bridge, which has no
/// underlay link to read one from.
pub const DEFAULT_UNDERLAY_MTU: u16 = 1500;

/// Deterministic interface name for a stack network's bridge.
pub fn bridge_name(network: &str) -> String {
    short_name("clbr-", network)
}

/// Deterministic interface name for a stack network's VXLAN device.
pub fn vxlan_name(network: &str) -> String {
    short_name("clvx-", network)
}

/// Build a `<= 15` byte interface name from a prefix and a network name. See
/// `egress::short_name`, which every interface and chain name shares.
fn short_name(prefix: &str, name: &str) -> String {
    crate::egress::short_name(prefix, name, IFNAMSIZ_USABLE)
}

/// FNV-1a. Used for both the interface name suffix and the VNI derivation;
/// not cryptographic, just needs to spread names across the output space.
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
/// The bridge is an *anycast* gateway: every node holds the same address
/// (`10.244.x.1`) on its own bridge, and a container talks to whichever one is
/// local to it. But all those bridges sit in one flooded VXLAN L2 domain, so a
/// container's ARP request for the gateway reaches every node and any of them
/// may answer first. With a per-node MAC, the container caches whichever MAC
/// won the race — and if that is a remote node's bridge, every packet it sends
/// to its "gateway" is tunnelled to a machine that holds no socket and no
/// conntrack entry for the flow, which promptly resets it. The result is a
/// container whose connections are refused for as long as the ARP entry lives,
/// re-rolled each time it is re-resolved: intermittent, per-container, and
/// invisible to every health check that talks to the container directly.
///
/// Giving every node the same MAC makes the race harmless — whoever answers,
/// the answer names a MAC the container's own local bridge owns, so the frame
/// is consumed locally and never leaves the node.
///
/// Derived from the name so every node computes it without coordinating, the
/// same way `allocate_vni` does. `0x02` marks it locally administered and
/// keeps the multicast bit clear.
///
/// Setting it explicitly also pins it: a bridge left to itself adopts the
/// lowest MAC among its ports, so it silently changes as containers come and
/// go.
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

/// VXLAN VNIs are 24 bit. Derive one deterministically from the network name
/// so every node computes the same VNI without coordination; on a collision
/// with `taken` (or the reserved 0), perturb the input with a counter and
/// rehash until a free one is found.
///
/// Only as good as `taken`: every node has to pass the same set or they
/// disagree about the VNI. Nothing replicates such a set today, so the daemon
/// passes none and instead refuses a network whose plain VNI collides with
/// another's (`vni_collides`) rather than renumbering a live tunnel.
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

/// Overlay device MTU: the underlay link's MTU minus VXLAN encapsulation
/// overhead. Set this on the bridge and the VXLAN device both, or large
/// packets get silently dropped somewhere on the path (the classic vxlan
/// black hole: no ICMP, no reset, TCP just stalls past a certain payload
/// size). `underlay_is_ipv6` picks which overhead to subtract, since IPv6's
/// bigger header eats more of the budget.
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

/// Diff the desired peer set against what's currently in the FDB.
pub fn diff_fdb(desired: &BTreeSet<Ipv4Addr>, actual: &BTreeSet<Ipv4Addr>) -> FdbDiff {
    FdbDiff {
        add: desired.difference(actual).copied().collect(),
        remove: actual.difference(desired).copied().collect(),
    }
}

/// The wildcard MAC used for every point to point FDB entry.
const WILDCARD_MAC: [u8; 6] = [0; 6];

/// The interface the overlay rides on.
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

/// Netlink side of the overlay: creating the bridge and VXLAN device,
/// enslaving one to the other, and keeping the FDB in sync. Every method is
/// idempotent, since the reconcile loop calls these on every pass regardless
/// of whether anything actually changed.
pub struct Overlay {
    handle: Handle,
}

impl Overlay {
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }

    /// The interface carrying `address`, as (index, name, mtu).
    ///
    /// The VXLAN device has to be told which link to send its encapsulated
    /// traffic out of, and its MTU has to be derived from that link's. Both come
    /// from the node's own address on the internal network, which is the one
    /// thing the config already pins down, rather than from an interface name
    /// nobody should have to write into a config file.
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

    /// Index of a link by name, if it exists.
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

    /// Ensure the bridge exists, has the gateway address, the right MTU and
    /// is up. Returns its link index.
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

    /// The VNI, local address and underlay link a VXLAN device was created
    /// with, or `None` if `index` is not a vxlan device (or the kernel did
    /// not report its settings).
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

    /// Ensure the VXLAN device exists over `underlay_index`, enslaved to
    /// `bridge_index`, with the given VNI, local address and MTU. Returns its
    /// link index.
    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_vxlan(
        &self,
        name: &str,
        vni: u32,
        underlay_index: u32,
        underlay_ip: Ipv4Addr,
        bridge_index: u32,
        mtu: u16,
    ) -> Result<u32> {
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

    /// Pin an already-existing bridge's MAC to the shared anycast value, if
    /// this node has that bridge at all. A no-op when it does not.
    ///
    /// `ensure_bridge` covers every network this node currently runs something
    /// on. This covers the ones it merely still *has*: when the last instance
    /// on a network moves to another node the bridge is left behind (it is only
    /// swept once the stack is gone cluster-wide), and a leftover bridge still
    /// holds the anycast gateway address on the same flooded segment — so it
    /// still answers ARP for it, and with the wrong MAC it still hijacks
    /// containers on other nodes. See `gateway_mac`.
    pub async fn ensure_existing_bridge_mac(&self, name: &str) -> Result<()> {
        let Some(index) = self.find_link(name).await? else {
            return Ok(());
        };
        self.ensure_mac(name, index, gateway_mac(name)).await
    }

    /// Drop every frame this node's own bridge sends before it leaves through
    /// the network's VXLAN device. A no-op when the device does not exist.
    ///
    /// Every node's bridge carries the same anycast gateway address and MAC
    /// (see `gateway_mac`) on one flooded segment, so whatever the host itself
    /// puts on the bridge and the bridge floods or forwards over the tunnel -
    /// its ARP requests, its answers to a remote container's ARP for the
    /// gateway - reaches every other node's bridge with that bridge's *own*
    /// MAC as the source. The kernel logs each one ("received packet on
    /// clvx-... with own address as source address"), thousands an hour per
    /// network.
    ///
    /// None of them is any use over there: a container only ever needs the
    /// gateway on its own node, and the host here cannot talk to a container
    /// on another node through the overlay anyway, since that container's
    /// reply to the shared gateway address is taken by its own node's bridge
    /// (the host reaches such a container through its node's relay instead,
    /// see `crate::relay`). Container-to-container frames carry the
    /// containers' own MACs and are untouched.
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

    /// Add the gateway address to the bridge if it isn't already there.
    /// Re-adding an existing address is an idempotent no-op on Linux, but we
    /// still check first so a permission-denied dry run doesn't misreport.
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

    /// Read the current point to point FDB entries (wildcard MAC, family
    /// bridge) on a VXLAN device.
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

    /// Bring the FDB in line with `peers`: every other node's internal IP.
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

    /// Remove a network's bridge and VXLAN device. Missing links are not an
    /// error, since deleting an already-gone device is the reconcile loop's
    /// steady state after a network is torn down.
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

/// Turn off TX checksum offload (and, as a dependency, TSO) on a
/// clouduler-managed bridge. Some kernel/NIC-driver combinations miscompute
/// the checksum for traffic the host itself originates onto a software
/// bridge it also has an address on: the kernel hands the packet out
/// believing hardware will fill in the checksum, no hardware ever does, and
/// the receiving container's stack silently drops the corrupt segment.
/// Symptom: the TCP handshake completes fine (small control packets, whose
/// checksum is often trivially valid anyway) but the first real request or
/// response payload vanishes, so every reverse-proxy call to that backend
/// times out or resets even though the backend itself is healthy. Confirmed
/// live: `ethtool -K <bridge> tx off` turned deterministic failures into
/// deterministic successes with zero other change.
/// Whether `ethtool` is on `PATH` at all. Checked once at daemon startup and
/// by `clouduler init` rather than left to surface only when the first
/// overlay bridge actually needs `disable_tx_checksum_offload` — a missing
/// binary there doesn't fail loudly on its own, it just leaves that bridge's
/// containers silently dropping every real request/response payload (see
/// `disable_tx_checksum_offload`'s doc comment), which is a much harder
/// symptom to trace back to "install ethtool" than a clear startup warning.
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

    /// The whole point: every node has to arrive at the same MAC from the name
    /// alone. If two nodes disagree, a container can ARP its anycast gateway
    /// and cache a *remote* node's bridge, which then resets every connection
    /// it makes.
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
        // Exercised manually / in the qemu harness: run ensure_bridge twice
        // and assert the second call is a no-op.
    }

    #[test]
    #[ignore = "needs CAP_NET_ADMIN and a real netlink socket"]
    fn reconcile_fdb_converges_against_a_real_kernel() {
        // Exercised manually / in the qemu harness.
    }
}
