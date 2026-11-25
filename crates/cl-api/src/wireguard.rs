//! WireGuard-kind networks: reserved-name conventions shared by the config import route and the ingress-rule CRUD route, plus the
//! ingress store. See `crate::routes::wireguard` and `cl_net::wireguard`.

use serde::{Deserialize, Serialize};

use cl_config::Protocol;
use cl_raft::ClusterState;

pub const WIREGUARD_INGRESS_CONFIG: &str = "_clouduler_wireguard_ingress";

/// One secret per (network, node): each node in a wireguard network runs its own tunnel with its own key material.
const WIREGUARD_SECRET_PREFIX: &str = "_clouduler_wireguard_config_";

pub fn wireguard_secret_name(network: &str, node: &str) -> String {
    format!("{WIREGUARD_SECRET_PREFIX}{network}_{node}")
}

pub fn is_reserved_wireguard_secret(name: &str) -> bool {
    name.starts_with(WIREGUARD_SECRET_PREFIX)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct WireguardIngressRule {
    pub id: String,
    pub network: String,
    pub port: u16,
    pub protocol: Protocol,
    /// Port on this node's own docker port bind the rule forwards to.
    pub target_port: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WireguardIngressStore {
    pub rules: Vec<WireguardIngressRule>,
}

impl WireguardIngressStore {
    pub fn load(state: &ClusterState) -> Self {
        let Some(entry) = state.configs.get(WIREGUARD_INGRESS_CONFIG) else {
            return Self::default();
        };
        let Ok(raw) = entry.decode() else {
            return Self::default();
        };
        serde_json::from_slice(&raw).unwrap_or_default()
    }

    pub fn encode(&self) -> String {
        crate::encode_json_config(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_name_is_scoped_per_network_and_node() {
        assert_eq!(
            wireguard_secret_name("vpn", "n1"),
            "_clouduler_wireguard_config_vpn_n1"
        );
        assert!(is_reserved_wireguard_secret(&wireguard_secret_name(
            "vpn", "n1"
        )));
        assert!(!is_reserved_wireguard_secret("db_password"));
    }

    #[test]
    fn store_round_trips_through_encode_and_load() {
        let mut store = WireguardIngressStore::default();
        store.rules.push(WireguardIngressRule {
            id: "abc".into(),
            network: "vpn".into(),
            port: 2222,
            protocol: Protocol::Tcp,
            target_port: 22,
        });
        let encoded = store.encode();
        let raw = cl_raft::base64_decode(&encoded).unwrap();
        let decoded: WireguardIngressStore = serde_json::from_slice(&raw).unwrap();
        assert_eq!(decoded.rules, store.rules);
    }
}
