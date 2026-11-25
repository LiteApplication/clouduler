//! Auth for node-to-node internal routes.
//!
//! These skip the session/api key [`Identity`](super::Identity) path and check only an HMAC-SHA256
//! signature over the shared cluster key (`cl_secret::internal_auth`); a node without the key already
//! cannot decrypt secrets, so this is no new trust boundary.
//!
//! The signature binds method, path and query, a timestamp, a nonce and a body hash, and is accepted
//! once.

use parking_lot::Mutex;
use std::collections::HashMap;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use cl_secret::internal_auth::{self, SKEW_SECS};
use cl_update::internal_client::{
    BODY_SHA256_HEADER, NONCE_HEADER, SIGNATURE_V3_HEADER, TIMESTAMP_HEADER,
};

use crate::error::ApiError;
use crate::server::AppState;

/// Stops a captured request being replayed inside the skew window. Only verified signatures are
/// stored (an unauthenticated caller cannot grow it) and each is dropped once it could no longer pass the skew check.
#[derive(Default)]
pub struct ReplayCache {
    seen: Mutex<HashMap<String, u64>>,
}

impl ReplayCache {
    fn first_use(&self, signature: &str, ts: u64, now: u64) -> bool {
        let mut seen = self.seen.lock();
        seen.retain(|_, expires| *expires >= now);
        seen.insert(signature.to_string(), ts + SKEW_SECS).is_none()
    }
}

pub struct InternalAuth;

/// For a route that reads a body. The handler must check the bytes it received against the signed
/// hash with [`InternalBodyAuth::check`], since an extractor cannot see the body.
pub struct InternalBodyAuth(String);

impl InternalBodyAuth {
    pub fn check(&self, body: &[u8]) -> Result<(), ApiError> {
        if internal_auth::body_sha256_hex(body) == self.0 {
            Ok(())
        } else {
            Err(ApiError::Unauthenticated(
                "body does not match the signed hash".into(),
            ))
        }
    }
}

fn header<'a>(parts: &'a Parts, name: &str) -> Result<&'a str, ApiError> {
    parts
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::Unauthenticated(format!("missing {name} header")))
}

/// An already used signature is routine (a retry racing its first attempt), so it logs at debug.
fn reject(parts: &Parts, scheme: &str, reason: &str, routine: bool) -> ApiError {
    if routine {
        tracing::debug!(scheme, method = %parts.method, uri = %parts.uri, "internal auth: {reason}");
    } else {
        tracing::warn!(scheme, method = %parts.method, uri = %parts.uri, "internal auth: {reason}");
    }
    ApiError::Unauthenticated(reason.into())
}

/// Verifies the signature and burns it, returning the body hash it commits to.
async fn authenticate(parts: &Parts, state: &AppState) -> Result<String, ApiError> {
    let body_sha256 = header(parts, BODY_SHA256_HEADER)?;
    let ts: u64 = header(parts, TIMESTAMP_HEADER)?
        .parse()
        .map_err(|_| reject(parts, "-", "bad internal timestamp", false))?;
    let nonce = header(parts, NONCE_HEADER)?;
    let signature = header(parts, SIGNATURE_V3_HEADER)?;

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Unauthenticated("this node holds no cluster key".into()))?;

    // Scoped to method and route path including the query (logs carry `follow`/`tail`/`since`), so a
    // signature for one endpoint cannot be replayed against another. Must match what the caller signs.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let now = internal_auth::unix_now();
    let method = parts.method.as_str();
    if !internal_auth::verify_v3(
        key,
        &internal_auth::V3Request {
            method,
            path_and_query,
            ts,
            nonce,
            body_sha256,
        },
        signature,
        now,
    ) {
        return Err(reject(parts, "v3", "bad internal signature", false));
    }
    if !state
        .internal_replay
        .first_use(&format!("v3:{signature}"), ts, now)
    {
        return Err(reject(parts, "v3", "internal signature already used", true));
    }
    Ok(body_sha256.to_string())
}

impl FromRequestParts<AppState> for InternalAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Only body-taking routes read a body, so the signed hash must be of nothing.
        let claimed = header(parts, BODY_SHA256_HEADER)?;
        if claimed != internal_auth::body_sha256_hex(b"") {
            return Err(ApiError::Unauthenticated(
                "this internal route takes no body".into(),
            ));
        }
        authenticate(parts, state).await?;
        Ok(InternalAuth)
    }
}

impl FromRequestParts<AppState> for InternalBodyAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(InternalBodyAuth(authenticate(parts, state).await?))
    }
}
