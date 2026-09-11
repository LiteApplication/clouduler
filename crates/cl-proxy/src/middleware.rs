//! Named request/response transforms a route can opt into via
//! `x-clouduler.proxy[].middlewares`.
//!
//! Applied in this fixed order, regardless of the order they're listed in the
//! stack file:
//!
//! 1. `redirect_https` — handled before any of the others, at the plain HTTP
//!    listener. Driven by `route.tls`, not by this list; see `server.rs`.
//! 2. `strip_prefix` — removes the route's own `path_prefix` from the request
//!    path before it reaches the backend.
//! 3. `basic_auth` — challenges the request; short circuits with 401 on failure.
//! 4. `headers` — adds/removes headers on the proxied request.
//!
//! `basic_auth` credentials and custom `headers` are not yet modeled in
//! `cl-config` (the `Middleware` enum carries no per-route config), so both
//! take their configuration as explicit arguments here. Until a config
//! channel exists, `server.rs` wires them up as a no-op when nothing has been
//! supplied, and logs that the middleware was requested but has nothing to do.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use cl_config::CorsPolicy;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};

/// Hop-by-hop headers per RFC 7230 section 6.1, stripped in both directions.
/// `Upgrade` and `Connection` are kept when the request is a websocket
/// upgrade; the caller decides that before calling this.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Strips hop-by-hop headers, including any extra ones the `Connection`
/// header names.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let mut extra: Vec<String> = Vec::new();
    for value in headers.get_all("connection") {
        if let Ok(v) = value.to_str() {
            extra.extend(v.split(',').map(|s| s.trim().to_lowercase()));
        }
    }
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    for name in extra {
        headers.remove(name.as_str());
    }
}

/// Whether this request is a websocket (or other) upgrade, in which case
/// `Connection`/`Upgrade` must survive `strip_hop_by_hop`.
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let has_upgrade_token = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("upgrade"))
        });
    has_upgrade_token && headers.contains_key("upgrade")
}

/// A 308 redirect to the same host and path over https. 308 preserves the
/// method and body, unlike 301/302.
pub fn redirect_https(host: &str, path_and_query: &str) -> http::response::Builder {
    let location = format!("https://{host}{path_and_query}");
    http::Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header(http::header::LOCATION, location)
}

/// Strips `prefix` from the front of `path`. Leaves the path untouched if it
/// doesn't start with the prefix, and always leaves a leading `/`.
pub fn strip_prefix(path: &str, prefix: &str) -> String {
    match path.strip_prefix(prefix) {
        Some("") => "/".to_string(),
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        Some(rest) => format!("/{rest}"),
        None => path.to_string(),
    }
}

/// Username/password `basic_auth` checks against.
#[derive(Debug, Clone)]
pub struct BasicAuthCreds {
    pub username: String,
    pub password: String,
}

