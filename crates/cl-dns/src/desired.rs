//! Desired DNS state, computed purely from `ClusterConfig`. No network I/O
//! here; this is what the reconciler diffs against the live records.

use cl_config::{qualify, ClusterConfig, Node};

use crate::api::RecordType;
use crate::error::{DnsError, Result};

/// One DNS record clouduler wants to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRecord {
    pub fqdn: String,
    pub kind: RecordType,
    pub content: String,
    pub ttl: u32,
    pub proxied: bool,
}

/// The records a hostname needs on a given network, for a given node.
///
/// The network's kind decides where the answer comes from:
/// - internal/private: the node's IP on that network, one A or AAAA record.
/// - public: the node's `PublicAddress`, which can yield up to three records
///   (ipv4 -> A, ipv6 -> AAAA, domain -> CNAME) since a node can publish
///   several ways to reach it at once.
pub fn records_for(
    cfg: &ClusterConfig,
    hostname: &str,
    network: &str,
    node: &Node,
) -> Result<Vec<DesiredRecord>> {
    let net = cfg
        .networks
        .get(network)
        .ok_or_else(|| DnsError::Invalid(format!("unknown network {network}")))?;

    let fqdn = qualify(hostname, net.domain.as_deref());

    if net.kind.is_subnetted() {
        let ip = node.ip_on(network).ok_or_else(|| {
            DnsError::Invalid(format!("node has no address on network {network}"))
        })?;
        let kind = if ip.is_ipv6() { RecordType::AAAA } else { RecordType::A };
        return Ok(vec![DesiredRecord {
            fqdn,
            kind,
            content: ip.to_string(),
            ttl: 1,
            proxied: false,
        }]);
    }

    // Public network: fan out over whichever address fields are set.
    let public = node.public_on(network).ok_or_else(|| {
        DnsError::Invalid(format!("node has no address on network {network}"))
    })?;
    if public.is_empty() {
        return Err(DnsError::Invalid(format!(
            "node has no address on network {network}"
        )));
    }

    let mut records = Vec::with_capacity(3);
    if let Some(ipv4) = public.ipv4 {
        records.push(DesiredRecord {
            fqdn: fqdn.clone(),
            kind: RecordType::A,
            content: ipv4.to_string(),
            ttl: 1,
            proxied: false,
        });
    }
    if let Some(ipv6) = public.ipv6 {
        records.push(DesiredRecord {
            fqdn: fqdn.clone(),
            kind: RecordType::AAAA,
            content: ipv6.to_string(),
            ttl: 1,
            proxied: false,
        });
    }
    if let Some(domain) = &public.domain {
        records.push(DesiredRecord {
            fqdn: fqdn.clone(),
            kind: RecordType::CNAME,
            content: domain.clone(),
            ttl: 1,
            proxied: false,
        });
    }
    Ok(records)
}

/// Desired records for the web UI hostname, on the node it runs on.
pub fn web_records_for(cfg: &ClusterConfig, node: &Node) -> Result<Vec<DesiredRecord>> {
    records_for(cfg, &cfg.web.address, &cfg.web.network, node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::{ClusterConfig, NodeAddress};

    fn cfg() -> ClusterConfig {
        ClusterConfig::parse(include_str!("../../../examples/cluster.yaml"))
            .expect("example config parses")
    }

    #[test]
    fn internal_network_yields_a_record() {
        let cfg = cfg();
        let node = cfg.node("n3").unwrap();
        let recs = records_for(&cfg, "svc", "tailscale", node).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].fqdn, "svc.int.example.com");
        assert_eq!(recs[0].kind, RecordType::A);
        assert_eq!(recs[0].content, "10.10.0.3");
    }

    #[test]
    fn private_network_yields_a_record() {
        let cfg = cfg();
        let node = cfg.node("n1").unwrap();
        let recs = records_for(&cfg, "svc", "lan", node).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].fqdn, "svc.lan.example.com");
        assert_eq!(recs[0].kind, RecordType::A);
        assert_eq!(recs[0].content, "192.168.1.1");
    }

    #[test]
    fn public_network_with_ipv4_and_ipv6_yields_two_records() {
        let cfg = cfg();
        let node = cfg.node("n1").unwrap();
        let mut recs = records_for(&cfg, "svc", "wan", node).unwrap();
        recs.sort_by_key(|r| format!("{:?}", r.kind));
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].kind, RecordType::A);
        assert_eq!(recs[0].content, "203.0.113.5");
        assert_eq!(recs[1].kind, RecordType::AAAA);
        assert_eq!(recs[1].content, "2001:db8::5");
        assert!(recs.iter().all(|r| r.fqdn == "svc.example.com"));
    }

    #[test]
    fn public_network_with_domain_yields_cname() {
        let cfg = cfg();
        let node = cfg.node("n2").unwrap();
        let recs = records_for(&cfg, "svc", "wan", node).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, RecordType::CNAME);
        assert_eq!(recs[0].content, "n2.example.net");
    }

    #[test]
    fn node_with_all_three_public_fields_yields_three_records() {
        let cfg = cfg();
        let mut node = cfg.node("n1").unwrap().clone();
        node.network.insert(
            "wan".to_string(),
            NodeAddress::Public(cl_config::PublicAddress {
                ipv4: Some("203.0.113.9".parse().unwrap()),
                ipv6: Some("2001:db8::9".parse().unwrap()),
                domain: Some("n1.example.net".to_string()),
                open_ports: Default::default(),
            }),
        );
        let recs = records_for(&cfg, "svc", "wan", &node).unwrap();
        assert_eq!(recs.len(), 3);
    }

    #[test]
    fn node_missing_address_on_requested_network_errors() {
        let cfg = cfg();
        let node = cfg.node("n3").unwrap();
        // n3 is not on lan.
        let err = records_for(&cfg, "svc", "lan", node).unwrap_err();
        assert!(matches!(err, DnsError::Invalid(_)));
    }

    #[test]
    fn node_missing_public_address_errors() {
        let cfg = cfg();
        let node = cfg.node("n3").unwrap();
        let err = records_for(&cfg, "svc", "wan", node).unwrap_err();
        assert!(matches!(err, DnsError::Invalid(_)));
    }

    #[test]
    fn bare_label_gets_network_domain_appended() {
        let cfg = cfg();
        let node = cfg.node("n1").unwrap();
        let recs = records_for(&cfg, "svc", "tailscale", node).unwrap();
        assert_eq!(recs[0].fqdn, "svc.int.example.com");
    }

    #[test]
    fn dotted_hostname_is_used_as_is() {
        let cfg = cfg();
        let node = cfg.node("n1").unwrap();
        let recs = records_for(&cfg, "svc.other.example", "tailscale", node).unwrap();
        assert_eq!(recs[0].fqdn, "svc.other.example");
    }

    #[test]
    fn web_records_use_web_config() {
        let cfg = cfg();
        let node = cfg.node("n1").unwrap();
        let recs = web_records_for(&cfg, node).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].fqdn, cfg.web_fqdn());
        assert_eq!(recs[0].fqdn, "cloud.int.example.com");
    }

    #[test]
    fn unknown_network_errors() {
        let cfg = cfg();
        let node = cfg.node("n1").unwrap();
        let err = records_for(&cfg, "svc", "nope", node).unwrap_err();
        assert!(matches!(err, DnsError::Invalid(_)));
    }
}
