use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};

/// Default port for the node to node RPC channel (raft, drive sync, proxy backhaul).
pub const DEFAULT_RPC_PORT: u16 = 7373;

/// Root of the cluster config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub cluster: ClusterMeta,
    pub networks: BTreeMap<String, Network>,
    #[serde(default)]
    pub drives: BTreeMap<String, Drive>,
    pub nodes: BTreeMap<String, Node>,
    pub web: WebConfig,
    #[serde(default)]
    pub cloudflare: Option<CloudflareConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ClusterMeta {
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkKind {
    /// The one network every node talks to every other node on. Carries raft,
    /// drive sync and the container overlay. Trusted, so unencrypted.
    Internal,
    /// A further non public subnet a node may sit on. Records for its domain
    /// resolve to the node address on that network.
    Private,
    /// Reachable from the internet. Records resolve to the node public address.
    Public,
}

impl NetworkKind {
    /// Internal and private networks address nodes by IP inside a subnet.
    pub fn is_subnetted(self) -> bool {
        matches!(self, NetworkKind::Internal | NetworkKind::Private)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(rename = "type")]
    pub kind: NetworkKind,
    /// Required for internal and private networks, must contain the node addresses.
    #[serde(default)]
    pub subnet: Option<IpNet>,
    /// Records under this domain resolve to node addresses on this network.
    #[serde(default)]
    pub domain: Option<String>,
}

/// A cluster wide directory namespace. Nodes opt in by mapping it to a local path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Drive {
    /// How often volumes on this drive replicate to the other implementing nodes.
    #[serde(with = "humantime_serde", default = "default_sync_interval")]
    pub sync_interval: Duration,
}

fn default_sync_interval() -> Duration {
    Duration::from_secs(15 * 60)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// Votes in the raft quorum, can be elected leader.
    Manager,
    /// Runs containers, follows the quorum.
    Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub role: NodeRole,
    /// Networks this node is attached to, keyed by network name. Subnetted
    /// networks take a bare IP, public networks take an address block.
    #[serde(default)]
    pub network: BTreeMap<String, NodeAddress>,
    /// Drives this node implements, mapped to the local directory backing them.
    #[serde(default)]
    pub drives: BTreeMap<String, PathBuf>,
    #[serde(default = "default_rpc_port")]
    pub rpc_port: u16,
}

fn default_rpc_port() -> u16 {
    DEFAULT_RPC_PORT
}

/// A node address on one network. Bare IP on subnetted networks, an address
/// block on public ones.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum NodeAddress {
    Ip(IpAddr),
    Public(PublicAddress),
}

impl NodeAddress {
    pub fn as_ip(&self) -> Option<IpAddr> {
        match self {
            NodeAddress::Ip(ip) => Some(*ip),
            NodeAddress::Public(_) => None,
        }
    }

    pub fn as_public(&self) -> Option<&PublicAddress> {
        match self {
            NodeAddress::Public(p) => Some(p),
            NodeAddress::Ip(_) => None,
        }
    }
}

/// How the node is reached on a public network. At least one field must be set.
/// ipv4 publishes an A record, ipv6 an AAAA, domain a CNAME.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicAddress {
    #[serde(default)]
    pub ipv4: Option<Ipv4Addr>,
    #[serde(default)]
    pub ipv6: Option<Ipv6Addr>,
    #[serde(default)]
    pub domain: Option<String>,
}

impl PublicAddress {
    pub fn is_empty(&self) -> bool {
        self.ipv4.is_none() && self.ipv6.is_none() && self.domain.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Hostname the UI is served on, always over HTTPS. A bare label gets the
    /// network domain appended.
    pub address: String,
    /// Network the hostname resolves through, picking which node address the
    /// record points at.
    pub network: String,
    #[serde(default = "default_web_port")]
    pub port: u16,
}

fn default_web_port() -> u16 {
    443
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CloudflareConfig {
    /// Path to a file holding the API token. Left out when the token is set
    /// from the web ui instead.
    #[serde(default)]
    pub api_token_file: Option<PathBuf>,
    /// Zones clouduler is allowed to write records into.
    pub zones: Vec<String>,
}

impl ClusterConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&raw).map_err(|e| match e {
            ConfigError::Parse { source, .. } => ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            },
            other => other,
        })
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let cfg: ClusterConfig = serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<config>"),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.get(name)
    }

    pub fn managers(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.nodes.iter().filter(|(_, n)| n.role == NodeRole::Manager)
    }

    /// Number of managers that must be reachable to hold quorum.
    pub fn quorum(&self) -> usize {
        self.managers().count() / 2 + 1
    }

    /// The single internal network. Validation guarantees exactly one exists.
    pub fn internal_network(&self) -> (&String, &Network) {
        self.networks
            .iter()
            .find(|(_, n)| n.kind == NetworkKind::Internal)
            .expect("validated config has exactly one internal network")
    }

    /// The node address on the internal network, used for all node to node RPC.
    pub fn internal_ip(&self, node: &Node) -> Option<IpAddr> {
        node.network.get(self.internal_network().0)?.as_ip()
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

    /// The UI hostname with the network domain appended when it is a bare label.
    pub fn web_fqdn(&self) -> String {
        qualify(
            &self.web.address,
            self.networks
                .get(&self.web.network)
                .and_then(|n| n.domain.as_deref()),
        )
    }
}

impl Node {
    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
    }

    /// The IP this node holds on a subnetted network.
    pub fn ip_on(&self, network: &str) -> Option<IpAddr> {
        self.network.get(network)?.as_ip()
    }

    /// The address block this node holds on a public network.
    pub fn public_on(&self, network: &str) -> Option<&PublicAddress> {
        self.network.get(network)?.as_public()
    }
}

/// Append `domain` to a bare label. Anything already containing a dot is taken
/// as a full name.
pub fn qualify(name: &str, domain: Option<&str>) -> String {
    match domain {
        Some(domain) if !name.contains('.') => format!("{name}.{domain}"),
        _ => name.to_string(),
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

impl fmt::Display for NetworkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkKind::Internal => f.write_str("internal"),
            NetworkKind::Private => f.write_str("private"),
            NetworkKind::Public => f.write_str("public"),
        }
    }
}
