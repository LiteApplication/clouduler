//! The one error type every handler returns. Maps cleanly onto the status
//! codes and json body documented in docs/api.md.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    /// Malformed request body or query, or a value that fails validation.
    BadRequest(String),
    /// No credentials, or credentials that do not check out.
    Unauthenticated(String),
    /// Authenticated, but the role does not allow this.
    Forbidden(String),
    NotFound(String),
    /// The request conflicts with the current state, e.g. no cluster key held.
    Conflict(String),
    /// Something this node needs to answer with is not there right now, and
    /// may be later. Docker being down is the case that matters: the rest of
    /// the api keeps working, container detail does not.
    Unavailable(String),
    /// This node is not the leader. Carries the leader's address so the
    /// caller can retry there directly.
    NotLeader { leader: Option<String> },
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::NotLeader { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> String {
        match self {
            ApiError::BadRequest(m) => m.clone(),
            ApiError::Unauthenticated(m) => m.clone(),
            ApiError::Forbidden(m) => m.clone(),
            ApiError::NotFound(m) => m.clone(),
            ApiError::Conflict(m) => m.clone(),
            ApiError::Unavailable(m) => m.clone(),
            ApiError::NotLeader { .. } => "not the leader".to_string(),
            ApiError::Internal(m) => m.clone(),
        }
    }

    pub fn internal(e: impl std::fmt::Display) -> Self {
        ApiError::Internal(e.to_string())
    }

    /// `ClusterNode::submit` failing means the request could not even be
    /// forwarded to a leader (usually because none is currently known); that
    /// is the same "come back later, maybe elsewhere" situation as this node
    /// not being the leader, so it gets the same status and header.
    pub fn not_leader(e: impl std::fmt::Display) -> Self {
        let _ = e;
        ApiError::NotLeader { leader: None }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let leader = match &self {
            ApiError::NotLeader { leader } => leader.clone(),
            _ => None,
        };
        let body = ErrorBody {
            error: self.message(),
            detail: None,
        };
        let mut resp = (status, Json(body)).into_response();
        if let Some(leader) = leader {
            if let Ok(value) = HeaderValue::from_str(&leader) {
                resp.headers_mut().insert(axum::http::header::LOCATION, value);
            }
        }
        resp
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn not_leader_carries_a_location_header() {
        let err = ApiError::NotLeader {
            leader: Some("https://n2.example.com".to_string()),
        };
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(axum::http::header::LOCATION).unwrap(),
            "https://n2.example.com"
        );
    }

    #[tokio::test]
    async fn body_matches_the_documented_shape() {
        let resp = ApiError::NotFound("no such node".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"], "no such node");
    }
}
