use super::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Hostname the UI is served on, always over HTTPS. A bare label gets the
    /// network domain appended.
    pub address: String,
    /// Network the hostname resolves through, picking which node address the
    /// record points at.
    pub network: String,
    #[serde(default = "default_web_port")]
    pub port: u16,

    /// Publish the dashboard as a route on the reverse proxy (standard HTTPS
    /// port, same certificate and DNS record as other services).
    ///
    /// The direct listener on `port` stays up either way, as the way back in when
    /// the proxy cannot bind or a config edit breaks routing.
    #[serde(default = "default_true_bool")]
    pub through_proxy: bool,

    /// Loopback port the proxy forwards dashboard traffic to.
    ///
    /// The proxy speaks plain HTTP to backends, so the api opens a second
    /// listener, bound to 127.0.0.1 only: the overlay is unencrypted and the
    /// dashboard carries session cookies. Each node's proxy reaches its own api.
    #[serde(default = "default_web_backend_port")]
    pub backend_port: u16,
}

/// Loopback port the api opens for the reverse proxy.
pub const DEFAULT_WEB_BACKEND_PORT: u16 = 8444;

/// The reverse proxy in front of every `x-clouduler.proxy` route.
///
/// Both listeners bind on every node; a request landing anywhere is forwarded
/// to wherever the container runs. `dns_publish` only decides what the public
/// DNS answer says.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
    /// Plain HTTP. Serves `tls: false` routes and redirects the rest.
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// TLS. Must differ from `web.port`, which the api holds on the same
    /// addresses; the https listener is skipped when they collide.
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default)]
    pub acme: AcmeConfig,
    /// How a proxied hostname's DNS record is published.
    #[serde(default)]
    pub dns_publish: DnsPublishMode,
    /// The client-facing DNS server, answering per the network the client is on.
    #[serde(default)]
    pub dns_server: DnsServerConfig,
    /// Login for routes carrying the `authenticated` middleware. A separate OIDC
    /// client (and group namespace) from `auth.oidc`, which is for operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProxyAuthConfig>,
}

/// The login portal behind the `authenticated` route middleware.
///
/// The portal is a hostname the proxy answers itself, one per network in
/// `networks`. Logins are per network, and a route may only ask for a login
/// on a listed network. An app without a valid cookie redirects to the
/// portal, which runs the OIDC login and sends the browser back through a
/// one-time-token URL that sets the app's cookie. Sessions are replicated in
/// raft, so any node accepts any login.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct ProxyAuthConfig {
    /// Portal hostname. A bare label gets each network's domain appended.
    pub host: String,
    /// Networks that offer the login. A route with the `authenticated`
    /// middleware must be published on one of these.
    pub networks: Vec<String>,
    pub issuer: String,
    pub client_id: String,
    /// The client secret is a sealed cluster secret named
    /// `_clouduler_proxy_oidc_client_secret`, never part of this file.
    #[serde(default = "default_proxy_auth_scopes")]
    pub scopes: Vec<String>,
    /// Claim listing the user's groups. Routes restrict access by naming
    /// groups from here.
    #[serde(default = "default_admin_claim")]
    pub groups_claim: String,
    /// Claim shown as the user's name. Falls back to `sub`.
    #[serde(default = "default_username_claim")]
    pub username_claim: String,
    /// How long a login lasts, for the portal and every app cookie it hands out.
    /// An app cookie never outlives the portal session.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_session_ttl")]
    pub session_ttl: Duration,
    /// Heading of the login page.
    #[serde(default = "default_portal_title")]
    pub title: String,
    /// Colour set of the login page's shapes and background.
    #[serde(default)]
    pub palette: PortalPalette,
    /// Overrides the palette's main colour (buttons and the biggest shape),
    /// as a CSS hex colour like `#e5603c`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Networks (subset of `networks`) whose login page offers "remember this IP
    /// address": a visitor inside that network may skip login until
    /// `remember_ip_ttl` passes. Empty means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remember_ip: Vec<String>,
    /// How long a remembered address stays trusted, from the login that saved it.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    #[serde(with = "humantime_serde", default = "default_remember_ip_ttl")]
    pub remember_ip_ttl: Duration,
    /// Publish the portal hostname's DNS records through Cloudflare's proxy
    /// (orange cloud) on the public networks in `networks`; non-public ones
    /// are unaffected. Needs at least one public network listed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cloudflare_proxy: bool,
}

