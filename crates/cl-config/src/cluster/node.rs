use super::*;
use crate::stack::Protocol;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// Votes in the raft quorum, can be elected leader.
    Manager,
    /// Runs containers, follows the quorum.
    Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Identity for raft membership and the removed-node tombstone. Minted fresh
    /// by `clouduler init` on every provisioning (including a `--force` rejoin),
    /// so only the old id stays tombstoned and a reused name never collides.
    pub id: uuid::Uuid,
    pub role: NodeRole,
    /// Networks this node is attached to, keyed by network name. Subnetted
    /// networks take a bare IP, public networks take an address block.
    #[serde(default)]
    pub network: BTreeMap<String, NodeAddress>,
    /// Drives this node implements, mapped to the local directory backing them.
    /// For a served drive, this is the real directory on the serving node and
    /// the mountpoint everywhere else.
    #[serde(default)]
    pub drives: BTreeMap<String, PathBuf>,

    /// Free-form labels, matched by `deploy.placement.constraints` as
    /// `node.labels.<key>`, so a stack can ask for a class of node instead of
    /// pinning to `node.hostname`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,

    #[serde(default = "default_rpc_port")]
    pub rpc_port: u16,
    /// Where this node keeps its database and working files. Overrides the
    /// cluster default, for nodes whose big disk is not in the usual place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,

    /// Where this node's docker daemon stores data (dockerd `data-root`); `None`
    /// leaves dockerd's default. Local only, so no cluster default. `clouduler
    /// init` writes it to `/etc/docker/daemon.json`; changing it later needs a
    /// dockerd restart and does not move existing data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_data_dir: Option<PathBuf>,

    /// Overrides `secrets.generate_if_missing` for this node.
    ///
    /// A joining node must not invent its own key (cluster secrets are sealed
    /// with the existing one). `clouduler init --join` sets this to false unless
    /// told otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_secret_key: Option<bool>,

    /// Host an instance only when no other eligible node can take it, so a
    /// large-drive but weak node is not piled with general workload.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last_resort: bool,

    /// Whether this node times a small durable write in its data dir when the
    /// connectivity table asks. Turn off for disks that should not be touched
    /// (an SD card); the node then reports no disk figure.
    #[serde(default = "default_true_bool", skip_serializing_if = "is_true")]
    pub disk_probe: bool,
}

/// A node address on one network. Bare IP on subnetted networks, an address
/// block on public ones, a host interface name on egress networks. An empty
/// value (`net:` or `net: null`) on a private network means the node
/// implements it for access only: it routes into it but holds no address.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(untagged)]
pub enum NodeAddress {
    /// No address; only valid on a private network. Listed first so a null
    /// is never offered to the other variants.
    None,
    Ip(IpAddr),
    /// An IP on a private network plus the ports the node answers there; see
    /// [`PrivateAddress`]. Tried after `Ip` and before the other maps: its
    /// `ip` key is unique to it.
    Private(PrivateAddress),
    // Tried after `Ip`, so a bare IP string still parses as one; only a
    // non-IP bare string falls through to here.
    Interface(InterfaceAddress),
    Public(PublicAddress),
}

impl NodeAddress {
    pub fn as_ip(&self) -> Option<IpAddr> {
        match self {
            NodeAddress::Ip(ip) => Some(*ip),
            NodeAddress::Private(p) => Some(p.ip),
            NodeAddress::None | NodeAddress::Interface(_) | NodeAddress::Public(_) => None,
        }
    }

    /// The ports this address declares open: the router's on a public
    /// network, the tunnel's on an egress or wireguard one, the node's own on
    /// a private one. Empty for an address that declares none.
    pub fn open_ports(&self) -> Option<&BTreeSet<OpenPort>> {
        match self {
            NodeAddress::Public(p) => Some(&p.open_ports),
            NodeAddress::Interface(i) => Some(&i.open_ports),
            NodeAddress::Private(p) => Some(&p.open_ports),
            NodeAddress::None | NodeAddress::Ip(_) => None,
        }
    }

    pub fn as_interface(&self) -> Option<&str> {
        match self {
            NodeAddress::Interface(addr) => Some(&addr.interface),
            NodeAddress::None
            | NodeAddress::Ip(_)
            | NodeAddress::Private(_)
            | NodeAddress::Public(_) => None,
        }
    }

