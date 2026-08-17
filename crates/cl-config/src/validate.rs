use std::collections::BTreeSet;

use crate::cluster::{ClusterConfig, NetworkKind};
use crate::error::{ConfigError, Result};

impl ClusterConfig {
    /// Structural checks the rest of the daemon then relies on. Anything that
    /// would otherwise surface as a confusing runtime failure belongs here.
    pub fn validate(&self) -> Result<()> {
        self.validate_meta()?;
        self.validate_networks()?;
        self.validate_nodes()?;
        self.validate_web()?;
        self.validate_cloudflare()?;
        Ok(())
    }

    fn validate_meta(&self) -> Result<()> {
        if self.cluster.name.trim().is_empty() {
            return Err(ConfigError::invalid("cluster.name", "must not be empty"));
        }
        Ok(())
    }

    fn validate_networks(&self) -> Result<()> {
        if self.networks.is_empty() {
            return Err(ConfigError::invalid("networks", "at least one is required"));
        }

        let mut private = 0;
        for (name, net) in &self.networks {
            let at = format!("networks.{name}");
            match net.kind {
                NetworkKind::Private => {
                    private += 1;
                    if net.subnet.is_none() {
                        return Err(ConfigError::invalid(
                            at,
                            "a private network needs a subnet holding the node private IPs",
                        ));
                    }
                }
                NetworkKind::Public => {
                    if net.subnet.is_some() {
                        return Err(ConfigError::invalid(
                            at,
                            "a public network must not declare a subnet",
                        ));
                    }
                }
            }
            if let Some(domain) = &net.domain {
                if domain.trim().is_empty() || domain.starts_with('.') || domain.ends_with('.') {
                    return Err(ConfigError::invalid(
                        format!("networks.{name}.domain"),
                        "must be a bare domain name",
                    ));
                }
            }
        }

        if private == 0 {
            return Err(ConfigError::invalid(
                "networks",
                "at least one private network is required for node to node traffic",
            ));
        }

        // Two networks owning the same domain makes record ownership ambiguous.
        let mut seen = BTreeSet::new();
        for (name, net) in &self.networks {
            if let Some(domain) = &net.domain {
                if !seen.insert(domain.to_ascii_lowercase()) {
                    return Err(ConfigError::invalid(
                        format!("networks.{name}.domain"),
                        format!("domain {domain} is already claimed by another network"),
                    ));
                }
            }
        }

        Ok(())
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

        let mut ips = BTreeSet::new();
        for (name, node) in &self.nodes {
            let at = format!("nodes.{name}");

            if !ips.insert(node.private_ip) {
                return Err(ConfigError::invalid(
                    format!("{at}.private_ip"),
                    format!("{} is used by another node", node.private_ip),
                ));
            }

            if self.private_network_of(node).is_none() {
                return Err(ConfigError::invalid(
                    format!("{at}.private_ip"),
                    format!(
                        "{} is not inside any declared private network subnet",
                        node.private_ip
                    ),
                ));
            }

            if let Some(public) = &node.public {
                if public.is_empty() {
                    return Err(ConfigError::invalid(
                        format!("{at}.public"),
                        "set at least one of ipv4, ipv6 or domain",
                    ));
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

        // A drive nobody implements can never host a volume, so it is a typo.
        for drive in self.drives.keys() {
            if !self.nodes.values().any(|n| n.implements(drive)) {
                return Err(ConfigError::invalid(
                    format!("drives.{drive}"),
                    "no node implements this drive",
                ));
            }
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
        match &net.domain {
            None => Err(ConfigError::invalid(
                "web.network",
                format!(
                    "network {} has no domain, so {} cannot be published on it",
                    self.web.network, self.web.address
                ),
            )),
            Some(domain) if !is_within(&self.web.address, domain) => Err(ConfigError::invalid(
                "web.address",
                format!("{} is not under {domain}", self.web.address),
            )),
            Some(_) => Ok(()),
        }
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
        if !cf.api_token_file.is_absolute() {
            return Err(ConfigError::invalid(
                "cloudflare.api_token_file",
                "must be an absolute path",
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
        Ok(())
    }
}

/// True when `name` is the domain itself or a subdomain of it.
fn is_within(name: &str, domain: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    name == domain || name.ends_with(&format!(".{domain}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
cluster: { name: home }
networks:
  lan:  { type: private, subnet: 10.10.0.0/24, domain: int.example.com }
  wan:  { type: public,  domain: example.com }
drives:
  fast: { sync_interval: 5m }
nodes:
  n1:
    role: manager
    private_ip: 10.10.0.1
    public: { ipv4: 203.0.113.5 }
    drives: { fast: /srv/clouduler/fast }
  n2:
    role: worker
    private_ip: 10.10.0.2
    drives: { fast: /mnt/fast }
web: { address: cloud.int.example.com, network: lan }
cloudflare:
  api_token_file: /etc/clouduler/cf.token
  zones: [example.com]
"#;

    fn parse_with(patch: &str) -> Result<ClusterConfig> {
        ClusterConfig::parse(&format!("{BASE}{patch}"))
    }

    #[test]
    fn parses_reference_config() {
        let cfg = ClusterConfig::parse(BASE).expect("base config is valid");
        assert_eq!(cfg.cluster.name, "home");
        assert_eq!(cfg.nodes.len(), 2);
        assert_eq!(cfg.managers().count(), 1);
        assert_eq!(
            cfg.drives["fast"].sync_interval,
            std::time::Duration::from_secs(300)
        );
        assert_eq!(
            cfg.nodes["n1"].rpc_addr().port(),
            crate::cluster::DEFAULT_RPC_PORT
        );
    }

    #[test]
    fn node_private_ip_must_be_in_a_private_subnet() {
        let err = parse_with("\n").err();
        assert!(err.is_none());

        let cfg = BASE.replace("private_ip: 10.10.0.2", "private_ip: 192.168.5.2");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("not inside any declared private network"));
    }

    #[test]
    fn private_ips_must_be_unique() {
        let cfg = BASE.replace("private_ip: 10.10.0.2", "private_ip: 10.10.0.1");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("used by another node"));
    }

    #[test]
    fn private_network_requires_a_subnet() {
        let cfg = BASE.replace("subnet: 10.10.0.0/24, ", "");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("needs a subnet"));
    }

    #[test]
    fn drive_references_must_resolve() {
        let cfg = BASE.replace("drives: { fast: /mnt/fast }", "drives: { bulk: /mnt/bulk }");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("no drive named bulk"));
    }

    #[test]
    fn drive_paths_must_be_absolute() {
        let cfg = BASE.replace("fast: /mnt/fast", "fast: mnt/fast");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn unimplemented_drive_is_rejected() {
        let cfg = BASE.replace("  fast: { sync_interval: 5m }", "  fast: { sync_interval: 5m }\n  bulk: {}");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("no node implements this drive"));
    }

    #[test]
    fn web_address_must_sit_under_its_network_domain() {
        let cfg = BASE.replace("cloud.int.example.com", "cloud.other.com");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("is not under int.example.com"));
    }

    #[test]
    fn network_domain_must_be_covered_by_a_zone() {
        let cfg = BASE.replace("zones: [example.com]", "zones: [other.com]");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
        assert!(err.to_string().contains("not covered by any configured cloudflare zone"));
    }

    #[test]
    fn cluster_needs_a_manager() {
        let cfg = BASE.replace("role: manager", "role: worker");
        let err = ClusterConfig::parse(&cfg).unwrap_err();
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
        let want = vec!["fast".to_string()];
        assert_eq!(cfg.nodes_implementing(&want).count(), 2);
        let want = vec!["fast".to_string(), "bulk".to_string()];
        assert_eq!(cfg.nodes_implementing(&want).count(), 0);
    }

    #[test]
    fn is_within_matches_subdomains_only() {
        assert!(is_within("a.example.com", "example.com"));
        assert!(is_within("example.com", "example.com"));
        assert!(!is_within("notexample.com", "example.com"));
        assert!(!is_within("example.com.evil.net", "example.com"));
    }
}
