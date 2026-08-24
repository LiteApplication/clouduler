//! `x-clouduler`: clouduler's own extensions to the compose format. See
//! `docs/stacks.md` for the user facing reference.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::stack::bytesize::ByteSize;
use crate::stack::port::Protocol;

/// Behaviours a proxy route can layer on.
///
/// Two forms are accepted. A bare string picks a middleware that needs no
/// settings (`redirect_https`, `strip_prefix`), and a single key map configures
/// one that does:
///
/// ```yaml
/// middlewares:
///   - redirect_https
///   - basic_auth:
///       users:
///         alice: "$2y$05$..."   # bcrypt, as in an htpasswd file
///   - headers:
///       request:
///         X-Real-Source: clouduler
///       remove_response:
///         - Server
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawMiddleware", into = "RawMiddleware")]
pub enum Middleware {
    /// Answer plain HTTP with a redirect to the same URL over HTTPS.
    RedirectHttps,
    /// Strip the route's `path_prefix` before forwarding.
    StripPrefix,
    BasicAuth(BasicAuth),
    Headers(Headers),
    Cors(CorsPolicy),
}

/// Wire form. serde_yaml renders an externally tagged enum as a yaml tag
/// (`!BasicAuth`), which nobody wants to write in a compose file, so the two
/// shapes are spelled out and mapped by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RawMiddleware {
    Bare(String),
    Configured(BTreeMap<String, serde_yaml::Value>),
}

impl TryFrom<RawMiddleware> for Middleware {
    type Error = String;

    fn try_from(raw: RawMiddleware) -> Result<Self, Self::Error> {
        match raw {
            RawMiddleware::Bare(name) => match name.as_str() {
                "redirect_https" => Ok(Middleware::RedirectHttps),
                "strip_prefix" => Ok(Middleware::StripPrefix),
                // Naming one that needs settings without giving any is a
                // mistake worth reporting rather than defaulting.
                "basic_auth" | "headers" | "cors" => {
                    Err(format!("middleware {name} needs settings, use a map"))
                }
                other => Err(format!("unknown middleware {other}")),
            },
            RawMiddleware::Configured(map) => {
                let mut entries = map.into_iter();
                let (name, value) = entries
                    .next()
                    .ok_or_else(|| "empty middleware entry".to_string())?;
                if entries.next().is_some() {
                    return Err(format!(
                        "middleware entry {name} must be the only key in its map"
                    ));
                }

                match name.as_str() {
                    "basic_auth" => serde_yaml::from_value(value)
                        .map(Middleware::BasicAuth)
                        .map_err(|e| format!("basic_auth: {e}")),
                    "headers" => serde_yaml::from_value(value)
                        .map(Middleware::Headers)
                        .map_err(|e| format!("headers: {e}")),
                    "cors" => serde_yaml::from_value(value)
                        .map(Middleware::Cors)
                        .map_err(|e| format!("cors: {e}")),
                    "redirect_https" => Ok(Middleware::RedirectHttps),
                    "strip_prefix" => Ok(Middleware::StripPrefix),
                    other => Err(format!("unknown middleware {other}")),
                }
            }
        }
    }
}

impl From<Middleware> for RawMiddleware {
    fn from(mw: Middleware) -> Self {
        let name = mw.name().to_string();
        match mw {
            Middleware::RedirectHttps | Middleware::StripPrefix => RawMiddleware::Bare(name),
            Middleware::BasicAuth(auth) => RawMiddleware::Configured(BTreeMap::from([(
                name,
                serde_yaml::to_value(auth).unwrap_or(serde_yaml::Value::Null),
            )])),
            Middleware::Headers(headers) => RawMiddleware::Configured(BTreeMap::from([(
                name,
                serde_yaml::to_value(headers).unwrap_or(serde_yaml::Value::Null),
            )])),
            Middleware::Cors(cors) => RawMiddleware::Configured(BTreeMap::from([(
                name,
                serde_yaml::to_value(cors).unwrap_or(serde_yaml::Value::Null),
            )])),
        }
    }
}

impl Middleware {
    /// Stable name, for logs and for the ui.
    pub fn name(&self) -> &'static str {
        match self {
            Middleware::RedirectHttps => "redirect_https",
            Middleware::StripPrefix => "strip_prefix",
            Middleware::BasicAuth(_) => "basic_auth",
            Middleware::Headers(_) => "headers",
            Middleware::Cors(_) => "cors",
        }
    }
}

