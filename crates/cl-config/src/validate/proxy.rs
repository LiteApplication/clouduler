use crate::cluster::{ClusterConfig, NetworkKind, ProxyAuthConfig};
use crate::error::{ConfigError, Result};
use crate::names::is_within;

impl ClusterConfig {
    pub(super) fn validate_proxy(&self) -> Result<()> {
        if !self.proxy.enabled {
            return Ok(());
        }
        check_dns_server(self)?;
        check_listen_ports(self)?;
        if self.proxy.acme.enabled && self.cloudflare.is_none() {
            return Err(ConfigError::invalid(
                "proxy.acme.enabled",
                "certificate issuance is DNS-01 only, so it needs a cloudflare section",
            ));
        }
        if let Some(auth) = &self.proxy.auth {
            check_auth(self, auth)?;
        }
        Ok(())
    }

    pub(super) fn validate_cloudflare(&self) -> Result<()> {
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
            if net.kind == NetworkKind::Container {
                continue;
            }
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

fn check_dns_server(cfg: &ClusterConfig) -> Result<()> {
    let dns = &cfg.proxy.dns_server;
    if dns.port == 0 {
        return Err(ConfigError::invalid(
            "proxy.dns_server.port",
            "must not be 0",
        ));
    }
    for (i, ip) in dns.upstream.iter().enumerate() {
        if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
            return Err(ConfigError::invalid(
                "proxy.dns_server.upstream",
                format!("{ip} is not a resolver address"),
            ));
        }
        if dns.upstream[..i].contains(ip) {
            return Err(ConfigError::invalid(
                "proxy.dns_server.upstream",
                format!("{ip} is listed twice"),
            ));
        }
    }
    for network in &dns.networks {
        let Some(net) = cfg.networks.get(network) else {
            return Err(ConfigError::invalid(
                "proxy.dns_server.networks",
                format!("no network named {network}"),
            ));
        };
        if !matches!(
            net.kind,
            NetworkKind::Internal
                | NetworkKind::Private
                | NetworkKind::Public
                | NetworkKind::Container
        ) {
            return Err(ConfigError::invalid(
                "proxy.dns_server.networks",
                format!("{network} has no node address to listen on"),
            ));
        }
        if net
            .container
            .as_ref()
            .is_some_and(|c| net.kind == NetworkKind::Container && !c.proxy)
        {
            return Err(ConfigError::invalid(
                "proxy.dns_server.networks",
                format!("{network} has no node address to listen on without proxy: true"),
            ));
        }
    }
    Ok(())
}

fn check_listen_ports(cfg: &ClusterConfig) -> Result<()> {
    let proxy = &cfg.proxy;
    if proxy.http_port == proxy.https_port {
        return Err(ConfigError::invalid(
            "proxy.https_port",
            "must differ from proxy.http_port",
        ));
    }
    // Not fatal on its own: the api holds web.port on the same addresses,
    // so the https listener would fail to bind and the plain one still
    // works. Refusing here says so at `check` time instead.
    if proxy.https_port == cfg.web.port {
        return Err(ConfigError::invalid(
            "proxy.https_port",
            format!("web.port already listens on {}", cfg.web.port),
        ));
    }
    if proxy.http_port == cfg.web.port {
        return Err(ConfigError::invalid(
            "proxy.http_port",
            format!("web.port already listens on {}", cfg.web.port),
        ));
    }
    // Only a clash on this node's loopback, but that is where the proxy
    // sends dashboard traffic, so it would forward to itself.
    for (field, port) in [
        ("proxy.http_port", proxy.http_port),
        ("proxy.https_port", proxy.https_port),
    ] {
        if port == cfg.web.backend_port {
            return Err(ConfigError::invalid(
                field,
                format!("web.backend_port already listens on {port}"),
            ));
        }
    }
    Ok(())
}

fn check_auth(cfg: &ClusterConfig, auth: &ProxyAuthConfig) -> Result<()> {
    check_auth_identity(auth)?;
    check_auth_networks(cfg, auth)?;
    check_auth_remember_ip(cfg, auth)?;
    if auth.cloudflare_proxy
        && !auth.networks.iter().any(|n| {
            cfg.networks
                .get(n)
                .is_some_and(|c| c.kind == NetworkKind::Public)
        })
    {
        return Err(ConfigError::invalid(
            "proxy.auth.cloudflare_proxy",
            "needs at least one public network in proxy.auth.networks",
        ));
    }
    check_auth_ttls_and_accent(auth)
}

fn check_auth_identity(auth: &ProxyAuthConfig) -> Result<()> {
    if !auth.issuer.starts_with("https://") {
        return Err(ConfigError::invalid(
            "proxy.auth.issuer",
            "must be an https url",
        ));
    }
    if auth.client_id.trim().is_empty() {
        return Err(ConfigError::invalid(
            "proxy.auth.client_id",
            "must not be empty",
        ));
    }
    if !auth.scopes.iter().any(|s| s == "openid") {
        return Err(ConfigError::invalid(
            "proxy.auth.scopes",
            "must include openid",
        ));
    }
    if auth.host.trim().is_empty() {
        return Err(ConfigError::invalid("proxy.auth.host", "must not be empty"));
    }
    Ok(())
}

fn check_auth_networks(cfg: &ClusterConfig, auth: &ProxyAuthConfig) -> Result<()> {
    if auth.networks.is_empty() {
        return Err(ConfigError::invalid(
            "proxy.auth.networks",
            "list at least one network that offers the login",
        ));
    }
    let mut portals = std::collections::BTreeSet::new();
    for network in &auth.networks {
        let Some(net) = cfg.networks.get(network) else {
            return Err(ConfigError::invalid(
                "proxy.auth.networks",
                format!("no network named {network}"),
            ));
        };
        if net.is_managed() && cfg.addressed_nodes_on(network).is_empty() {
            return Err(ConfigError::invalid(
                "proxy.auth.networks",
                format!(
                    "{network} is a managed network and no node has an IP address on it; the login page is served from node addresses, so give at least one node an address there or pick another network"
                ),
            ));
        }
        if net.domain.is_none() {
            return Err(ConfigError::invalid(
                "proxy.auth.networks",
                format!("network {network} has no domain, so it cannot serve a login page"),
            ));
        }
        let fqdn = auth.fqdn(cfg, network);
        if fqdn == cfg.web_fqdn() {
            return Err(ConfigError::invalid(
                "proxy.auth.host",
                "must differ from the dashboard hostname",
            ));
        }
        if !portals.insert(fqdn.clone()) {
            return Err(ConfigError::invalid(
                "proxy.auth.networks",
                format!(
                    "{network} and another listed network resolve the login to the same hostname {fqdn}; \
                     use networks with different domains"
                ),
            ));
        }
    }
    Ok(())
}

fn check_auth_remember_ip(cfg: &ClusterConfig, auth: &ProxyAuthConfig) -> Result<()> {
    for network in &auth.remember_ip {
        if !auth.covers(network) {
            return Err(ConfigError::invalid(
                "proxy.auth.remember_ip",
                format!("{network} is not listed in proxy.auth.networks"),
            ));
        }
        if !matches!(
            cfg.networks[network].kind,
            NetworkKind::Internal | NetworkKind::Private | NetworkKind::Public
        ) {
            return Err(ConfigError::invalid(
                "proxy.auth.remember_ip",
                format!("{network} has no clients of its own to recognise by address"),
            ));
        }
    }
    Ok(())
}

fn check_auth_ttls_and_accent(auth: &ProxyAuthConfig) -> Result<()> {
    if auth.remember_ip_ttl.is_zero() {
        return Err(ConfigError::invalid(
            "proxy.auth.remember_ip_ttl",
            "must be greater than zero",
        ));
    }
    if auth.session_ttl.is_zero() {
        return Err(ConfigError::invalid(
            "proxy.auth.session_ttl",
            "must be greater than zero",
        ));
    }
    let accent = auth
        .accent
        .as_deref()
        .map(|a| a.strip_prefix('#').unwrap_or(""));
    if accent
        .is_some_and(|a| !matches!(a.len(), 3 | 6) || !a.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(ConfigError::invalid(
            "proxy.auth.accent",
            "must be a hex colour like #e5603c",
        ));
    }
    Ok(())
}
