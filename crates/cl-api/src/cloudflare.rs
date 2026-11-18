//! Reserved secret name for the Cloudflare API token. Sealed and stored through raft like a wireguard config (`crate::wireguard`),
//! so every node holding the cluster key can open it and issuance/DNS publishing survive a leader failover.

pub const CLOUDFLARE_TOKEN_SECRET: &str = "_clouduler_cloudflare_token";

pub fn is_reserved_cloudflare_secret(name: &str) -> bool {
    name == CLOUDFLARE_TOKEN_SECRET
}
