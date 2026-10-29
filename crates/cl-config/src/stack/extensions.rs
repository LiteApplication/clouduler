//! `x-clouduler`: clouduler's own extensions to the compose format. See
//! `docs/stacks.md` for the user facing reference.

use std::collections::BTreeMap;
use std::path::PathBuf;
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "RawMiddleware", into = "RawMiddleware")]
pub enum Middleware {
    /// Answer plain HTTP with a redirect to the same URL over HTTPS.
    RedirectHttps,
    /// Strip the route's `path_prefix` before forwarding.
    StripPrefix,
    BasicAuth(BasicAuth),
    Headers(Headers),
    Cors(CorsPolicy),
    ForwardAuth(ForwardAuth),
    RedirectRegex(RedirectRegex),
    /// Requires a session established through clouduler's own OIDC login
    /// before a request reaches the backend. Needs no settings here: the
    /// identity provider is the cluster-wide `auth.oidc` config already used
    /// for the web ui, not something declared per route.
    Authenticated,
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
                "authenticated" => Ok(Middleware::Authenticated),
                // Naming one that needs settings without giving any is a
                // mistake worth reporting rather than defaulting.
                "basic_auth" | "headers" | "cors" | "forward_auth" | "redirect_regex" => {
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
                    "forward_auth" => {
                        let auth: ForwardAuth = serde_yaml::from_value(value)
                            .map_err(|e| format!("forward_auth: {e}"))?;
                        if !auth.address.starts_with("http://")
                            && !auth.address.starts_with("https://")
                        {
                            return Err(format!(
                                "forward_auth: address \"{}\" must start with http:// or https://",
                                auth.address
                            ));
                        }
                        Ok(Middleware::ForwardAuth(auth))
                    }
                    "redirect_regex" => {
                        let redirect: RedirectRegex = serde_yaml::from_value(value)
                            .map_err(|e| format!("redirect_regex: {e}"))?;
                        if let Err(e) = regex::Regex::new(&redirect.regex) {
                            return Err(format!(
                                "redirect_regex: invalid regex \"{}\": {e}",
                                redirect.regex
                            ));
                        }
                        Ok(Middleware::RedirectRegex(redirect))
                    }
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
            Middleware::RedirectHttps | Middleware::StripPrefix | Middleware::Authenticated => {
                RawMiddleware::Bare(name)
            }
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
            Middleware::ForwardAuth(auth) => RawMiddleware::Configured(BTreeMap::from([(
                name,
                serde_yaml::to_value(auth).unwrap_or(serde_yaml::Value::Null),
            )])),
            Middleware::RedirectRegex(redirect) => RawMiddleware::Configured(BTreeMap::from([(
                name,
                serde_yaml::to_value(redirect).unwrap_or(serde_yaml::Value::Null),
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
            Middleware::ForwardAuth(_) => "forward_auth",
            Middleware::RedirectRegex(_) => "redirect_regex",
            Middleware::Authenticated => "authenticated",
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
    #[serde(
        default = "default_cors_methods",
        skip_serializing_if = "is_default_cors_methods"
    )]
    pub methods: Vec<String>,
    /// Headers a request may set. Empty means whatever the browser asked for
    /// in `Access-Control-Request-Headers` is echoed back, which is the
    /// common case and avoids listing every header a client might send.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_credentials: bool,
    /// How long a browser may cache a preflight result.
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
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

/// Delegates authentication to an external HTTP endpoint before forwarding
/// the request. The endpoint is called with the original request's method,
/// URI and a subset of its headers; a 2xx response lets the request through,
/// anything else is relayed back to the client as-is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ForwardAuth {
    /// The auth endpoint's URL, e.g. `https://auth.example.com/verify`.
    pub address: String,
    /// Header names copied from the auth endpoint's response onto the
    /// request forwarded to the backend, e.g. `X-User`, so the backend can
    /// see the identity the auth endpoint established.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_response_headers: Vec<String>,
    /// How long to wait for the auth endpoint. Unset waits forever, which
    /// ties up the client's connection if it wedges.
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
}

/// Rewrites a request's URL via regex and redirects to the result, instead of
/// the fixed-string target a UI-managed redirect rule uses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RedirectRegex {
    /// Matched against the full request URL (`scheme://host/path?query`), not
    /// just the path, so a rewrite can also change the host.
    pub regex: String,
    /// The replacement, with `$1`, `$2`, ... substituted from the regex's
    /// capture groups.
    pub replacement: String,
    /// 301/308 (permanent) when true, 302/307 (temporary) when false.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub permanent: bool,
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
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
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
    /// Announce the real client to the backend with a PROXY protocol header
    /// before relaying anything else.
    ///
    /// A published port is forwarded in userspace (`cl_net::expose`), which
    /// means the backend sees a connection from *this node's* address on the
    /// container's bridge, never the client's. For most services that is
    /// merely a worse access log. For anything that makes decisions per
    /// client address — rate limits, fail2ban, allowlists — it is actively
    /// dangerous: every client looks like the same address, so one abusive
    /// client gets the relay itself banned and takes every other client down
    /// with it. That is not hypothetical; it is why this option exists.
    ///
    /// Off by default, and necessarily opt in: the header is extra bytes at
    /// the head of the stream, so a backend that is not expecting one sees a
    /// malformed request. **Enable it on the backend first.**
    ///
    /// TCP only. The relay rejects `proxy_protocol` on a UDP port rather
    /// than silently ignoring it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub proxy_protocol: bool,
}

fn is_default_protocol(p: &Protocol) -> bool {
    *p == Protocol::default()
}

/// Bind-mounts the host's docker socket into the container. Every other
/// mount in clouduler has to live on a drive so it can be replicated and
/// migrated (see `VolumeMount`), but the docker socket is neither data nor
/// something a drive could back — it's the one deliberate escape hatch, for
/// containers that need to control docker itself (CI runners, docker-in-docker
/// builds).
///
/// Granting this is equivalent to root on the host: anything with write
/// access to the socket can launch a privileged container and mount the host
/// filesystem into it, so only give it to services you already trust with
/// root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DockerSocket {
    /// Path the socket is mounted at inside the container.
    pub target: PathBuf,
    /// Read-write by default: most docker clients need to create and start
    /// containers, not just inspect what's running, so that's the useful
    /// default despite being the more dangerous one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
}

/// Per volume override of the drive's default sync interval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct VolumeSyncOverride {
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Name of one of this service's `actions`, run in its container on the
    /// primary node before each replication. A failure skips that
    /// replication. Service level only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Action run after each replication attempt, whether it succeeded or
    /// was skipped because `before` failed. Service level only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// Per service/stack override of whether a restic backup run stops this
/// service's containers first. `None` defers to the drive backup target's
/// own setting, then a hardcoded default of stopping - see
/// `StackFile::effective_backup_stop_before`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct BackupOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_before: Option<bool>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_socket: Option<DockerSocket>,
    /// Other services in this stack that must always land on the same node
    /// as this one. Transitively closed: naming `b` here and `c` on `b`
    /// still puts `a`, `b` and `c` in one group even though `a` never named
    /// `c` directly. See `StackFile::colocate_group`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub colocate: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupOverride>,
    /// Named commands an operator can run in this service's containers with
    /// one click. The key is the button's label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ServiceAction>,
}