/// Cross-origin access control for one route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CorsPolicy {
    /// Origins allowed to read the response. `["*"]` allows any origin, but
    /// is rejected together with `allow_credentials`: a browser refuses to
    /// pair a wildcard origin with credentials, so the combination could
    /// only ever surface as a confusing runtime failure.
    pub origins: Vec<String>,
    #[serde(default = "default_cors_methods", skip_serializing_if = "is_default_cors_methods")]
    pub methods: Vec<String>,
    /// Headers a request may set. Empty means whatever the browser asked for
    /// in `Access-Control-Request-Headers` is echoed back, which is the
    /// common case and avoids listing every header a client might send.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_credentials: bool,
    /// How long a browser may cache a preflight result.
    #[serde(default, with = "humantime_serde::option", skip_serializing_if = "Option::is_none")]
    pub max_age: Option<Duration>,
}

fn default_cors_methods() -> Vec<String> {
    ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn is_default_cors_methods(methods: &[String]) -> bool {
    methods == default_cors_methods().as_slice()
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_true(b: &bool) -> bool {
    *b
}

/// HTTP basic auth in front of a route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BasicAuth {
    /// Username to password hash. Hashes only: a plaintext password in a stack
    /// file would end up in the cluster database and in everyone's shell
    /// history.
    pub users: BTreeMap<String, String>,
    #[serde(default = "default_realm", skip_serializing_if = "is_default_realm")]
    pub realm: String,
}

fn is_default_realm(realm: &str) -> bool {
    realm == default_realm()
}

fn default_realm() -> String {
    "clouduler".to_string()
}

/// Header rewriting in both directions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Headers {
    /// Set on the request going to the container.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub request: BTreeMap<String, String>,
    /// Set on the response going back to the client.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub response: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_request: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_response: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub tls: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub middlewares: Vec<Middleware>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky: Option<bool>,

    /// Largest request body to forward, e.g. `10MB`. Anything bigger is
    /// answered with 413 and never reaches the container. Unset means no
    /// limit, which is the right default for an upload-heavy service and the
    /// wrong one for anything exposed to the internet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_size: Option<ByteSize>,

    /// Let `Upgrade` requests through, which is what a websocket needs. On by
    /// default: a route that does not use websockets is unaffected, and one
    /// that does would otherwise fail in a way that is tedious to diagnose.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub websockets: bool,

    /// Forward the client's `Host` header instead of rewriting it to the
    /// backend's address. Off by default because most containers do not care,
    /// but anything that builds absolute URLs or does virtual hosting needs it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub preserve_host: bool,

    /// How long to wait for the backend's response headers, connection time
    /// included. Unset waits forever, which ties up a connection when a
    /// container wedges.
    #[serde(default, with = "humantime_serde::option", skip_serializing_if = "Option::is_none")]
    pub response_timeout: Option<Duration>,
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
    #[serde(default, skip_serializing_if = "is_default_protocol")]
    pub protocol: Protocol,
    pub network: String,
}

fn is_default_protocol(p: &Protocol) -> bool {
    *p == Protocol::default()
}

/// Per volume override of the drive's default sync interval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct VolumeSyncOverride {
    #[serde(default, with = "humantime_serde::option", skip_serializing_if = "Option::is_none")]
    pub interval: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Clouduler extensions on a single service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServiceExtensions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proxy: Vec<ProxyRoute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose: Vec<ExposePort>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub volume_sync: BTreeMap<String, VolumeSyncOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Stack wide defaults under the top level `x-clouduler` key. Services
/// inherit these for any field they don't set themselves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct StackDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
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
    fn parses_the_reverse_proxy_settings() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
proxy:
  - host: files
    network: wan
    port: 80
    max_body_size: 10MB
    websockets: false
    preserve_host: true
    response_timeout: 30s
