use super::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Where the key that decrypts stored secrets lives.
///
/// Secrets are encrypted before entering the replicated database; the key
/// itself sits in a file on at least one node. A node without it replicates
/// secrets but cannot start a container that uses one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    /// Path to the key file.
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,

    /// Generate a key on first launch when the file is missing. Turn off on
    /// nodes that should only receive an existing key.
    #[serde(default = "default_true_bool")]
    pub generate_if_missing: bool,

    /// Path to this node's own asymmetric keypair, generated on first launch.
    /// Its public half goes in the cluster roster so a `key_file` holder can wrap
    /// the cluster key for this node.
    #[serde(default = "default_node_key_file")]
    pub node_key_file: PathBuf,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            key_file: default_key_file(),
            generate_if_missing: true,
            node_key_file: default_node_key_file(),
        }
    }
}

/// How people authenticate to the web ui and the api.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// OIDC is the recommended way in. Without it, api keys are the only option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OidcConfig>,

    /// Api keys always work as a fallback; one is generated and printed on first
    /// launch. Set to false only once OIDC is known to work.
    #[serde(default = "default_true_bool")]
    pub api_keys: bool,

    /// How long a ui session lasts.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_session_ttl")]
    pub session_ttl: Duration,
}

/// Deriving this would leave api keys off, which the validator rejects. The
/// default has to match what the serde field defaults say.
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            oidc: None,
            api_keys: true,
            session_ttl: default_session_ttl(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// Issuer URL. Discovery happens at `<issuer>/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    /// The client secret is a sealed replicated cluster secret (set from the web
    /// ui), not stored here.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Claim listing the user's groups or roles.
    #[serde(default = "default_admin_claim")]
    pub admin_claim: String,
    /// Value in that claim which grants admin. Without it, a successful login
    /// gets read only access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_value: Option<String>,
    /// Claim shown as the signed-in user's name. Falls back to `sub`.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
}
