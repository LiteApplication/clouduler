use std::collections::BTreeSet;

use crate::cluster::{ClusterConfig, NetworkKind, NodeAddress};
use crate::error::{ConfigError, Result};

impl ClusterConfig {
    /// Structural checks the rest of the daemon then relies on. Anything that
    /// would otherwise surface as a confusing runtime failure belongs here.
    pub fn validate(&self) -> Result<()> {
        self.validate_meta()?;
        self.validate_networks()?;
        self.validate_nodes()?;
        self.validate_drives()?;
        self.validate_web()?;
        self.validate_data_dirs()?;
        self.validate_secrets()?;
        self.validate_auth()?;
        self.validate_failover()?;
        self.validate_cloudflare()?;
        self.validate_proxy()?;
        Ok(())
    }

    fn validate_failover(&self) -> Result<()> {
        let f = &self.failover;
        // Fencing must complete before anyone else takes the containers over,
        // otherwise the same volume gets two writers.
        if f.fence_timeout >= f.node_timeout {
            return Err(ConfigError::invalid(
                "failover.fence_timeout",
                format!(
                    "must be shorter than node_timeout ({:?}), or a container can run on two nodes at once",
                    f.node_timeout
                ),
            ));
        }
        if f.probe_interval >= f.fence_timeout {
            return Err(ConfigError::invalid(
                "failover.probe_interval",
                format!(
                    "must be shorter than fence_timeout ({:?}), or a node cannot be missed before it fences",
                    f.fence_timeout
                ),
            ));
        }
        if f.probe_interval.is_zero() {
            return Err(ConfigError::invalid(
                "failover.probe_interval",
                "must not be zero",
            ));
        }
        Ok(())
    }

    fn validate_meta(&self) -> Result<()> {
        if self.cluster.name.trim().is_empty() {
            return Err(ConfigError::invalid("cluster.name", "must not be empty"));
        }
        Ok(())
    }

    fn validate_networks(&self) -> Result<()> {
        let mut internal = Vec::new();
        let mut domains = BTreeSet::new();

        for (name, net) in &self.networks {
            let at = format!("networks.{name}");
            if net.kind == NetworkKind::Internal {
                internal.push(name.as_str());
            }
            match (net.kind.is_subnetted(), net.subnet) {
                (true, None) => {
                    return Err(ConfigError::invalid(
                        at,
                        format!("a {} network needs a subnet", net.kind),
                    ))
                }
                (false, Some(_)) => {
                    return Err(ConfigError::invalid(
                        at,
                        format!("a {} network must not declare a subnet", net.kind),
                    ))
                }
                _ => {}
            }
            if matches!(net.kind, NetworkKind::Egress | NetworkKind::Wireguard) && net.domain.is_some() {
                return Err(ConfigError::invalid(
                    format!("{at}.domain"),
                    format!("a {} network is not addressable and must not declare a domain", net.kind),
                ));
            }
            if let Some(domain) = &net.domain {
                if domain.trim().is_empty() || domain.starts_with('.') || domain.ends_with('.') {
                    return Err(ConfigError::invalid(
                        format!("{at}.domain"),
                        "must be a bare domain name",
                    ));
                }
                // Two networks owning one domain makes record ownership ambiguous.
                if !domains.insert(domain.to_ascii_lowercase()) {
                    return Err(ConfigError::invalid(
                        format!("{at}.domain"),
                        format!("{domain} is already claimed by another network"),
                    ));
                }
            }
        }

        match internal.len() {
            1 => Ok(()),
            0 => Err(ConfigError::invalid(
                "networks",
                "exactly one internal network is required for node to node traffic",
            )),
            _ => Err(ConfigError::invalid(
                "networks",
                format!(
                    "only one internal network is allowed, found {}",
                    internal.join(", ")
                ),
            )),
        }
    }

