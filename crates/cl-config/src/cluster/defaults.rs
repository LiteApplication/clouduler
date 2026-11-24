use super::*;
use std::path::PathBuf;
use std::time::Duration;

pub(super) fn default_terraform_image() -> String {
    DEFAULT_TERRAFORM_IMAGE.to_string()
}

pub(super) fn default_terraform_timeout() -> Duration {
    Duration::from_secs(600)
}

pub(super) fn default_terraform_init_network() -> String {
    "bridge".to_string()
}

pub(super) fn default_image_check_interval() -> Duration {
    Duration::from_secs(6 * 60 * 60)
}

pub(super) fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/clouduler")
}

pub(super) fn default_key_file() -> PathBuf {
    PathBuf::from("/etc/clouduler/secrets.key")
}

pub(super) fn default_node_key_file() -> PathBuf {
    PathBuf::from("/etc/clouduler/node.key")
}

pub(super) fn default_true_bool() -> bool {
    true
}

pub(super) fn default_session_ttl() -> Duration {
    Duration::from_secs(12 * 3600)
}

pub(super) fn default_scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

pub(super) fn default_admin_claim() -> String {
    "groups".to_string()
}

pub(super) fn default_username_claim() -> String {
    "preferred_username".to_string()
}

pub(super) fn default_shutdown_push_timeout() -> Duration {
    Duration::from_secs(15 * 60)
}

pub(super) fn default_stop_grace() -> Duration {
    DEFAULT_STOP_GRACE
}

/// Long enough that the default `fence_timeout` (15s) plus the default
/// `stop_grace` (30s) still finish before the leader reschedules, which
/// `fence_margin_warning` requires.
pub(super) fn default_node_timeout() -> Duration {
    Duration::from_secs(60)
}

pub(super) fn default_fence_timeout() -> Duration {
    Duration::from_secs(15)
}

pub(super) fn default_probe_interval() -> Duration {
    Duration::from_secs(5)
}

pub(super) fn is_true(b: &bool) -> bool {
    *b
}

pub(super) fn default_max_parallel_syncs() -> usize {
    1
}

pub(super) fn is_default_max_parallel_syncs(n: &usize) -> bool {
    *n == default_max_parallel_syncs()
}

pub(super) fn default_sync_interval() -> Duration {
    Duration::from_secs(15 * 60)
}

pub(super) fn default_rpc_port() -> u16 {
    DEFAULT_RPC_PORT
}

/// Not 443: the reverse proxy owns the standard ports.
pub(super) fn default_web_port() -> u16 {
    8443
}

pub(super) fn default_web_backend_port() -> u16 {
    DEFAULT_WEB_BACKEND_PORT
}

pub(super) fn is_false(b: &bool) -> bool {
    !*b
}

pub(super) fn default_remember_ip_ttl() -> Duration {
    Duration::from_secs(30 * 24 * 3600)
}

pub(super) fn default_proxy_auth_scopes() -> Vec<String> {
    vec![
        "openid".into(),
        "profile".into(),
        "email".into(),
        "groups".into(),
    ]
}

pub(super) fn default_portal_title() -> String {
    "Sign in to continue".to_string()
}

pub(super) fn default_dns_port() -> u16 {
    53
}

pub(super) fn default_http_port() -> u16 {
    80
}

pub(super) fn default_https_port() -> u16 {
    443
}

pub(super) fn default_update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

pub(super) fn default_backup_branch() -> String {
    "main".to_string()
}
