//! Forwarding a read to the node that actually holds the answer.
//!
//! Container inspection, logs and stats only exist on the node running the
//! container; resource usage only on the node itself. cl-raft carries no rpc
//! path for one node to ask another for any of these, so this goes over the
//! same channel `cl-update`'s internal self-update calls use: the node's
//! normal https api, authenticated by an HMAC signature over the shared
//! cluster key rather than session or api key auth, since this is node to
//! node and no browser is involved.

use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use cl_secret::internal_auth;
use futures::TryStreamExt;
use serde::de::DeserializeOwned;

use crate::error::ApiError;
use crate::server::AppState;

/// Generous but bounded: a peer that cannot answer a plain read in this long
/// is treated the same as one that is down, so a caller waiting on the
/// cluster-wide instance list is never stuck on one unreachable node.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Forwarded write commands can run a docker restart or an image pull on the
/// target node before answering, either of which can legitimately take a
/// while (a large image pull especially), so this is far more generous than
/// `FETCH_TIMEOUT`.
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);

/// A peer answering 503 means "not there right now, and may be later" (see
/// `ApiError::Unavailable`) — the exact shape of a node whose docker client
/// is mid-reconnect, or that is still fencing right after a raft rejoin.
/// Both of those clear up within a few seconds in the common case, so it is
/// worth a few retries before giving up and surfacing it to the caller.
const RETRY_ATTEMPTS: u32 = 5;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// Retry `attempt` (0-based) after a short exponential backoff, but only when
/// the peer's answer was specifically 503 — any other non-success status is a
/// real failure (auth, not found, ...) that retrying will not fix.
async fn retry_on_unavailable(attempt: u32, status: reqwest::StatusCode) -> bool {
    if attempt + 1 >= RETRY_ATTEMPTS || status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return false;
    }
    tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt)).await;
    true
}

/// Same backoff, for when the request never got a response at all (dropped
/// connection, handshake hiccup) rather than an explicit 503. On a mesh like
/// tailscale these are just as transient as a 503 and just as worth a
/// couple of quiet retries instead of failing a whole scan over one dropped
/// packet — see the "could not scan a node" symptom this replaced.
///
/// Deliberately excludes timeouts (`reqwest::Error::is_timeout`): those
/// already burned a full `FETCH_TIMEOUT`, so retrying stacks that wait up to
/// `RETRY_ATTEMPTS` times instead of amortizing it away, and `scan_node`'s
/// callers fan out to every node in parallel — one slow node then bounds the
/// whole request and can trip an upstream proxy's own timeout (a 504)
/// instead of the fast, contributes-nothing-and-moves-on failure the caller
/// expects.
async fn retry_on_send_error(attempt: u32, err: &reqwest::Error) -> bool {
    if err.is_timeout() || attempt + 1 >= RETRY_ATTEMPTS {
        return false;
    }
    tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt)).await;
    true
}

async fn peer_base_url(state: &AppState, node: &str) -> Result<String, ApiError> {
    let cfg = state.cluster_config.read().await;
    cl_update::peers::base_url(&cfg, node)
        .ok_or_else(|| ApiError::Unavailable(format!("no address on record for node {node}")))
}

async fn signature(state: &AppState, path: &str) -> Result<String, ApiError> {
    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("this node holds no cluster key".to_string()))?;
    Ok(internal_auth::sign(key, path))
}

/// `GET path` (query string included, if any) on `node`'s api, decoded as `T`.
pub async fn fetch_json<T: DeserializeOwned>(
    state: &AppState,
    node: &str,
    path: &str,
) -> Result<T, ApiError> {
    let base = peer_base_url(state, node).await?;
    let signature = signature(state, path).await?;
    for attempt in 0.. {
        let sent = state
            .http
            .get(format!("{base}{path}"))
            .header(cl_update::internal_client::AUTH_HEADER, signature.clone())
            .timeout(FETCH_TIMEOUT)
            .send()
            .await;
        let resp = match sent {
            Ok(resp) => resp,
            Err(e) => {
                if retry_on_send_error(attempt, &e).await {
                    continue;
                }
                return Err(ApiError::Unavailable(format!("asking {node}: {e}")));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            if retry_on_unavailable(attempt, status).await {
                continue;
            }
            return Err(ApiError::Unavailable(format!("{node} answered {status}")));
        }
        return resp
            .json::<T>()
            .await
            .map_err(|e| ApiError::Internal(format!("decoding {node}'s response: {e}")));
    }
    unreachable!("retry_on_unavailable eventually returns false")
}

/// `POST path` (no body) on `node`'s api, for write commands — restart,
/// update — whose target instance lives on another node. Any manager can
/// issue these; the receiving node trusts the caller's earlier
/// `RequireAdmin` check the same way it trusts it for the read-only
/// forwards above.
pub async fn post_empty(state: &AppState, node: &str, path: &str) -> Result<(), ApiError> {
    let base = peer_base_url(state, node).await?;
    let signature = signature(state, path).await?;
    for attempt in 0.. {
        let resp = state
            .http
            .post(format!("{base}{path}"))
            .header(cl_update::internal_client::AUTH_HEADER, signature.clone())
            .timeout(WRITE_TIMEOUT)
            .send()
            .await
            .map_err(|e| ApiError::Unavailable(format!("asking {node}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            if retry_on_unavailable(attempt, status).await {
                continue;
            }
            return Err(ApiError::Unavailable(format!("{node} answered {status}")));
        }
        return Ok(());
    }
    unreachable!("retry_on_unavailable eventually returns false")
}

/// `GET path` on `node`'s api, proxied straight through as the response body.
/// Used for the SSE log and stats streams: neither side needs to parse the
/// events to relay them, and a fixed timeout would cut a `follow=true` stream
/// short, so none is set here.
pub async fn proxy_stream(state: &AppState, node: &str, path: &str) -> Result<Response, ApiError> {
    let base = peer_base_url(state, node).await?;
    let signature = signature(state, path).await?;
    let resp = state
        .http
        .get(format!("{base}{path}"))
        .header(cl_update::internal_client::AUTH_HEADER, signature)
        .send()
        .await
        .map_err(|e| ApiError::Unavailable(format!("asking {node}: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::Unavailable(format!(
            "{node} answered {}",
            resp.status()
        )));
    }
    let stream = resp.bytes_stream().map_err(std::io::Error::other);
    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|e| ApiError::Internal(format!("building the proxied response: {e}")))
}
