//! Reserved secret name for the OIDC client secret.
//!
//! Sealed and stored through raft the same way the Cloudflare api token is
//! (`crate::cloudflare`), instead of sitting in a file on whichever one node
//! an operator happened to set it from. Every node that holds the cluster key
//! can open it, so login keeps working after a failover to a different
//! leader.

pub const OIDC_CLIENT_SECRET: &str = "_clouduler_oidc_client_secret";

pub fn is_reserved_oidc_secret(name: &str) -> bool {
    name == OIDC_CLIENT_SECRET
}
