//! Reserved secret name for the OIDC client secret.
//!
//! Sealed and stored through raft the same way the Cloudflare api token is
//! (`crate::cloudflare`), instead of sitting in a file on whichever one node
//! an operator happened to set it from. Every node that holds the cluster key
//! can open it, so login keeps working after a failover to a different
//! leader.

pub const OIDC_CLIENT_SECRET: &str = "_clouduler_oidc_client_secret";

/// Client secret of the reverse proxy's login portal (`proxy.auth`), a
/// different identity provider client from the dashboard's. The proxy reads
/// it by this name (`cl_proxy::auth_gate`).
pub const PROXY_OIDC_CLIENT_SECRET: &str = "_clouduler_proxy_oidc_client_secret";

pub fn is_reserved_oidc_secret(name: &str) -> bool {
    name == OIDC_CLIENT_SECRET || name == PROXY_OIDC_CLIENT_SECRET
}
