//! Forwarding a read to the node that holds the answer.
//!
//! Container inspection, logs and stats exist only on the node running the container, and
//! cl-raft has no rpc for one node to ask another, so this uses the node's https api, authenticated
//! by an HMAC signature over the cluster key rather than session or api key auth.

use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use cl_update::internal_client;
use futures::TryStreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::ApiError;
use crate::server::AppState;

/// A peer that cannot answer a plain read in this long is treated as down, so the cluster-wide
/// instance list is never stuck on one node.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Far more generous than `FETCH_TIMEOUT`: a forwarded write can run a docker restart or a large image pull.
const WRITE_TIMEOUT: Duration = Duration::from_secs(300);

/// A 503 means "not there right now" (`ApiError::Unavailable`), e.g. docker mid-reconnect or fencing
/// after a raft rejoin; usually clears within seconds.
const RETRY_ATTEMPTS: u32 = 5;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// Retries only on 503; any other non-success status is a real failure.
async fn retry_on_unavailable(attempt: u32, status: reqwest::StatusCode) -> bool {
    if attempt + 1 >= RETRY_ATTEMPTS || status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return false;
    }
    tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt)).await;
    true
}

/// For requests that got no response at all (dropped connection), as transient as a 503 on a mesh
/// like tailscale.
///
/// Excludes timeouts: those already burned a full `FETCH_TIMEOUT`, and `scan_node`'s callers fan out
/// to every node in parallel, so retrying would let one slow node trip an upstream proxy 504.
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

/// Signed per attempt: the peer accepts each signature once, so a retry would be refused as a replay.
async fn signed(
    state: &AppState,
    request: reqwest::RequestBuilder,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<reqwest::RequestBuilder, ApiError> {
    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Unavailable("this node holds no cluster key".to_string()))?;
    Ok(internal_client::authenticate(
        request, key, method, path, body,
    ))
}

pub async fn fetch_json<T: DeserializeOwned>(
    state: &AppState,
    node: &str,
    path: &str,
) -> Result<T, ApiError> {
    fetch_json_within(state, node, path, FETCH_TIMEOUT).await
}

/// For reads that do real work on the peer (the image-update scan takes several seconds).
pub async fn fetch_json_within<T: DeserializeOwned>(
    state: &AppState,
    node: &str,
    path: &str,
    timeout: Duration,
) -> Result<T, ApiError> {
    let base = peer_base_url(state, node).await?;
    for attempt in 0.. {
        let sent = signed(
            state,
            state.http.get(format!("{base}{path}")),
            "GET",
            path,
            b"",
        )
        .await?
        .timeout(timeout)
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

/// For write commands (restart, update) on another node; the receiver trusts the caller's earlier `RequireAdmin` check.
pub async fn post_empty(state: &AppState, node: &str, path: &str) -> Result<(), ApiError> {
    post_response(state, node, path).await.map(|_| ())
}

pub async fn post_json<T: DeserializeOwned>(
    state: &AppState,
    node: &str,
    path: &str,
) -> Result<T, ApiError> {
    post_response(state, node, path)
        .await?
        .json::<T>()
        .await
        .map_err(|e| ApiError::Internal(format!("decoding {node}'s response: {e}")))
}

async fn post_response(
    state: &AppState,
    node: &str,
    path: &str,
) -> Result<reqwest::Response, ApiError> {
    let base = peer_base_url(state, node).await?;
    for attempt in 0.. {
        let resp = signed(
            state,
            state.http.post(format!("{base}{path}")),
            "POST",
            path,
            b"",
        )
        .await?
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
        return Ok(resp);
    }
    unreachable!("retry_on_unavailable eventually returns false")
}

/// Between a status read and a docker write: a large `tail` off a slow disk is not instant.
const TEXT_TIMEOUT: Duration = Duration::from_secs(30);

/// Raw body text, for the SSE log route with `follow=false`, which ends on its own.
pub async fn fetch_text(state: &AppState, node: &str, path: &str) -> Result<String, ApiError> {
    let base = peer_base_url(state, node).await?;
    for attempt in 0.. {
        let resp = signed(
            state,
            state.http.get(format!("{base}{path}")),
            "GET",
            path,
            b"",
        )
        .await?
        .timeout(TEXT_TIMEOUT)
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
        return resp
            .text()
            .await
            .map_err(|e| ApiError::Internal(format!("reading {node}'s response: {e}")));
    }
    unreachable!("retry_on_unavailable eventually returns false")
}

/// The signature commits to a hash of the body, so the receiving route must take `InternalBodyAuth`
/// and check the bytes it read. Uses the long write timeout (`exec-run` caps itself at 300s).
pub async fn post_json_body<B: Serialize, T: DeserializeOwned>(
    state: &AppState,
    node: &str,
    path: &str,
    body: &B,
) -> Result<T, ApiError> {
    let bytes = serde_json::to_vec(body)
        .map_err(|e| ApiError::Internal(format!("encoding the request for {node}: {e}")))?;
    let base = peer_base_url(state, node).await?;
    for attempt in 0.. {
        let request = state
            .http
            .post(format!("{base}{path}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(bytes.clone());
        let resp = signed(state, request, "POST", path, &bytes)
            .await?
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
        return resp
            .json::<T>()
            .await
            .map_err(|e| ApiError::Internal(format!("decoding {node}'s response: {e}")));
    }
    unreachable!("retry_on_unavailable eventually returns false")
}

/// Proxied straight through for the SSE log and stats streams; no timeout, which would cut a `follow=true` stream.
pub async fn proxy_stream(state: &AppState, node: &str, path: &str) -> Result<Response, ApiError> {
    let base = peer_base_url(state, node).await?;
    let resp = signed(
        state,
        state.http.get(format!("{base}{path}")),
        "GET",
        path,
        b"",
    )
    .await?
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