    pub fn as_interface_address(&self) -> Option<&InterfaceAddress> {
        match self {
            NodeAddress::Interface(addr) => Some(addr),
            NodeAddress::None
            | NodeAddress::Ip(_)
            | NodeAddress::Private(_)
            | NodeAddress::Public(_) => None,
        }
    }

    pub fn as_public(&self) -> Option<&PublicAddress> {
        match self {
            NodeAddress::Public(p) => Some(p),
            NodeAddress::None
            | NodeAddress::Ip(_)
            | NodeAddress::Private(_)
            | NodeAddress::Interface(_) => None,
        }
    }
}

/// A node's address on an unmanaged private network when it also declares
/// open ports: the IP, and the ports this node answers on it. A raw
/// `x-clouduler.expose` port on the network is then only placed on a node that
/// lists it. A node with no ports keeps the bare IP form.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct PrivateAddress {
    pub ip: IpAddr,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub open_ports: BTreeSet<OpenPort>,
    /// Every port (any protocol) answers on this node, so the scheduler treats
    /// all of them as open and `open_ports` is not consulted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_all: bool,
}

/// A node's address on an egress or wireguard network: the local host
/// interface (a VPN tunnel, usually) a service egresses through.
/// `open_ports` names ports the tunnel's far end forwards back to this node,
/// so the scheduler only places a service exposing one where it will arrive.
///
/// `open_all` declares every port open, so `open_ports` is not consulted.
///
/// Serializes as a bare string when `open_ports` is empty, `open_all` is off and
/// `fallback` is unset, so `vpn: wg0` round trips unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, as = "InterfaceAddressWire")
)]
pub struct InterfaceAddress {
    pub interface: String,
    pub open_ports: BTreeSet<OpenPort>,
    /// Every port (any protocol) is forwarded back to this node, so the scheduler
    /// treats all of them as open and `open_ports` is not consulted.
    pub open_all: bool,
    /// Egress networks only: a second interface to route out of while
    /// `health_check_ip` stops answering pings sent out `interface`. Re-checked
    /// every reconcile tick, so it switches back when the primary recovers. See
    /// `docs/networking.md`.
    pub fallback: Option<EgressFallback>,
}

/// The wire shape of `InterfaceAddress` for the generated TypeScript bindings:
/// a bare interface name, or the full object once ports or a fallback are set.
#[cfg(feature = "ts")]
#[derive(ts_rs::TS)]
#[ts(rename = "InterfaceAddress", untagged)]
#[allow(dead_code)]
enum InterfaceAddressWire {
    Bare(String),
    Full {
        interface: String,
        open_ports: BTreeSet<OpenPort>,
        #[ts(optional)]
        open_all: Option<bool>,
        #[ts(optional)]
        fallback: Option<EgressFallback>,
    },
}

impl InterfaceAddress {
    pub fn new(interface: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            open_ports: BTreeSet::new(),
            open_all: false,
            fallback: None,
        }
    }
}

/// A fallback interface for an `egress` network's node address, and the
/// address used to tell whether the primary interface still has a route to
/// the internet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct EgressFallback {
    pub interface: String,
    pub health_check_ip: IpAddr,
}

impl Serialize for InterfaceAddress {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.open_ports.is_empty() && !self.open_all && self.fallback.is_none() {
            serializer.serialize_str(&self.interface)
        } else {
            #[derive(Serialize)]
            struct Full<'a> {
                interface: &'a str,
                open_ports: &'a BTreeSet<OpenPort>,
                #[serde(skip_serializing_if = "std::ops::Not::not")]
                open_all: bool,
                #[serde(skip_serializing_if = "Option::is_none")]
                fallback: &'a Option<EgressFallback>,
            }
            Full {
                interface: &self.interface,
                open_ports: &self.open_ports,
                open_all: self.open_all,
                fallback: &self.fallback,
            }
            .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for InterfaceAddress {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            interface: String,
            #[serde(default)]
            open_ports: BTreeSet<OpenPort>,
            #[serde(default)]
            open_all: bool,
            #[serde(default)]
            fallback: Option<EgressFallback>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            Full(Full),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Bare(interface) => InterfaceAddress {
                interface,
                open_ports: BTreeSet::new(),
                open_all: false,
                fallback: None,
            },
            Raw::Full(full) => InterfaceAddress {
                interface: full.interface,
                open_ports: full.open_ports,
                open_all: full.open_all,
                fallback: full.fallback,
            },
        })
    }
}

