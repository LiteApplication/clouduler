//! Reserved secret name for the Cloudflare API token.
//!
//! Sealed and stored through raft the same way a wireguard config is
//! (`crate::wireguard`), instead of sitting in a file on whichever one node
//! an operator happened to set it from. Every node that holds the cluster key
//! can open it, so certificate issuance and DNS record publishing keep
//! working after a failover to a different leader.

pub const CLOUDFLARE_TOKEN_SECRET: &str = "_clouduler_cloudflare_token";

pub fn is_reserved_cloudflare_secret(name: &str) -> bool {
    name == CLOUDFLARE_TOKEN_SECRET
}
