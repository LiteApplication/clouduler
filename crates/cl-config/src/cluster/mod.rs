mod auth;
mod defaults;
mod drive;
mod failover;
mod integrations;
mod network;
mod node;
mod proxy;
#[cfg(test)]
mod tests;

pub use auth::*;
use defaults::*;
pub use drive::*;
pub use failover::*;
pub use integrations::*;
pub use network::*;
pub use node::*;
pub use proxy::*;

use crate::error::{ConfigError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Default port for the node to node RPC channel (raft, drive sync, proxy backhaul).
pub const DEFAULT_RPC_PORT: u16 = 7373;

/// Root of the cluster config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    pub cluster: ClusterMeta,
    #[serde(deserialize_with = "deserialize_networks")]
    pub networks: BTreeMap<String, Network>,
    #[serde(default)]
    pub drives: BTreeMap<String, Drive>,
    pub nodes: BTreeMap<String, Node>,
    pub web: WebConfig,
    /// Cluster wide default for where nodes keep their data. Individual nodes
    /// can override it.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub failover: FailoverConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareConfig>,
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Git-based cluster self-update. Write-once through the web ui.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_update: Option<SelfUpdateConfig>,
    /// Mirrors cluster.yaml, stacks, configs and secrets into an admin-owned git repo.
    /// Absent means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<GitBackupConfig>,
    /// The MCP endpoint and the OAuth server that fronts it.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Registry checks for container image updates.
    #[serde(default)]
    pub updates: ImageUpdatesConfig,
    /// Per-service OpenTofu modules. Absent means the feature is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terraform: Option<TerraformConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ClusterMeta {
    pub name: String,
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
}
