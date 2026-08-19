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
pub async fn fetch_json<T: DeserializeOwned>(state: &AppState, node: &str, path: &str) -> Result<T, ApiError> {
    let base = peer_base_url(state, node).await?;
    let signature = signature(state, path).await?;
    let resp = state
        .http
        .get(format!("{base}{path}"))
        .header(cl_update::internal_client::AUTH_HEADER, signature)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| ApiError::Unavailable(format!("asking {node}: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::Unavailable(format!("{node} answered {}", resp.status())));
    }
    resp.json::<T>()
        .await
        .map_err(|e| ApiError::Internal(format!("decoding {node}'s response: {e}")))
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
        return Err(ApiError::Unavailable(format!("{node} answered {}", resp.status())));
    }
    let stream = resp.bytes_stream().map_err(std::io::Error::other);
    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|e| ApiError::Internal(format!("building the proxied response: {e}")))
}
