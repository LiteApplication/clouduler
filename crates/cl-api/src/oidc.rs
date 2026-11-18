//! Reserved secret name for the OIDC client secret. Sealed and stored through raft like the Cloudflare token (`crate::cloudflare`),
//! so every node holding the cluster key can open it and login survives a leader failover.

pub const OIDC_CLIENT_SECRET: &str = "_clouduler_oidc_client_secret";

/// The reverse proxy login portal's client (`proxy.auth`), distinct from the dashboard's; read by `cl_proxy::auth_gate`.
pub const PROXY_OIDC_CLIENT_SECRET: &str = "_clouduler_proxy_oidc_client_secret";

pub fn is_reserved_oidc_secret(name: &str) -> bool {
    name == OIDC_CLIENT_SECRET || name == PROXY_OIDC_CLIENT_SECRET
}