"#,
        )
        .expect("parse");
        let route = &ext.proxy[0];
        assert_eq!(route.max_body_size, Some(ByteSize(10_000_000)));
        assert!(!route.websockets);
        assert!(route.preserve_host);
        assert_eq!(route.response_timeout, Some(Duration::from_secs(30)));
    }

    /// Every new knob is optional, and the defaults have to be the behaviour
    /// that existed before they were added: no cap, no timeout, host rewritten,
    /// and websockets working without anyone asking for them.
    #[test]
    fn the_proxy_settings_default_to_the_previous_behaviour() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            "proxy:\n  - host: site\n    network: wan\n    port: 80\n",
        )
        .expect("parse");
        let route = &ext.proxy[0];
        assert_eq!(route.max_body_size, None);
        assert!(route.websockets, "websockets must stay on by default");
        assert!(!route.preserve_host);
        assert_eq!(route.response_timeout, None);
    }

    /// The settings are stack-file settings: this asserts they survive a real
    /// stack file end to end, not just a hand written fragment.
    #[test]
    fn the_example_stack_carries_its_proxy_settings() {
        let raw = include_str!("../../../../examples/stacks/gitlab.yaml");
        let file: serde_yaml::Value = serde_yaml::from_str(raw).expect("example parses");
        let ext = &file["services"]["gitlab"]["x-clouduler"];
        let ext: ServiceExtensions = serde_yaml::from_value(ext.clone()).expect("extensions parse");
        let route = &ext.proxy[0];
        assert_eq!(route.max_body_size, Some(ByteSize(2 * 1024 * 1024 * 1024)));
        assert!(route.preserve_host);
        assert!(route.websockets);
        assert_eq!(route.response_timeout, Some(Duration::from_secs(120)));
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

    #[test]
    fn bare_and_configured_middlewares_both_parse() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
proxy:
  - host: site
    network: wan
    port: 80
    middlewares:
      - redirect_https
      - strip_prefix
      - basic_auth:
          users:
            alice: "$2y$05$abcdefghijklmnopqrstuv"
      - headers:
          request:
            X-Real-Source: clouduler
          remove_response:
            - Server
"#,
        )
        .expect("parse");

        let mw = &ext.proxy[0].middlewares;
        assert_eq!(
            mw.iter().map(|m| m.name()).collect::<Vec<_>>(),
            vec!["redirect_https", "strip_prefix", "basic_auth", "headers"]
        );

        match &mw[2] {
            Middleware::BasicAuth(auth) => {
                assert!(auth.users.contains_key("alice"));
                // The realm has a sensible default so it can be left out.
                assert_eq!(auth.realm, "clouduler");
            }
            other => panic!("expected basic_auth, got {other:?}"),
        }

        match &mw[3] {
            Middleware::Headers(headers) => {
                assert_eq!(
                    headers.request.get("X-Real-Source").map(String::as_str),
                    Some("clouduler")
                );
                assert_eq!(headers.remove_response, vec!["Server"]);
                assert!(headers.response.is_empty());
            }
            other => panic!("expected headers, got {other:?}"),
        }
    }

    #[test]
    fn cors_middleware_parses_with_defaults() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
proxy:
  - host: api
    network: wan
    port: 80
    middlewares:
      - cors:
          origins: ["https://app.example.com"]
"#,
        )
        .expect("parse");
        match &ext.proxy[0].middlewares[0] {
            Middleware::Cors(cors) => {
                assert_eq!(cors.origins, vec!["https://app.example.com"]);
                assert_eq!(cors.methods, default_cors_methods());
                assert!(cors.headers.is_empty());
                assert!(!cors.allow_credentials);
                assert_eq!(cors.max_age, None);
            }
            other => panic!("expected cors, got {other:?}"),
        }
    }

    #[test]
    fn bare_cors_is_rejected() {
        let err = serde_yaml::from_str::<ServiceExtensions>(
            "proxy:\n  - host: api\n    network: wan\n    port: 80\n    middlewares: [cors]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("needs settings"), "{err}");
    }

    #[test]
    fn middleware_config_rejects_unknown_keys() {
        let err = serde_yaml::from_str::<ServiceExtensions>(
            r#"
proxy:
  - host: site
    network: wan
    port: 80
    middlewares:
      - basic_auth:
          users: {}
          bogus: 1
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn middlewares_round_trip() {
        let original = Middleware::Headers(Headers {
            request: BTreeMap::from([("A".to_string(), "b".to_string())]),
            ..Default::default()
        });
        let encoded = serde_yaml::to_string(&original).expect("encode");
        let decoded: Middleware = serde_yaml::from_str(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }
}