    fn validate_nodes(&self) -> Result<()> {
        if self.nodes.is_empty() {
            return Err(ConfigError::invalid("nodes", "at least one is required"));
        }
        if self.managers().next().is_none() {
            return Err(ConfigError::invalid(
                "nodes",
                "at least one node must have role manager",
            ));
        }

        let internal = self.internal_network().0.clone();
        let mut internal_ips = BTreeSet::new();

        for (name, node) in &self.nodes {
            let at = format!("nodes.{name}");

            if !node.network.contains_key(&internal) {
                return Err(ConfigError::invalid(
                    format!("{at}.network"),
                    format!("every node must have an address on the internal network {internal}"),
                ));
            }

            for (net_name, addr) in &node.network {
                let at = format!("{at}.network.{net_name}");
                let Some(net) = self.networks.get(net_name) else {
                    return Err(ConfigError::invalid(
                        at,
                        format!("no network named {net_name}"),
                    ));
                };
                match (net.kind, addr) {
                    (NetworkKind::Internal | NetworkKind::Private, NodeAddress::Ip(ip)) => {
                        let subnet = net.subnet.expect("subnetted network validated above");
                        if !subnet.contains(ip) {
                            return Err(ConfigError::invalid(
                                at,
                                format!("{ip} is outside the {net_name} subnet {subnet}"),
                            ));
                        }
                        if net_name == &internal && !internal_ips.insert(*ip) {
                            return Err(ConfigError::invalid(
                                at,
                                format!("{ip} is used by another node"),
                            ));
                        }
                    }
                    (NetworkKind::Internal | NetworkKind::Private, _) => {
                        return Err(ConfigError::invalid(
                            at,
                            format!("a {} network takes a bare IP address", net.kind),
                        ))
                    }
                    (NetworkKind::Public, NodeAddress::Public(public)) => {
                        if public.is_empty() {
                            return Err(ConfigError::invalid(
                                at,
                                "set at least one of ipv4, ipv6 or domain",
                            ));
                        }
                    }
                    (NetworkKind::Public, _) => {
                        return Err(ConfigError::invalid(
                            at,
                            "a public network takes an ipv4, ipv6 and/or domain block",
                        ))
                    }
                    (NetworkKind::Egress | NetworkKind::Wireguard, NodeAddress::Interface(addr)) => {
                        if addr.interface.trim().is_empty() {
                            return Err(ConfigError::invalid(
                                at,
                                "must name a non-empty host interface",
                            ));
                        }
                    }
                    (NetworkKind::Egress | NetworkKind::Wireguard, _) => {
                        return Err(ConfigError::invalid(
                            at,
                            format!("a {} network takes a bare host interface name", net.kind),
                        ))
                    }
                }
            }

            for (drive, path) in &node.drives {
                if !self.drives.contains_key(drive) {
                    return Err(ConfigError::invalid(
                        format!("{at}.drives.{drive}"),
                        format!("no drive named {drive} is declared under drives"),
                    ));
                }
                if !path.is_absolute() {
                    return Err(ConfigError::invalid(
                        format!("{at}.drives.{drive}"),
                        "must be an absolute path",
                    ));
                }
            }
        }

        Ok(())
    }

    fn validate_drives(&self) -> Result<()> {
        for (name, drive) in &self.drives {
            // A drive nobody implements can never host a volume, so it is a typo.
            if !self.nodes.values().any(|n| n.implements(name)) {
                return Err(ConfigError::invalid(
                    format!("drives.{name}"),
                    "no node implements this drive",
                ));
            }

            match (drive.is_served(), drive.served_by.is_empty()) {
                (true, true) => {
                    return Err(ConfigError::invalid(
                        format!("drives.{name}.served_by"),
                        "a served drive must name at least one node holding the data",
                    ))
                }
                (false, false) => {
                    return Err(ConfigError::invalid(
                        format!("drives.{name}.served_by"),
                        "only a served drive has a serving node, set mode: served",
                    ))
                }
                (true, false) => {
                    let mut seen = std::collections::HashSet::new();
                    for server in &drive.served_by {
                        if !seen.insert(server) {
                            return Err(ConfigError::invalid(
                                format!("drives.{name}.served_by"),
                                format!("{server} is listed more than once"),
                            ));
                        }
                        let Some(node) = self.nodes.get(server) else {
                            return Err(ConfigError::invalid(
                                format!("drives.{name}.served_by"),
                                format!("no node named {server}"),
                            ));
                        };
                        if !node.implements(name) {
                            return Err(ConfigError::invalid(
                                format!("drives.{name}.served_by"),
                                format!("{server} must map {name} to the directory holding its copy"),
                            ));
                        }
                    }
                }
                (false, true) => {}
            }
        }
        Ok(())
    }

