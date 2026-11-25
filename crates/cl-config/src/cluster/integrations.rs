use super::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DEFAULT_TERRAFORM_IMAGE: &str = "ghcr.io/opentofu/opentofu:1.10";

/// How clouduler runs the OpenTofu modules stored with stacks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct TerraformConfig {
    /// The OpenTofu image used for init, plan and apply.
    #[serde(default = "default_terraform_image")]
    pub image: String,
    /// Wall-clock limit for one run.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_terraform_timeout")]
    pub timeout: Duration,
    /// Docker network the `tofu init` container (provider downloads) runs on.
    #[serde(default = "default_terraform_init_network")]
    pub init_network: String,
}

impl Default for TerraformConfig {
    fn default() -> Self {
        Self {
            image: default_terraform_image(),
            timeout: default_terraform_timeout(),
            init_network: default_terraform_init_network(),
        }
    }
}

/// How often the leader checks registries for newer image digests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ImageUpdatesConfig {
    /// Time between checks, raised to `MIN_IMAGE_CHECK_INTERVAL` if lower.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_image_check_interval")]
    pub check_interval: Duration,
}

/// The shortest effective `updates.check_interval`.
pub const MIN_IMAGE_CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);

impl Default for ImageUpdatesConfig {
    fn default() -> Self {
        Self {
            check_interval: default_image_check_interval(),
        }
    }
}

impl ImageUpdatesConfig {
    /// `check_interval`, raised to the minimum.
    pub fn effective_interval(&self) -> Duration {
        self.check_interval.max(MIN_IMAGE_CHECK_INTERVAL)
    }
}

/// Whether AI clients may connect over MCP.
/// Whether AI clients may connect over MCP.
///
/// Off by default; every grant still needs an admin or viewer approval.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Where the cluster downloads its own prebuilt binary to roll out updates.
/// A GitHub api token (private repo / rate limits) lives sealed in the cluster
/// secrets, not here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct SelfUpdateConfig {
    /// Either a repo url (`https://github.com/OWNER/REPO`), which tracks
    /// that repo's latest release, or a specific release's url
    /// (`https://github.com/OWNER/REPO/releases/tag/TAG`), which pins it.
    pub release_url: String,
    /// Whether the leader periodically looks up the latest release.
    #[serde(default = "default_true_bool")]
    pub check_updates: bool,
    /// How often that lookup runs.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_update_check_interval")]
    pub check_interval: Duration,
}

/// Where and how the cluster mirrors its state into git.
///
/// clouduler generates its own ssh keypair; the admin adds its public half as
/// a deploy key (`GET /api/git-backup/status`). Secrets are re-encrypted for
/// `age_recipient`, so the private key can live outside the cluster.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct GitBackupConfig {
    /// SSH url, e.g. `git@github.com:org/repo.git`. Only ssh push (deploy
    /// key) auth is implemented, so http(s) urls are rejected.
    pub repo_url: String,
    /// An `age1...` X25519 recipient public key. The matching private key is
    /// never given to clouduler.
    pub age_recipient: String,
    #[serde(default = "default_backup_branch")]
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct CloudflareConfig {
    /// Zones clouduler may write records into. The api token is a sealed cluster
    /// secret (set from the web ui), not stored here.
    pub zones: Vec<String>,
}