/// Colour sets for the login page: a dark background with a few loud shapes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "kebab-case")]
pub enum PortalPalette {
    /// Deep grape with pink and lime.
    #[default]
    Grape,
    /// Warm night with orange and sky blue.
    Sunset,
    /// Deep sea with cyan and coral.
    Ocean,
    /// Pine green with yellow and pink.
    Forest,
}

impl ProxyAuthConfig {
    /// The portal's fully qualified, lowercase hostname on `network`.
    pub fn fqdn(&self, cfg: &ClusterConfig, network: &str) -> String {
        qualify(
            &self.host,
            cfg.networks.get(network).and_then(|n| n.domain.as_deref()),
        )
    }

    /// Every portal hostname with the network it serves.
    pub fn portals(&self, cfg: &ClusterConfig) -> Vec<(String, String)> {
        self.networks
            .iter()
            .map(|n| (n.clone(), self.fqdn(cfg, n)))
            .collect()
    }

    /// Does this network offer the login?
    pub fn covers(&self, network: &str) -> bool {
        self.networks.iter().any(|n| n == network)
    }

    /// Does `network` let visitors save their IP address at login?
    pub fn remembers_ip(&self, network: &str) -> bool {
        self.covers(network) && self.remember_ip.iter().any(|n| n == network)
    }
}

pub(super) fn is_globally_routable(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (o[0] == 100 && (64..128).contains(&o[1])))
        }
        std::net::IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            http_port: default_http_port(),
            https_port: default_https_port(),
            acme: AcmeConfig::default(),
            dns_publish: DnsPublishMode::default(),
            dns_server: DnsServerConfig::default(),
            auth: None,
        }
    }
}

/// A DNS server on the nodes' own addresses that answers cluster hostnames by
/// the network the client is on. Hostnames on a network the client cannot
/// reach are refused; other names are forwarded upstream for clients inside a
/// subnetted network and refused otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct DnsServerConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Networks to listen on, at this node's address on each. Empty means every
    /// subnetted network the node has an address on plus every container network
    /// with `proxy: true`; a public network is only served when listed.
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default = "default_dns_port")]
    pub port: u16,
    /// Upstream resolvers, tried in order on port 53. Empty means the host's
    /// (`/etc/resolv.conf`) minus Tailscale MagicDNS (split DNS can loop back to
    /// this server), or `1.1.1.1`/`1.0.0.1` when MagicDNS is all there is.
    #[serde(default)]
    pub upstream: Vec<std::net::IpAddr>,
}

impl Default for DnsServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            networks: Vec::new(),
            port: default_dns_port(),
            upstream: Vec::new(),
        }
    }
}

/// Who a proxied hostname's DNS record names.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "kebab-case")]
pub enum DnsPublishMode {
    /// Every node's address is published for every proxied hostname (like
    /// Swarm's routing mesh). Safe default; needs no placement co-location.
    #[default]
    AllNodes,
    /// Only the node(s) running the containers behind a hostname are published;
    /// stale records elsewhere are deleted. Requires every container publishing
    /// the same hostname to land on one node (an implicit scheduler constraint).
    OwningNode,
}

/// Certificate issuance for proxy routes. Needs `cloudflare`, since the only
/// challenge implemented is DNS-01 against Cloudflare.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    /// Off by default: issuing against production Let's Encrypt from an
    /// unreachable cluster burns rate limit.
    #[serde(default)]
    pub enabled: bool,
    /// ACME directory. Left out for Let's Encrypt production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_url: Option<String>,
    /// Contact address registered with the CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_email: Option<String>,
}

impl ClusterConfig {
    /// The UI hostname with the network domain appended when it is a bare label.
    pub fn web_fqdn(&self) -> String {
        qualify(
            &self.web.address,
            self.networks
                .get(&self.web.network)
                .and_then(|n| n.domain.as_deref()),
        )
    }
}
