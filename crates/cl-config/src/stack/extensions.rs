//! `x-clouduler`: clouduler's own extensions to the compose format. See
//! `docs/stacks.md` for the user facing reference.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::stack::port::Protocol;

/// Named behaviours a proxy route can layer on.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Middleware {
    RedirectHttps,
    StripPrefix,
    BasicAuth,
    Headers,
}

/// One HTTP route published through the reverse proxy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProxyRoute {
    /// Bare label gets the network's domain appended.
    pub host: String,
    /// Declared network this route is published on.
    pub network: String,
    /// Container port traffic is forwarded to.
    pub port: u16,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default = "default_true")]
    pub tls: bool,
    #[serde(default)]
    pub middlewares: Vec<Middleware>,
    #[serde(default)]
    pub sticky: Option<bool>,
}

fn default_true() -> bool {
    true
}

/// A non HTTP port routed over the overlay rather than the reverse proxy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExposePort {
    pub port: u16,
    pub published: u16,
    #[serde(default)]
    pub protocol: Protocol,
    pub network: String,
}

/// Per volume override of the drive's default sync interval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct VolumeSyncOverride {
    #[serde(default, with = "humantime_serde::option")]
    pub interval: Option<Duration>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Clouduler extensions on a single service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServiceExtensions {
    #[serde(default)]
    pub proxy: Vec<ProxyRoute>,
    #[serde(default)]
    pub expose: Vec<ExposePort>,
    #[serde(default)]
    pub volume_sync: BTreeMap<String, VolumeSyncOverride>,
    #[serde(default)]
    pub auto_update: Option<bool>,
    #[serde(default)]
    pub node: Option<String>,
}

/// Stack wide defaults under the top level `x-clouduler` key. Services
/// inherit these for any field they don't set themselves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct StackDefaults {
    #[serde(default)]
    pub auto_update: Option<bool>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub volume_sync: BTreeMap<String, VolumeSyncOverride>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_route() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
proxy:
  - host: gitlab
    network: wan
    port: 80
    middlewares: [redirect_https, strip_prefix]
    sticky: true
"#,
        )
        .unwrap();
        let route = &ext.proxy[0];
        assert_eq!(route.host, "gitlab");
        assert_eq!(route.network, "wan");
        assert_eq!(route.port, 80);
        assert!(route.tls);
        assert_eq!(route.middlewares, vec![Middleware::RedirectHttps, Middleware::StripPrefix]);
        assert_eq!(route.sticky, Some(true));
    }

    #[test]
    fn parses_expose() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
expose:
  - port: 22
    published: 2222
    protocol: tcp
    network: tailscale
"#,
        )
        .unwrap();
        let e = &ext.expose[0];
        assert_eq!(e.port, 22);
        assert_eq!(e.published, 2222);
        assert_eq!(e.protocol, Protocol::Tcp);
    }

    #[test]
    fn parses_volume_sync() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
volume_sync:
  gitlab-data:
    interval: 1h
    enabled: false
"#,
        )
        .unwrap();
        let o = &ext.volume_sync["gitlab-data"];
        assert_eq!(o.interval, Some(Duration::from_secs(3600)));
        assert_eq!(o.enabled, Some(false));
    }

    #[test]
    fn parses_auto_update_and_node() {
        let ext: ServiceExtensions = serde_yaml::from_str("auto_update: true\nnode: n1\n").unwrap();
        assert_eq!(ext.auto_update, Some(true));
        assert_eq!(ext.node.as_deref(), Some("n1"));
    }

    #[test]
    fn unknown_key_rejected() {
        let err = serde_yaml::from_str::<ServiceExtensions>("bogus: 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }
}
