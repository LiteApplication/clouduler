//! `/auth/*`: identity, logout, and the OIDC redirect dance.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;

use crate::auth::session::COOKIE_NAME;
use crate::auth::Identity;
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/auth/oidc/login", get(login))
        .route("/auth/oidc/callback", get(callback))
}

async fn me(identity: Identity) -> impl IntoResponse {
    Json(identity)
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    if let Some(cookie) = jar.get(COOKIE_NAME) {
        state.sessions.remove(cookie.value()).await;
    }
    let cleared = Cookie::build((COOKIE_NAME, "")).path("/").build();
    (jar.remove(cleared), StatusCode::OK)
}

async fn login(State(state): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let oidc = state
        .oidc
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("oidc is not configured".into()))?;
    let url = oidc.start_login().await?;
    Ok(Redirect::to(&url))
}

#[derive(Deserialize)]
struct CallbackParams {
    code: String,
    state: String,
}

async fn callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
    jar: CookieJar,
) -> Result<impl IntoResponse, ApiError> {
    let oidc = state
        .oidc
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("oidc is not configured".into()))?;
    let identity = oidc.finish_login(params.code, params.state).await?;
    let session_id = state.sessions.create(identity, state.session_ttl).await;

    let cookie = Cookie::build((COOKIE_NAME, session_id))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(::cookie::time::Duration::seconds(state.session_ttl.as_secs() as i64))
        .build();

    Ok((jar.add(cookie), Redirect::to("/")))
}
