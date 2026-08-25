//! `forward_auth` middleware: ask an external HTTP endpoint whether a request
//! is allowed before it reaches the backend, and let the endpoint hand back
//! identity headers to attach to the request that does.

use bytes::Bytes;
use cl_config::ForwardAuth;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};

/// Headers carried from the original request to the auth endpoint. Anything
/// else is deliberately left out: the auth endpoint's own view of "who is
/// asking" should not be widened by whatever the client happened to send.
const FORWARDED_REQUEST_HEADERS: &[&str] = &["cookie", "authorization"];

/// The result of asking the auth endpoint.
pub enum Outcome {
    /// The request may proceed. Carries the headers named in
    /// `auth_response_headers` that the auth endpoint's response actually
    /// set, to be merged onto the request forwarded to the backend.
    Allow(Vec<(HeaderName, HeaderValue)>),
    /// The request is refused. Carries what to send back to the client
    /// verbatim: the auth endpoint's own status, headers and body, or a
    /// synthesized 502 if the endpoint could not be reached at all.
    Deny(StatusCode, HeaderMap, Bytes),
}

/// Calls `cfg.address`, forwarding a minimal set of identifying headers plus
/// `X-Forwarded-Method`/`X-Forwarded-Uri` (Traefik's convention, so the auth
/// endpoint can make its decision based on what was actually requested).
pub async fn check(client: &reqwest::Client, cfg: &ForwardAuth, method: &Method, uri: &Uri, headers: &HeaderMap) -> Outcome {
    let mut request = client.get(cfg.address.as_str());
    for name in FORWARDED_REQUEST_HEADERS {
        if let Some(value) = headers.get(*name) {
            request = request.header(*name, value.clone());
        }
    }
    request = request
        .header("x-forwarded-method", method.as_str())
        .header("x-forwarded-uri", uri.to_string());
    if let Some(timeout) = cfg.timeout {
        request = request.timeout(timeout);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            return Outcome::Deny(
                StatusCode::BAD_GATEWAY,
                HeaderMap::new(),
                Bytes::from(format!("forward_auth endpoint unreachable: {e}")),
            )
        }
    };

    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let body = response.bytes().await.unwrap_or_default();
        return Outcome::Deny(status, headers, body);
    }

    let allowed = cfg
        .auth_response_headers
        .iter()
        .filter_map(|name| {
            let header_name: HeaderName = name.parse().ok()?;
            let value = response.headers().get(&header_name)?.clone();
            Some((header_name, value))
        })
        .collect();
    Outcome::Allow(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::net::SocketAddr;

    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper::Request;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    async fn spawn_auth_endpoint<F, Fut>(handler: F) -> SocketAddr
    where
        F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
        Fut: std::future::Future<Output = hyper::Response<Full<Bytes>>> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        let handler = handler.clone();
                        async move { Ok::<_, Infallible>(handler(req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, svc).await;
                });
            }
        });
        addr
    }

    fn cfg(address: String) -> ForwardAuth {
        ForwardAuth { address, auth_response_headers: vec!["X-User".to_string()], timeout: None }
    }

    #[tokio::test]
    async fn a_2xx_response_allows_and_carries_named_headers() {
        let addr = spawn_auth_endpoint(|_req| async {
            hyper::Response::builder()
                .status(200)
                .header("x-user", "alice")
                .header("x-not-copied", "nope")
                .body(Full::new(Bytes::new()))
                .unwrap()
        })
        .await;

        let client = reqwest::Client::new();
        let outcome = check(
            &client,
            &cfg(format!("http://{addr}/verify")),
            &Method::GET,
            &"/protected".parse().unwrap(),
            &HeaderMap::new(),
        )
        .await;

        match outcome {
            Outcome::Allow(headers) => {
                assert_eq!(headers.len(), 1);
                assert_eq!(headers[0].0, "x-user");
                assert_eq!(headers[0].1, "alice");
            }
            Outcome::Deny(..) => panic!("expected allow"),
        }
    }

    #[tokio::test]
    async fn a_401_response_is_denied_and_relayed() {
        let addr = spawn_auth_endpoint(|_req| async {
            hyper::Response::builder()
                .status(401)
                .body(Full::new(Bytes::from("nope")))
                .unwrap()
        })
        .await;

        let client = reqwest::Client::new();
        let outcome = check(
            &client,
            &cfg(format!("http://{addr}/verify")),
            &Method::GET,
            &"/protected".parse().unwrap(),
            &HeaderMap::new(),
        )
        .await;

        match outcome {
            Outcome::Deny(status, _headers, body) => {
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(&body[..], b"nope");
            }
            Outcome::Allow(..) => panic!("expected deny"),
        }
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_denied_with_a_bad_gateway() {
        let client = reqwest::Client::new();
        let outcome = check(
            &client,
            &cfg("http://127.0.0.1:1".to_string()),
            &Method::GET,
            &"/protected".parse().unwrap(),
            &HeaderMap::new(),
        )
        .await;

        match outcome {
            Outcome::Deny(status, _, _) => assert_eq!(status, StatusCode::BAD_GATEWAY),
            Outcome::Allow(..) => panic!("expected deny"),
        }
    }
}
