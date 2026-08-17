use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
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

/// A network is either private (node to node, trusted, carries the overlay) or
/// public (reachable from the internet). Both may own a domain; records for a
/// private network's domain resolve to private IPs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkKind {
    Private,
    Public,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(rename = "type")]
    pub kind: NetworkKind,
    /// Required for private networks, the range node private IPs must fall in.
    #[serde(default)]
    pub subnet: Option<IpNet>,
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
    pub private_ip: IpAddr,
    #[serde(default)]
    pub public: Option<PublicAddress>,
    /// Drives this node implements, mapped to the local directory backing them.
    #[serde(default)]
    pub drives: BTreeMap<String, PathBuf>,
    #[serde(default = "default_rpc_port")]
    pub rpc_port: u16,
}

fn default_rpc_port() -> u16 {
    DEFAULT_RPC_PORT
}

/// How the node is reached from outside. At least one field must be set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicAddress {
    #[serde(default)]
    pub ipv4: Option<Ipv4Addr>,
    #[serde(default)]
    pub ipv6: Option<Ipv6Addr>,
    /// Used instead of A/AAAA records, published as a CNAME.
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Hostname the UI is served on, always over HTTPS.
    pub address: String,
    /// Network the hostname resolves through.
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
    /// Path to a file holding the API token. Kept out of the config so the
    /// config itself stays safe to commit.
    pub api_token_file: PathBuf,
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
        let cfg: ClusterConfig =
            serde_yaml::from_str(raw).map_err(|source| ConfigError::Parse {
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
        self.nodes
            .iter()
            .filter(|(_, n)| n.role == NodeRole::Manager)
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

    /// The private network whose subnet contains this node's private IP.
    pub fn private_network_of(&self, node: &Node) -> Option<(&String, &Network)> {
        self.networks.iter().find(|(_, net)| {
            net.kind == NetworkKind::Private
                && net.subnet.map(|s| s.contains(&node.private_ip)).unwrap_or(false)
        })
    }
}

impl Node {
    /// Address other nodes use to reach this one.
    pub fn rpc_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.private_ip, self.rpc_port)
    }

    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
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

impl PublicAddress {
    pub fn is_empty(&self) -> bool {
        self.ipv4.is_none() && self.ipv6.is_none() && self.domain.is_none()
    }
}