/// Checks the `Authorization` header against `creds`. `None` on the header
/// means "not authenticated", same as a mismatch.
pub fn check_basic_auth(headers: &HeaderMap, creds: &BasicAuthCreds) -> bool {
    let Some(value) = headers.get(http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(encoded) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = BASE64.decode(encoded) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    match decoded.split_once(':') {
        Some((user, pass)) => user == creds.username && pass == creds.password,
        None => false,
    }
}

/// 401 response challenging for basic auth.
pub fn basic_auth_challenge(realm: &str) -> http::response::Builder {
    http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(
            http::header::WWW_AUTHENTICATE,
            format!("Basic realm=\"{realm}\""),
        )
}

/// Static header additions/removals a route can apply to the proxied request.
#[derive(Debug, Clone, Default)]
pub struct HeaderOps {
    pub set: Vec<(HeaderName, HeaderValue)>,
    pub remove: Vec<HeaderName>,
}

pub fn apply_headers(headers: &mut HeaderMap, ops: &HeaderOps) {
    for name in &ops.remove {
        headers.remove(name);
    }
    for (name, value) in &ops.set {
        headers.insert(name.clone(), value.clone());
    }
}

/// Whether `policy` allows `origin` to read the response.
pub fn cors_origin_allowed(policy: &CorsPolicy, origin: &str) -> bool {
    policy.origins.iter().any(|o| o == "*" || o == origin)
}

/// Whether this request is a CORS preflight: an `OPTIONS` carrying the header
/// a browser only sends ahead of a cross-origin request it expects to need
/// approval for.
pub fn is_cors_preflight(method: &http::Method, headers: &HeaderMap) -> bool {
    method == http::Method::OPTIONS && headers.contains_key("access-control-request-method")
}

/// `Access-Control-*` response headers for a request from `origin`, or none
/// if the policy does not allow it. `requested_headers` is the preflight's
/// `Access-Control-Request-Headers`, echoed back when the policy names none
/// of its own.
pub fn cors_response_headers(
    policy: &CorsPolicy,
    origin: &str,
    requested_headers: Option<&str>,
) -> Vec<(HeaderName, HeaderValue)> {
    if !cors_origin_allowed(policy, origin) {
        return Vec::new();
    }
    // A wildcard policy still echoes the caller's origin rather than sending
    // `*` back, since that is what lets `Vary: Origin` responses cache
    // correctly per origin and is required at all once credentials are on
    // (rejected together with a wildcard origin at config validation time,
    // but the reflection is harmless either way).
    let mut out = vec![
        (
            HeaderName::from_static("access-control-allow-origin"),
            HeaderValue::from_str(origin).unwrap_or_else(|_| HeaderValue::from_static("*")),
        ),
        (
            HeaderName::from_static("vary"),
            HeaderValue::from_static("Origin"),
        ),
        (
            HeaderName::from_static("access-control-allow-methods"),
            HeaderValue::from_str(&policy.methods.join(", "))
                .unwrap_or_else(|_| HeaderValue::from_static("*")),
        ),
    ];
    let allow_headers = if policy.headers.is_empty() {
        requested_headers.map(str::to_string)
    } else {
        Some(policy.headers.join(", "))
    };
    if let Some(value) = allow_headers.and_then(|v| HeaderValue::from_str(&v).ok()) {
        out.push((
            HeaderName::from_static("access-control-allow-headers"),
            value,
        ));
    }
    if policy.allow_credentials {
        out.push((
            HeaderName::from_static("access-control-allow-credentials"),
            HeaderValue::from_static("true"),
        ));
    }
    if let Some(max_age) = policy.max_age {
        out.push((
            HeaderName::from_static("access-control-max-age"),
            HeaderValue::from_str(&max_age.as_secs().to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_hop_by_hop_and_connection_named_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "connection",
            HeaderValue::from_static("keep-alive, x-custom"),
        );
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert("x-custom", HeaderValue::from_static("drop-me"));
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));

        strip_hop_by_hop(&mut headers);

        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("x-custom"));
        assert!(headers.contains_key("x-forwarded-for"));
    }

    #[test]
    fn detects_websocket_upgrade() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("Upgrade"));
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        assert!(is_upgrade(&headers));

        let mut plain = HeaderMap::new();
        plain.insert("connection", HeaderValue::from_static("keep-alive"));
        assert!(!is_upgrade(&plain));
    }

    #[test]
    fn redirect_https_sets_location_and_status() {
        let resp = redirect_https("app.example.com", "/foo?x=1")
            .body(())
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            resp.headers().get(http::header::LOCATION).unwrap(),
            "https://app.example.com/foo?x=1"
        );
    }

    #[test]
    fn strip_prefix_removes_matched_prefix() {
        assert_eq!(strip_prefix("/api/v1/things", "/api"), "/v1/things");
        assert_eq!(strip_prefix("/api", "/api"), "/");
        assert_eq!(strip_prefix("/other", "/api"), "/other");
    }

    #[test]
    fn basic_auth_accepts_correct_credentials() {
        let creds = BasicAuthCreds {
            username: "admin".to_string(),
            password: "hunter2".to_string(),
        };
        let mut headers = HeaderMap::new();
        let token = BASE64.encode("admin:hunter2");
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {token}")).unwrap(),
        );
        assert!(check_basic_auth(&headers, &creds));
    }

    #[test]
    fn basic_auth_rejects_wrong_credentials_or_missing_header() {
        let creds = BasicAuthCreds {
            username: "admin".to_string(),
            password: "hunter2".to_string(),
        };
        assert!(!check_basic_auth(&HeaderMap::new(), &creds));

        let mut headers = HeaderMap::new();
        let token = BASE64.encode("admin:wrong");
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {token}")).unwrap(),
        );
        assert!(!check_basic_auth(&headers, &creds));
    }

    fn cors_policy(origins: &[&str]) -> CorsPolicy {
        CorsPolicy {
            origins: origins.iter().map(|s| s.to_string()).collect(),
            methods: vec!["GET".to_string(), "POST".to_string()],
            headers: Vec::new(),
            allow_credentials: false,
            max_age: None,
        }
    }

    #[test]
    fn cors_allows_a_listed_origin_and_rejects_others() {
        let policy = cors_policy(&["https://app.example.com"]);
        assert!(cors_origin_allowed(&policy, "https://app.example.com"));
        assert!(!cors_origin_allowed(&policy, "https://evil.example.com"));
    }

    #[test]
    fn cors_wildcard_allows_any_origin() {
        let policy = cors_policy(&["*"]);
        assert!(cors_origin_allowed(&policy, "https://anything.example.com"));
    }

    #[test]
    fn cors_headers_are_empty_for_a_disallowed_origin() {
        let policy = cors_policy(&["https://app.example.com"]);
        assert!(cors_response_headers(&policy, "https://evil.example.com", None).is_empty());
    }

    #[test]
    fn cors_headers_reflect_the_allowed_origin_and_methods() {
        let policy = cors_policy(&["https://app.example.com"]);
        let headers = cors_response_headers(&policy, "https://app.example.com", None);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, v)| v.to_str().unwrap())
        };
        assert_eq!(
            get("access-control-allow-origin"),
            Some("https://app.example.com")
        );
        assert_eq!(get("access-control-allow-methods"), Some("GET, POST"));
        assert_eq!(get("access-control-allow-credentials"), None);
    }

    #[test]
    fn cors_echoes_requested_headers_when_the_policy_names_none() {
        let policy = cors_policy(&["*"]);
        let headers = cors_response_headers(&policy, "https://app.example.com", Some("X-Custom"));
        let allow_headers = headers
            .iter()
            .find(|(n, _)| n.as_str() == "access-control-allow-headers")
            .map(|(_, v)| v.to_str().unwrap());
        assert_eq!(allow_headers, Some("X-Custom"));
    }

    #[test]
    fn cors_preflight_detected_by_method_and_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "access-control-request-method",
            HeaderValue::from_static("POST"),
        );
        assert!(is_cors_preflight(&http::Method::OPTIONS, &headers));
        assert!(!is_cors_preflight(&http::Method::GET, &headers));
        assert!(!is_cors_preflight(
            &http::Method::OPTIONS,
            &HeaderMap::new()
        ));
    }

    #[test]
    fn header_ops_set_and_remove() {
        let mut headers = HeaderMap::new();
        headers.insert("x-drop", HeaderValue::from_static("bye"));
        let ops = HeaderOps {
            set: vec![(
                HeaderName::from_static("x-added"),
                HeaderValue::from_static("yes"),
            )],
            remove: vec![HeaderName::from_static("x-drop")],
        };
        apply_headers(&mut headers, &ops);
        assert!(!headers.contains_key("x-drop"));
        assert_eq!(headers.get("x-added").unwrap(), "yes");
    }
}