/// A predefined command, run in a container when its action is triggered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceAction {
    /// A shell string (run as `sh -c`) or an exec array.
    pub cmd: super::Command,
    /// User to run as. Defaults to the container's own user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// How long to wait for the command, default 30s, at most 300s.
    #[serde(
        default,
        with = "humantime_serde::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    /// Ask for confirmation in the UI before running. Off by default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confirm: bool,
    /// Whether the action gets a button in the UI. Turn off for one that only
    /// exists to be a `volume_sync` `before`/`after` hook. On by default.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub show_in_ui: bool,
}

impl ServiceAction {
    pub fn argv(&self) -> Vec<String> {
        match &self.cmd {
            super::Command::Shell(s) => vec!["sh".to_string(), "-c".to_string(), s.clone()],
            super::Command::Exec(v) => v.clone(),
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupOverride>,
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
        assert_eq!(
            route.middlewares,
            vec![Middleware::RedirectHttps, Middleware::StripPrefix]
        );
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
        let ext: ServiceExtensions =
            serde_yaml::from_str("proxy:\n  - host: site\n    network: wan\n    port: 80\n")
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
    fn parses_docker_socket() {
        let ext: ServiceExtensions =
            serde_yaml::from_str("docker_socket:\n  target: /var/run/docker.sock\n").unwrap();
        let ds = ext.docker_socket.expect("docker_socket");
        assert_eq!(ds.target, PathBuf::from("/var/run/docker.sock"));
        assert!(!ds.read_only, "read-write is the default");
    }

    #[test]
    fn docker_socket_read_only_opt_in() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            "docker_socket:\n  target: /var/run/docker.sock\n  read_only: true\n",
        )
        .unwrap();
        assert!(ext.docker_socket.unwrap().read_only);
    }