    fn validate_data_dirs(&self) -> Result<()> {
        if !self.data_dir.is_absolute() {
            return Err(ConfigError::invalid("data_dir", "must be an absolute path"));
        }
        for (name, node) in &self.nodes {
            if let Some(dir) = &node.data_dir {
                if !dir.is_absolute() {
                    return Err(ConfigError::invalid(
                        format!("nodes.{name}.data_dir"),
                        "must be an absolute path",
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_secrets(&self) -> Result<()> {
        if !self.secrets.key_file.is_absolute() {
            return Err(ConfigError::invalid(
                "secrets.key_file",
                "must be an absolute path",
            ));
        }
        if !self.secrets.node_key_file.is_absolute() {
            return Err(ConfigError::invalid(
                "secrets.node_key_file",
                "must be an absolute path",
            ));
        }
        Ok(())
    }

    fn validate_auth(&self) -> Result<()> {
        let Some(oidc) = &self.auth.oidc else {
            // With no identity provider, api keys are the only way in.
            if !self.auth.api_keys {
                return Err(ConfigError::invalid(
                    "auth.api_keys",
                    "cannot be disabled without an oidc provider, that would lock everyone out",
                ));
            }
            return Ok(());
        };

        if !oidc.issuer.starts_with("https://") {
            return Err(ConfigError::invalid(
                "auth.oidc.issuer",
                "must be an https url",
            ));
        }
        if oidc.client_id.trim().is_empty() {
            return Err(ConfigError::invalid(
                "auth.oidc.client_id",
                "must not be empty",
            ));
        }
        if let Some(path) = &oidc.client_secret_file {
            if !path.is_absolute() {
                return Err(ConfigError::invalid(
                    "auth.oidc.client_secret_file",
                    "must be an absolute path",
                ));
            }
        }
        if !oidc.scopes.iter().any(|s| s == "openid") {
            return Err(ConfigError::invalid(
                "auth.oidc.scopes",
                "must include openid",
            ));
        }
        Ok(())
    }

    fn validate_web(&self) -> Result<()> {
        if self.web.address.trim().is_empty() {
            return Err(ConfigError::invalid("web.address", "must not be empty"));
        }
        let Some(net) = self.networks.get(&self.web.network) else {
            return Err(ConfigError::invalid(
                "web.network",
                format!("no network named {}", self.web.network),
            ));
        };
        if net.domain.is_none() && !self.web.address.contains('.') {
            return Err(ConfigError::invalid(
                "web.address",
                format!(
                    "network {} has no domain, so {} cannot be qualified",
                    self.web.network, self.web.address
                ),
            ));
        }
        // The api binds both, so they cannot be the same number. Caught here
        // rather than at bind time, where it surfaces as one listener silently
        // missing.
        if self.web.backend_port == self.web.port {
            return Err(ConfigError::invalid(
                "web.backend_port",
                "must differ from web.port",
            ));
        }
        Ok(())
    }

    fn validate_proxy(&self) -> Result<()> {
        let proxy = &self.proxy;
        if !proxy.enabled {
            return Ok(());
        }
        if proxy.http_port == proxy.https_port {
            return Err(ConfigError::invalid(
                "proxy.https_port",
                "must differ from proxy.http_port",
            ));
        }
        // Not fatal on its own: the api holds web.port on the same addresses,
        // so the https listener would fail to bind and the plain one still
        // works. Refusing here says so at `check` time instead.
        if proxy.https_port == self.web.port {
            return Err(ConfigError::invalid(
                "proxy.https_port",
                format!("web.port already listens on {}", self.web.port),
            ));
        }
        if proxy.http_port == self.web.port {
            return Err(ConfigError::invalid(
                "proxy.http_port",
                format!("web.port already listens on {}", self.web.port),
            ));
        }
        // Only a clash on this node's loopback, but that is where the proxy
        // sends dashboard traffic, so it would forward to itself.
        for (field, port) in [("proxy.http_port", proxy.http_port), ("proxy.https_port", proxy.https_port)] {
            if port == self.web.backend_port {
                return Err(ConfigError::invalid(
                    field,
                    format!("web.backend_port already listens on {port}"),
                ));
            }
        }
        if proxy.acme.enabled && self.cloudflare.is_none() {
            return Err(ConfigError::invalid(
                "proxy.acme.enabled",
                "certificate issuance is DNS-01 only, so it needs a cloudflare section",
            ));
        }
        Ok(())
    }

    fn validate_cloudflare(&self) -> Result<()> {
        let Some(cf) = &self.cloudflare else {
            return Ok(());
        };
        if cf.zones.is_empty() {
            return Err(ConfigError::invalid(
                "cloudflare.zones",
                "list at least one zone clouduler may write to",
            ));
        }
        // Every network domain must sit under a zone or its records cannot be written.
        for (name, net) in &self.networks {
            if let Some(domain) = &net.domain {
                if !cf.zones.iter().any(|z| is_within(domain, z)) {
                    return Err(ConfigError::invalid(
                        format!("networks.{name}.domain"),
                        format!("{domain} is not covered by any configured cloudflare zone"),
                    ));
                }
            }
        }
        let web = self.web_fqdn();
        if !cf.zones.iter().any(|z| is_within(&web, z)) {
            return Err(ConfigError::invalid(
                "web.address",
                format!("{web} is not covered by any configured cloudflare zone"),
            ));
        }
        Ok(())
    }
}

/// True when `name` is the domain itself or a subdomain of it.
pub(crate) fn is_within(name: &str, domain: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    name == domain || name.ends_with(&format!(".{domain}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{qualify, DEFAULT_RPC_PORT};
    use crate::error::ConfigError;

    const BASE: &str = r#"
cluster: { name: home }
networks:
  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
  lan:       { type: private,  subnet: 192.168.1.0/24, domain: lan.example.com }
  wan:       { type: public,   domain: example.com }
drives:
  fast: { sync_interval: 5m }
nodes:
  n1:
    role: manager
    network:
      tailscale: 10.10.0.1
      lan: 192.168.1.1
      wan: { ipv4: 203.0.113.5, ipv6: "2001:db8::5" }
    drives: { fast: /srv/clouduler/fast }
  n2:
    role: worker
    network:
      tailscale: 10.10.0.2
      wan: { domain: n2.example.net }
    drives: { fast: /mnt/fast }
web: { address: cloud, network: tailscale }
cloudflare:
  zones: [example.com]
"#;

    fn parse(patch: &[(&str, &str)]) -> Result<ClusterConfig> {
        let mut raw = BASE.to_string();
        for (from, to) in patch {
            assert!(raw.contains(from), "patch anchor {from} not found");
            raw = raw.replace(from, to);
        }
        ClusterConfig::parse(&raw)
    }

    #[test]
    fn parses_reference_config() {
        let cfg = ClusterConfig::parse(BASE).expect("base config is valid");
        assert_eq!(cfg.cluster.name, "home");
        assert_eq!(cfg.nodes.len(), 2);
        assert_eq!(cfg.managers().count(), 1);
        assert_eq!(cfg.quorum(), 1);
        assert_eq!(cfg.internal_network().0, "tailscale");
        assert_eq!(
            cfg.drives["fast"].sync_interval,
            std::time::Duration::from_secs(300)
        );
    }

    #[test]
    fn rpc_uses_the_internal_address() {
        let cfg = ClusterConfig::parse(BASE).unwrap();
        let addr = cfg.rpc_addr(&cfg.nodes["n1"]).unwrap();
        assert_eq!(addr.ip().to_string(), "10.10.0.1");
        assert_eq!(addr.port(), DEFAULT_RPC_PORT);
    }

    #[test]
    fn node_addresses_resolve_per_network() {
        let cfg = ClusterConfig::parse(BASE).unwrap();
        let n1 = &cfg.nodes["n1"];
        assert_eq!(n1.ip_on("lan").unwrap().to_string(), "192.168.1.1");
        assert_eq!(n1.public_on("wan").unwrap().ipv4.unwrap().to_string(), "203.0.113.5");
        assert!(n1.ip_on("wan").is_none());
        assert_eq!(
            cfg.nodes["n2"].public_on("wan").unwrap().domain.as_deref(),
            Some("n2.example.net")
        );
    }

    #[test]
    fn bare_web_address_gets_the_network_domain() {
        let cfg = ClusterConfig::parse(BASE).unwrap();
        assert_eq!(cfg.web_fqdn(), "cloud.int.example.com");
        let cfg = parse(&[("address: cloud,", "address: ui.lan.example.com,")]).unwrap();
        assert_eq!(cfg.web_fqdn(), "ui.lan.example.com");
    }

    #[test]
    fn only_one_internal_network_is_allowed() {
        let err = parse(&[("lan:       { type: private", "lan:       { type: internal")]).unwrap_err();
        assert!(err.to_string().contains("only one internal network"));

        let err = parse(&[("tailscale: { type: internal", "tailscale: { type: private")]).unwrap_err();
        assert!(err.to_string().contains("exactly one internal network"));
    }

    #[test]
    fn every_node_needs_an_internal_address() {
        let err = parse(&[("      tailscale: 10.10.0.2\n", "")]).unwrap_err();
        assert!(err.to_string().contains("address on the internal network"));
    }

    #[test]
    fn node_ip_must_sit_in_the_network_subnet() {
        let err = parse(&[("tailscale: 10.10.0.2", "tailscale: 10.99.0.2")]).unwrap_err();
        assert!(err.to_string().contains("outside the tailscale subnet"));
    }

    #[test]
    fn internal_ips_must_be_unique() {
        let err = parse(&[("tailscale: 10.10.0.2", "tailscale: 10.10.0.1")]).unwrap_err();
        assert!(err.to_string().contains("used by another node"));
    }

    #[test]
    fn address_form_must_match_the_network_kind() {
        let err = parse(&[("lan: 192.168.1.1", "lan: { ipv4: 192.168.1.1 }")]).unwrap_err();
        assert!(err.to_string().contains("takes a bare IP address"));

        let err = parse(&[
            ("wan: { ipv4: 203.0.113.5, ipv6: \"2001:db8::5\" }", "wan: 203.0.113.5"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("takes an ipv4, ipv6 and/or domain"));
    }

    #[test]
    fn public_address_must_not_be_empty() {
        let err = parse(&[("wan: { domain: n2.example.net }", "wan: {}")]).unwrap_err();
        assert!(err.to_string().contains("at least one of ipv4"));
    }

    #[test]
    fn unknown_network_reference_is_rejected() {
        let err = parse(&[("lan: 192.168.1.1", "wifi: 192.168.1.1")]).unwrap_err();
        assert!(err.to_string().contains("no network named wifi"));
    }

    #[test]
    fn subnetted_networks_need_a_subnet() {
        let err = parse(&[("subnet: 10.10.0.0/24, ", "")]).unwrap_err();
        assert!(err.to_string().contains("internal network needs a subnet"));
    }

    #[test]
    fn public_networks_reject_a_subnet() {
        let err = parse(&[("wan:       { type: public,", "wan:       { type: public, subnet: 1.0.0.0/8,")])
            .unwrap_err();
        assert!(err.to_string().contains("must not declare a subnet"));
    }

    #[test]
    fn drive_references_must_resolve() {
        let err = parse(&[("drives: { fast: /mnt/fast }", "drives: { bulk: /mnt/bulk }")]).unwrap_err();
        assert!(err.to_string().contains("no drive named bulk"));
    }

    #[test]
    fn drive_paths_must_be_absolute() {
        let err = parse(&[("fast: /mnt/fast", "fast: mnt/fast")]).unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn unimplemented_drive_is_rejected() {
        let err = parse(&[("  fast: { sync_interval: 5m }", "  fast: { sync_interval: 5m }\n  bulk: {}")])
            .unwrap_err();
        assert!(err.to_string().contains("no node implements this drive"));
    }

    #[test]
    fn wireguard_network_takes_a_bare_interface_name() {
        let cfg = parse(&[(
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard }",
        )])
        .expect("valid");
        assert_eq!(cfg.networks["vpn"].kind, NetworkKind::Wireguard);

        let err = parse(&[
            (
                "  wan:       { type: public,   domain: example.com }",
                "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard }",
            ),
            ("      tailscale: 10.10.0.1", "      tailscale: 10.10.0.1\n      vpn: { ipv4: 1.2.3.4 }"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("takes a bare host interface name"));
    }

    #[test]
    fn wireguard_interface_open_ports_round_trip_and_stay_bare_when_empty() {
        let cfg = parse(&[
            (
                "  wan:       { type: public,   domain: example.com }",
                "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard }",
            ),
            (
                "      tailscale: 10.10.0.1",
                "      tailscale: 10.10.0.1\n      vpn: { interface: wg0, open_ports: [{ port: 51413 }, { port: 53, protocol: udp }] }",
            ),
        ])
        .expect("valid");
        let addr = cfg.nodes["n1"].network["vpn"].as_interface_address().expect("interface address");
        assert_eq!(addr.interface, "wg0");
        assert_eq!(addr.open_ports.len(), 2);
        assert!(addr.open_ports.contains(&crate::cluster::OpenPort {
            port: 51413,
            protocol: crate::stack::Protocol::Tcp,
        }));

        // A bare interface (no ports declared) round trips as a plain string.
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        assert!(yaml.contains("wan:") && yaml.contains("203.0.113.5"));
        let n2_addr = cfg.nodes["n2"].network.get("tailscale").unwrap();
        let reserialized = serde_yaml::to_string(n2_addr).unwrap();
        assert_eq!(reserialized.trim(), "10.10.0.2");
    }

    #[test]
    fn wireguard_network_rejects_a_domain() {
        let err = parse(&[(
            "  wan:       { type: public,   domain: example.com }",
            "  wan:       { type: public,   domain: example.com }\n  vpn:       { type: wireguard, domain: vpn.example.com }",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("wireguard network is not addressable"));
    }

    #[test]
    fn network_domain_must_be_covered_by_a_zone() {
        let err = parse(&[("zones: [example.com]", "zones: [other.com]")]).unwrap_err();
        assert!(err.to_string().contains("not covered by any configured cloudflare zone"));
    }

    #[test]
    fn failover_defaults_are_ordered_correctly() {
        let cfg = ClusterConfig::parse(BASE).expect("valid");
        let f = &cfg.failover;
        assert!(f.probe_interval < f.fence_timeout);
        assert!(f.fence_timeout < f.node_timeout);
    }

    #[test]
    fn fencing_must_beat_rescheduling() {
        let err = parse(&[(
            "web: { address: cloud, network: tailscale }",
            "failover: { node_timeout: 10s, fence_timeout: 20s }\nweb: { address: cloud, network: tailscale }",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("must be shorter than node_timeout"));
    }

    #[test]
    fn probing_must_beat_fencing() {
        let err = parse(&[(
            "web: { address: cloud, network: tailscale }",
            "failover: { fence_timeout: 5s, probe_interval: 5s }\nweb: { address: cloud, network: tailscale }",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("must be shorter than fence_timeout"));
    }

    #[test]
    fn failover_timings_are_configurable() {
        let cfg = parse(&[(
            "web: { address: cloud, network: tailscale }",
            "failover: { node_timeout: 2m, fence_timeout: 45s, probe_interval: 10s }\nweb: { address: cloud, network: tailscale }",
        )])
        .expect("valid");
        assert_eq!(cfg.failover.node_timeout, std::time::Duration::from_secs(120));
        assert_eq!(cfg.failover.fence_timeout, std::time::Duration::from_secs(45));
    }

    #[test]
    fn cluster_needs_a_manager() {
        let err = parse(&[("role: manager", "role: worker")]).unwrap_err();
        assert!(err.to_string().contains("role manager"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = ClusterConfig::parse(&format!("{BASE}extra: 1\n")).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn nodes_implementing_filters_on_every_drive() {
        let cfg = ClusterConfig::parse(BASE).unwrap();
        assert_eq!(cfg.nodes_implementing(&["fast".into()]).count(), 2);
        assert_eq!(
            cfg.nodes_implementing(&["fast".into(), "bulk".into()]).count(),
            0
        );
    }

    #[test]
    fn qualify_only_touches_bare_labels() {
        assert_eq!(qualify("cloud", Some("example.com")), "cloud.example.com");
        assert_eq!(qualify("a.b.com", Some("example.com")), "a.b.com");
        assert_eq!(qualify("cloud", None), "cloud");
    }

    #[test]
    fn is_within_matches_subdomains_only() {
        assert!(is_within("a.example.com", "example.com"));
        assert!(is_within("example.com", "example.com"));
        assert!(!is_within("notexample.com", "example.com"));
        assert!(!is_within("example.com.evil.net", "example.com"));
    }

    #[test]
    fn data_dir_defaults_and_overrides_per_node() {
        let cfg = ClusterConfig::parse(BASE).expect("valid");
        assert_eq!(cfg.data_dir, std::path::PathBuf::from("/var/lib/clouduler"));
        assert_eq!(cfg.nodes["n1"].data_dir(&cfg), cfg.data_dir);

        let cfg = parse(&[("    role: worker", "    data_dir: /mnt/big/clouduler\n    role: worker")])
            .expect("valid");
        assert_eq!(
            cfg.nodes["n2"].data_dir(&cfg),
            std::path::Path::new("/mnt/big/clouduler")
        );
        // The other node still falls back to the cluster default.
        assert_eq!(cfg.nodes["n1"].data_dir(&cfg), cfg.data_dir);
    }

    #[test]
    fn data_dirs_must_be_absolute() {
        let err = parse(&[("    role: worker", "    data_dir: relative/path\n    role: worker")])
            .unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn a_served_drive_needs_a_serving_node() {
        let err = parse(&[("fast: { sync_interval: 5m }", "fast: { mode: served }")]).unwrap_err();
        assert!(err.to_string().contains("must name at least one node holding the data"));
    }

    #[test]
    fn a_replicated_drive_rejects_a_serving_node() {
        let err = parse(&[(
            "fast: { sync_interval: 5m }",
            "fast: { sync_interval: 5m, served_by: [n1] }",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("only a served drive"));
    }

    #[test]
    fn the_serving_node_must_implement_the_drive() {
        let err = parse(&[(
            "fast: { sync_interval: 5m }",
            "fast: { mode: served, served_by: [nowhere] }",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("no node named nowhere"));
    }

    #[test]
    fn a_served_drive_rejects_a_duplicate_server() {
        let err = parse(&[(
            "fast: { sync_interval: 5m }",
            "fast: { mode: served, served_by: [n1, n1] }",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("is listed more than once"));
    }

    #[test]
    fn a_served_drive_is_accepted() {
        let cfg = parse(&[(
            "fast: { sync_interval: 5m }",
            "fast: { mode: served, served_by: [n1] }",
        )])
        .expect("valid");
        assert!(cfg.drives["fast"].is_served());
        assert_eq!(cfg.drives["fast"].served_by, vec!["n1".to_string()]);
    }

    #[test]
    fn a_served_drive_accepts_multiple_backup_servers() {
        let cfg = parse(&[(
            "fast: { sync_interval: 5m }",
            "fast: { mode: served, served_by: [n1, n2] }",
        )])
        .expect("valid");
        assert_eq!(cfg.drives["fast"].served_by, vec!["n1".to_string(), "n2".to_string()]);
    }

    #[test]
    fn api_keys_cannot_be_the_only_way_in_and_disabled() {
        let err = parse(&[(
            "web: { address: cloud, network: tailscale }",
            "auth: { api_keys: false }\nweb: { address: cloud, network: tailscale }",
        )])
        .unwrap_err();
        assert!(err.to_string().contains("lock everyone out"));
    }

    #[test]
    fn oidc_is_validated_when_present() {
        let good = "auth:\n  api_keys: false\n  oidc:\n    issuer: https://accounts.example.com\n    client_id: clouduler\n";
        let cfg = parse(&[(
            "web: { address: cloud, network: tailscale }",
            &format!("{good}web: {{ address: cloud, network: tailscale }}"),
        )])
        .expect("valid");
        let oidc = cfg.auth.oidc.expect("oidc");
        assert_eq!(oidc.scopes, vec!["openid", "profile", "email"]);
        assert_eq!(oidc.admin_claim, "groups");

        let bad = good.replace("https://accounts", "http://accounts");
        let err = parse(&[(
            "web: { address: cloud, network: tailscale }",
            &format!("{bad}web: {{ address: cloud, network: tailscale }}"),
        )])
        .unwrap_err();
        assert!(err.to_string().contains("https url"));
    }

    #[test]
    fn secrets_default_to_a_generated_key() {
        let cfg = ClusterConfig::parse(BASE).expect("valid");
        assert!(cfg.secrets.generate_if_missing);
        assert_eq!(
            cfg.secrets.key_file,
            std::path::PathBuf::from("/etc/clouduler/secrets.key")
        );
    }
}
