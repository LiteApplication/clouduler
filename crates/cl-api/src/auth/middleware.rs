use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::cookie::CookieJar;

use crate::error::ApiError;
use crate::server::AppState;

use super::keys::{self, ApiKeyStore};
use super::session::COOKIE_NAME;
use super::{Identity, Role};

impl FromRequestParts<AppState> for Identity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(header) = parts.headers.get(axum::http::header::AUTHORIZATION) {
            let value = header
                .to_str()
                .map_err(|_| ApiError::Unauthenticated("malformed authorization header".into()))?;
            if let Some((id, secret)) = keys::parse_authorization_header(value) {
                let cluster_state = state.cluster.snapshot().await;
                let store = ApiKeyStore::load(&cluster_state);
                return keys::verify(&store, id, secret)
                    .map(|record| Identity {
                        subject: record.id.clone(),
                        username: record.name.clone().unwrap_or_else(|| record.id.clone()),
                        role: Role::Admin,
                    })
                    .ok_or_else(|| ApiError::Unauthenticated("invalid api key".into()));
            }
            return Err(ApiError::Unauthenticated(
                "malformed authorization header".into(),
            ));
        }

        let jar = CookieJar::from_headers(&parts.headers);
        if let Some(cookie) = jar.get(COOKIE_NAME) {
            if let Some(identity) = state.sessions.get(cookie.value()).await {
                return Ok(identity);
            }
        }

        Err(ApiError::Unauthenticated("no credentials presented".into()))
    }
}

/// Rejects a viewer with 403; use on every admin-only route so a viewer never reaches code that might leak a secret value.
pub struct RequireAdmin(pub Identity);

impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let identity = Identity::from_request_parts(parts, state).await?;
        if identity.role.is_admin() {
            Ok(RequireAdmin(identity))
        } else {
            Err(ApiError::Forbidden("admin role required".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_role_passes_the_require_admin_check() {
        assert!(Role::Admin.is_admin());
        assert!(!Role::Viewer.is_admin());
    }
}
