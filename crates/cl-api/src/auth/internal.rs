//! Auth for node-to-node internal routes.
//!
//! These are not reached by a browser, so they skip the normal session/api
//! key [`Identity`](super::Identity) path entirely and check only an
//! HMAC-SHA256 signature computed over the shared cluster key, following
//! `cl_secret::internal_auth`. A node without the cluster key already cannot
//! decrypt secrets or run containers that need them, so requiring it here is
//! a consistent trust boundary, not a new one.
//!
//! The signature binds method, path and query, a timestamp and a body hash,
//! and a given signature is accepted once. The legacy path-only header is
//! ignored: it never expires, so honouring it would leave every captured
//! request replayable forever.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use cl_secret::internal_auth::{self, SKEW_SECS};
use cl_update::internal_client::{BODY_SHA256_HEADER, SIGNATURE_HEADER, TIMESTAMP_HEADER};

use crate::error::ApiError;
use crate::server::AppState;

/// Signatures already accepted, so a request captured off the wire cannot be
/// played back inside the skew window. Entries only exist for signatures that
/// verified, so an unauthenticated caller cannot grow it, and each is dropped
/// once its timestamp could no longer pass the skew check anyway.
#[derive(Default)]
pub struct ReplayCache {
    seen: Mutex<HashMap<String, u64>>,
}

impl ReplayCache {
    /// Record `signature` (made at `ts`). False when it was already seen.
    fn first_use(&self, signature: &str, ts: u64, now: u64) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, expires| *expires >= now);
        seen.insert(signature.to_string(), ts + SKEW_SECS).is_none()
    }
}

/// Proof the caller holds the cluster key, for a route that takes no body.
pub struct InternalAuth;

/// As [`InternalAuth`], for a route that reads a body. Carries the body hash
/// the caller signed; the handler must check the bytes it received against it
/// with [`InternalBodyAuth::check`], since an extractor cannot see the body.
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

/// Verify the v2 signature and burn it, returning the body hash it commits to.
async fn authenticate(parts: &Parts, state: &AppState) -> Result<String, ApiError> {
    let signature = header(parts, SIGNATURE_HEADER)?;
    let body_sha256 = header(parts, BODY_SHA256_HEADER)?;
    let ts: u64 = header(parts, TIMESTAMP_HEADER)?
        .parse()
        .map_err(|_| ApiError::Unauthenticated("bad internal timestamp".into()))?;

    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Unauthenticated("this node holds no cluster key".into()))?;

    // Scoped to the exact method and route path (query string included), so a
    // signature captured for one internal endpoint cannot be replayed
    // against another. Must match what the caller signs: the query string is
    // part of it for routes like logs that carry `follow`/`tail`/`since`.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let now = internal_auth::unix_now();
    if !internal_auth::verify_v2(
        key,
        parts.method.as_str(),
        path_and_query,
        ts,
        body_sha256,
        signature,
        now,
    ) {
        return Err(ApiError::Unauthenticated("bad internal signature".into()));
    }
    if !state.internal_replay.first_use(signature, ts, now) {
        return Err(ApiError::Unauthenticated(
            "internal signature already used".into(),
        ));
    }
    Ok(body_sha256.to_string())
}

impl FromRequestParts<AppState> for InternalAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // No internal route but the body-taking ones reads a body, so the
        // signed hash must be of nothing: a signature minted for a request
        // with a body is not good for one of these.
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
