//! The allowlist: which docker engine requests the clouduler socket answers.
//! Anything not named here is refused, so a new engine endpoint is never
//! reachable by accident.

use hyper::Method;

/// A request the socket knows how to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Ping,
    Version,
    Info,
    ListImages,
    InspectImage(String),
    ImageHistory(String),
    SaveImage(String),
    /// `POST /images/create?fromImage=..&tag=..`, already joined into one reference.
    PullImage(String),
}

/// Why a request was not routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not an endpoint this socket serves at all.
    NotPermitted,
    /// An endpoint it serves, asked for with the wrong verb or missing input.
    BadRequest(&'static str),
}

/// Drop the engine's `/v1.NN` version prefix.
fn strip_version(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("/v") else {
        return path;
    };
    let Some((version, tail)) = rest.split_once('/') else {
        return path;
    };
    let numeric = !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.');
    if numeric {
        // Keep the slash that separated the version from the path.
        &path[path.len() - tail.len() - 1..]
    } else {
        path
    }
}

fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A reference docker would accept in a path: never empty, no traversal or
/// control characters, since it is handed on to a registry or another node.
fn valid_reference(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 512
        && !name.split('/').any(|seg| seg == ".." || seg.is_empty())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | ':' | '@' | '.' | '_' | '-'))
}

pub fn route(method: &Method, path: &str, query: Option<&str>) -> Result<Route, Refusal> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = strip_version(path);
    let get = method == Method::GET || method == Method::HEAD;

    let simple = match path {
        "/_ping" => Some(Route::Ping),
        "/version" => Some(Route::Version),
        "/info" => Some(Route::Info),
        "/images/json" => Some(Route::ListImages),
        _ => None,
    };
    if let Some(route) = simple {
        // `_ping` is also probed with HEAD; the rest are plain reads.
        return if get {
            Ok(route)
        } else {
            Err(Refusal::NotPermitted)
        };
    }

    // The Go client's ImageSave: `names` repeated once per image. Only one is
    // served; the scanners this exists for ask for exactly one.
    if path == "/images/get" {
        if !get {
            return Err(Refusal::NotPermitted);
        }
        let names: Vec<String> = query
            .unwrap_or("")
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .filter(|(k, _)| *k == "names")
            .map(|(_, v)| percent_decode(v))
            .collect();
        return match names.as_slice() {
            [one] if valid_reference(one) => Ok(Route::SaveImage(one.clone())),
            [_] => Err(Refusal::BadRequest("invalid image reference")),
            _ => Err(Refusal::BadRequest("exactly one image name is supported")),
        };
    }

    if path == "/images/create" {
        if method != Method::POST {
            return Err(Refusal::NotPermitted);
        }
        let from =
            query_param(query, "fromImage").ok_or(Refusal::BadRequest("fromImage is required"))?;
        // `fromImage=repo&tag=1.2` is the engine's way of writing `repo:1.2`.
        let reference = match query_param(query, "tag").filter(|t| !t.is_empty()) {
            Some(tag) if tag.starts_with("sha256:") => format!("{from}@{tag}"),
            Some(tag) => format!("{from}:{tag}"),
            None => from,
        };
        return if valid_reference(&reference) {
            Ok(Route::PullImage(reference))
        } else {
            Err(Refusal::BadRequest("invalid image reference"))
        };
    }

    let Some(rest) = path.strip_prefix("/images/") else {
        return Err(Refusal::NotPermitted);
    };
    let (name, make): (&str, fn(String) -> Route) = if let Some(n) = rest.strip_suffix("/json") {
        (n, Route::InspectImage)
    } else if let Some(n) = rest.strip_suffix("/history") {
        (n, Route::ImageHistory)
    } else if let Some(n) = rest.strip_suffix("/get") {
        (n, Route::SaveImage)
    } else {
        return Err(Refusal::NotPermitted);
    };
    if !get {
        return Err(Refusal::NotPermitted);
    }
    if !valid_reference(name) {
        return Err(Refusal::BadRequest("invalid image reference"));
    }
    Ok(make(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Result<Route, Refusal> {
        route(&Method::GET, path, None)
    }

    #[test]
    fn the_version_prefix_is_optional() {
        assert_eq!(get("/_ping"), Ok(Route::Ping));
        assert_eq!(get("/v1.45/_ping"), Ok(Route::Ping));
        assert_eq!(
            get("/v1.45/images/alpine:3.20/json"),
            Ok(Route::InspectImage("alpine:3.20".into()))
        );
    }

    #[test]
    fn registry_references_with_slashes_route() {
        assert_eq!(
            get("/images/ghcr.io/anchore/grype:latest/get"),
            Ok(Route::SaveImage("ghcr.io/anchore/grype:latest".into()))
        );
        assert_eq!(
            get("/images/nginx@sha256:abc123/history"),
            Ok(Route::ImageHistory("nginx@sha256:abc123".into()))
        );
    }

    #[test]
    fn everything_else_is_refused() {
        for (method, path) in [
            (Method::POST, "/containers/create"),
            (Method::GET, "/containers/json"),
            (Method::DELETE, "/images/alpine"),
            (Method::POST, "/images/alpine/json"),
            (Method::POST, "/images/load"),
            (Method::POST, "/build"),
            (Method::GET, "/volumes"),
            (Method::POST, "/v1.45/exec/abc/start"),
            (Method::POST, "/_ping"),
        ] {
            assert_eq!(
                route(&method, path, None),
                Err(Refusal::NotPermitted),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn pull_joins_the_tag_onto_the_repository() {
        let pull = |q| route(&Method::POST, "/v1.45/images/create", Some(q));
        assert_eq!(
            pull("fromImage=alpine&tag=3.20"),
            Ok(Route::PullImage("alpine:3.20".into()))
        );
        assert_eq!(
            pull("fromImage=alpine%3A3.19"),
            Ok(Route::PullImage("alpine:3.19".into()))
        );
        assert_eq!(
            pull("fromImage=alpine&tag=sha256:ab"),
            Ok(Route::PullImage("alpine@sha256:ab".into()))
        );
        assert!(matches!(
            route(&Method::POST, "/images/create", None),
            Err(Refusal::BadRequest(_))
        ));
        assert!(matches!(
            pull("fromImage=../etc/passwd"),
            Err(Refusal::BadRequest(_))
        ));
        // Importing from a tarball or url is `fromSrc`, which is never routed.
        assert!(matches!(
            pull("fromSrc=http://evil/x.tar"),
            Err(Refusal::BadRequest(_))
        ));
    }

    #[test]
    fn the_plural_save_form_names_one_image() {
        let save = |q| route(&Method::GET, "/v1.45/images/get", Some(q));
        assert_eq!(
            save("names=sha256%3Aabc123"),
            Ok(Route::SaveImage("sha256:abc123".into()))
        );
        assert!(matches!(
            save("names=a&names=b"),
            Err(Refusal::BadRequest(_))
        ));
        assert!(matches!(save(""), Err(Refusal::BadRequest(_))));
        assert_eq!(
            route(&Method::POST, "/images/get", Some("names=a")),
            Err(Refusal::NotPermitted)
        );
    }

    #[test]
    fn traversal_and_junk_references_are_rejected() {
        for path in ["/images/../etc/json", "/images//json", "/images/a b/json"] {
            assert!(matches!(get(path), Err(Refusal::BadRequest(_))), "{path}");
        }
    }
}
