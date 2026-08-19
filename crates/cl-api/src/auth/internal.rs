//! Auth for node-to-node internal self-update routes.
//!
//! These are not reached by a browser, so they skip the normal session/api
//! key [`Identity`](super::Identity) path entirely and check only an
//! HMAC-SHA256 signature computed over the shared cluster key, following
//! `cl_secret::internal_auth`. A node without the cluster key already cannot
//! decrypt secrets or run containers that need them, so requiring it here is
//! a consistent trust boundary, not a new one.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::error::ApiError;
use crate::server::AppState;

pub struct InternalAuth;

impl FromRequestParts<AppState> for InternalAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let presented = parts
            .headers
            .get(cl_update::internal_client::AUTH_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::Unauthenticated("missing internal auth header".into()))?;

        let key_guard = state.key.read().await;
        let key = key_guard
            .as_ref()
            .ok_or_else(|| ApiError::Unauthenticated("this node holds no cluster key".into()))?;

        // Scoped to the exact route path, so a signature captured for one
        // internal endpoint cannot be replayed against another.
        let label = parts.uri.path();
        if !cl_secret::internal_auth::verify(key, label, presented) {
            return Err(ApiError::Unauthenticated("bad internal signature".into()));
        }
        Ok(InternalAuth)
    }
}