/// How the node is reached on a public network. At least one field must be set.
/// ipv4 publishes an A record, ipv6 an AAAA, domain a CNAME.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct PublicAddress {
    #[serde(default)]
    pub ipv4: Option<Ipv4Addr>,
    #[serde(default)]
    pub ipv6: Option<Ipv6Addr>,
    #[serde(default)]
    pub domain: Option<String>,
    /// Ports this node's router forwards to it. A raw `x-clouduler.expose` port
    /// on this network is only placed on a node that lists it here.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub open_ports: BTreeSet<OpenPort>,
    /// Every port (any protocol) is forwarded to this node, so the scheduler
    /// treats all of them as open and `open_ports` is not consulted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_all: bool,
}

impl NodeAddress {
    /// Whether this address declares every port open (`open_all`), on a public
    /// network's router, an egress or wireguard tunnel, or a private address.
    pub fn opens_all_ports(&self) -> bool {
        match self {
            NodeAddress::Public(p) => p.open_all,
            NodeAddress::Interface(i) => i.open_all,
            NodeAddress::Private(p) => p.open_all,
            NodeAddress::None | NodeAddress::Ip(_) => false,
        }
    }
}

impl PublicAddress {
    pub fn is_empty(&self) -> bool {
        self.ipv4.is_none() && self.ipv6.is_none() && self.domain.is_none()
    }
}

/// One port a node's public router forwards to it, e.g. `25/tcp` for SMTP.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct OpenPort {
    pub port: u16,
    #[serde(default)]
    pub protocol: Protocol,
}

impl Node {
    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
    }

    /// Where this node keeps its database and working files.
    pub fn data_dir<'a>(&'a self, cluster: &'a ClusterConfig) -> &'a Path {
        self.data_dir.as_deref().unwrap_or(&cluster.data_dir)
    }

    /// Whether this node may create the cluster key when its file is missing.
    pub fn generates_secret_key(&self, cluster: &ClusterConfig) -> bool {
        self.generate_secret_key
            .unwrap_or(cluster.secrets.generate_if_missing)
    }

    /// The IP this node holds on a subnetted network.
    pub fn ip_on(&self, network: &str) -> Option<IpAddr> {
        self.network.get(network)?.as_ip()
    }

    /// Whether this node can host containers on `network`: it lists the
    /// network and either holds an address on it or the network is managed (a
    /// managed network's address is an optional node-local endpoint; hosting
    /// needs only the tunnel). `managed` is whether the network is managed.
    pub fn hosts_on(&self, network: &str, managed: bool) -> bool {
        self.network
            .get(network)
            .is_some_and(|a| managed || *a != NodeAddress::None)
    }

    /// Whether this node lists `network` without an address (access only).
    pub fn is_addressless_on(&self, network: &str) -> bool {
        matches!(self.network.get(network), Some(NodeAddress::None))
    }

    /// The address block this node holds on a public network.
    pub fn public_on(&self, network: &str) -> Option<&PublicAddress> {
        self.network.get(network)?.as_public()
    }
}

impl fmt::Display for NodeRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeRole::Manager => f.write_str("manager"),
            NodeRole::Worker => f.write_str("worker"),
        }
    }
}

impl ClusterConfig {
    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.get(name)
    }

    /// True when `node` has a public network address block with an `ipv6`,
    /// i.e. clients are given an AAAA record that points at it.
    pub fn node_has_public_v6(&self, node: &str) -> bool {
        self.node(node).is_some_and(|n| {
            n.network
                .values()
                .filter_map(|a| a.as_public())
                .any(|p| p.ipv6.is_some())
        })
    }

    pub fn managers(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.role == NodeRole::Manager)
    }

    /// Number of managers that must be reachable to hold quorum.
    pub fn quorum(&self) -> usize {
        self.managers().count() / 2 + 1
    }

    /// The node address on the internal network, used for all node to node RPC.
    pub fn internal_ip(&self, node: &Node) -> Option<IpAddr> {
        node.network.get(self.internal_network()?.0)?.as_ip()
    }

    /// Address other nodes use to reach this one.
    pub fn rpc_addr(&self, node: &Node) -> Option<SocketAddr> {
        Some(SocketAddr::new(self.internal_ip(node)?, node.rpc_port))
    }

    /// Nodes implementing every one of the given drives.
    pub fn nodes_implementing<'a>(
        &'a self,
        drives: &'a [String],
    ) -> impl Iterator<Item = (&'a String, &'a Node)> {
        self.nodes
            .iter()
            .filter(move |(_, n)| drives.iter().all(|d| n.drives.contains_key(d)))
    }
}