    #[test]
    fn docker_socket_rejects_unknown_keys() {
        let err = serde_yaml::from_str::<ServiceExtensions>(
            "docker_socket:\n  target: /var/run/docker.sock\n  bogus: 1\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn parses_backup_stop_before() {
        let ext: ServiceExtensions =
            serde_yaml::from_str("backup:\n  stop_before: false\n").unwrap();
        assert_eq!(
            ext.backup,
            Some(BackupOverride {
                stop_before: Some(false)
            })
        );
    }

    #[test]
    fn backup_rejects_unknown_keys() {
        let err = serde_yaml::from_str::<ServiceExtensions>("backup:\n  bogus: 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn parses_colocate() {
        let ext: ServiceExtensions = serde_yaml::from_str("colocate: [db, cache]\n").unwrap();
        assert_eq!(ext.colocate, vec!["db".to_string(), "cache".to_string()]);
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
    fn forward_auth_parses_and_validates_the_address() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
proxy:
  - host: site
    network: wan
    port: 80
    middlewares:
      - forward_auth:
          address: https://auth.example.com/verify
          auth_response_headers: [X-User, X-Email]
          timeout: 5s
"#,
        )
        .expect("parse");
        match &ext.proxy[0].middlewares[0] {
            Middleware::ForwardAuth(auth) => {
                assert_eq!(auth.address, "https://auth.example.com/verify");
                assert_eq!(auth.auth_response_headers, vec!["X-User", "X-Email"]);
                assert_eq!(auth.timeout, Some(Duration::from_secs(5)));
            }
            other => panic!("expected forward_auth, got {other:?}"),
        }
    }

    #[test]
    fn forward_auth_rejects_an_address_with_no_scheme() {
        let err = serde_yaml::from_str::<ServiceExtensions>(
            "proxy:\n  - host: site\n    network: wan\n    port: 80\n    middlewares:\n      - forward_auth:\n          address: auth.example.com/verify\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("http:// or https://"), "{err}");
    }

    #[test]
    fn bare_forward_auth_is_rejected() {
        let err = serde_yaml::from_str::<ServiceExtensions>(
            "proxy:\n  - host: site\n    network: wan\n    port: 80\n    middlewares: [forward_auth]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("needs settings"), "{err}");
    }

    #[test]
    fn authenticated_is_a_bare_middleware() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            "proxy:\n  - host: site\n    network: wan\n    port: 80\n    middlewares: [authenticated]\n",
        )
        .expect("parse");
        assert_eq!(ext.proxy[0].middlewares, vec![Middleware::Authenticated]);
    }

    #[test]
    fn redirect_regex_parses_with_capture_groups() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            r#"
proxy:
  - host: site
    network: wan
    port: 80
    middlewares:
      - redirect_regex:
          regex: "^/old/(.*)"
          replacement: "/new/$1"
          permanent: false
"#,
        )
        .expect("parse");
        match &ext.proxy[0].middlewares[0] {
            Middleware::RedirectRegex(r) => {
                assert_eq!(r.regex, "^/old/(.*)");
                assert_eq!(r.replacement, "/new/$1");
                assert!(!r.permanent);
            }
            other => panic!("expected redirect_regex, got {other:?}"),
        }
    }

    #[test]
    fn redirect_regex_defaults_to_permanent() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            "proxy:\n  - host: site\n    network: wan\n    port: 80\n    middlewares:\n      - redirect_regex:\n          regex: \"^/a$\"\n          replacement: \"/b\"\n",
        )
        .expect("parse");
        match &ext.proxy[0].middlewares[0] {
            Middleware::RedirectRegex(r) => assert!(r.permanent),
            other => panic!("expected redirect_regex, got {other:?}"),
        }
    }

    #[test]
    fn redirect_regex_rejects_an_invalid_pattern() {
        let err = serde_yaml::from_str::<ServiceExtensions>(
            "proxy:\n  - host: site\n    network: wan\n    port: 80\n    middlewares:\n      - redirect_regex:\n          regex: \"(unclosed\"\n          replacement: \"/b\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid regex"), "{err}");
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

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn parses_actions() {
        let ext: ServiceExtensions = serde_yaml::from_str(
            "actions:\n  Reset WAL:\n    cmd: pg_resetwal -f /var/lib/postgresql/data\n    user: postgres\n    timeout: 1m\n    confirm: true\n    show_in_ui: false\n  Ping:\n    cmd: [echo, hi]\n",
        )
        .unwrap();
        let wal = &ext.actions["Reset WAL"];
        assert_eq!(wal.argv()[..2], ["sh", "-c"]);
        assert_eq!(wal.user.as_deref(), Some("postgres"));
        assert_eq!(wal.timeout, Some(Duration::from_secs(60)));
        assert!(wal.confirm);
        assert!(!wal.show_in_ui);
        assert!(ext.actions["Ping"].show_in_ui);
        assert!(!ext.actions["Ping"].confirm);
        assert_eq!(ext.actions["Ping"].argv(), ["echo", "hi"]);
    }
}
