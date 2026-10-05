//! `/auth/*`: identity, logout, and the OIDC redirect dance.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::Deserialize;

use crate::auth::keys::{self, ApiKeyStore};
use crate::auth::oidc::{check_state_cookie, LOGIN_TIMEOUT, STATE_COOKIE_NAME, STATE_COOKIE_PATH};
use crate::auth::session::COOKIE_NAME;
use crate::auth::{Identity, Role};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/me", get(me))
        .route("/auth/login", post(login_with_key))
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
    let cleared = Cookie::build((COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .build();
    (jar.remove(cleared), StatusCode::OK)
}

#[derive(Deserialize)]
struct LoginParams {
    key: String,
}

/// Trade an api key for a session cookie.
///
/// The ui needs this because `EventSource` cannot carry an Authorization
/// header, so every SSE endpoint is invisible to bearer auth. Exchanging the
/// key once for the same cookie OIDC issues gets the streams authenticated
/// like everything else, and keeps the key itself out of browser storage.
async fn login_with_key(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(params): Json<LoginParams>,
) -> Result<impl IntoResponse, ApiError> {
    let (id, secret) = keys::parse(params.key.trim())
        .ok_or_else(|| ApiError::Unauthenticated("that is not an api key".into()))?;
    let cluster_state = state.cluster.state().await;
    let store = ApiKeyStore::load(&cluster_state);
    let identity = keys::verify(&store, id, secret)
        .map(|record| Identity {
            subject: record.id.clone(),
            username: record.name.clone().unwrap_or_else(|| record.id.clone()),
            role: Role::Admin,
        })
        .ok_or_else(|| ApiError::Unauthenticated("invalid api key".into()))?;

    let session_ttl = state.cluster_config.read().await.auth.session_ttl;
    let session_id = state.sessions.create(identity.clone(), session_ttl).await;
    Ok((
        jar.add(session_cookie(session_id, session_ttl)),
        Json(identity),
    ))
}

/// The one place the session cookie's flags are decided, so the api key and
/// OIDC paths cannot drift apart. The server only ever speaks TLS, so `Secure`
/// is unconditional.
fn session_cookie(session_id: String, ttl: std::time::Duration) -> Cookie<'static> {
    Cookie::build((COOKIE_NAME, session_id))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(::cookie::time::Duration::seconds(ttl.as_secs() as i64))
        .build()
}

/// The cookie that binds a login's `state` to the browser that started it.
/// `Lax` is enough: the provider's redirect back is a top-level GET, which
/// carries it. The state is random and only meaningful server side, so the
/// cookie holds it as is.
fn state_cookie(state: String) -> Cookie<'static> {
    Cookie::build((STATE_COOKIE_NAME, state))
        .path(STATE_COOKIE_PATH)
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(::cookie::time::Duration::seconds(
            LOGIN_TIMEOUT.as_secs() as i64
        ))
        .build()
}

async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<impl IntoResponse, ApiError> {
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("oidc is not configured".into()))?;
    let (url, login_state) = oidc.start_login().await?;
    Ok((jar.add(state_cookie(login_state)), Redirect::to(&url)))
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
) -> Result<(CookieJar, Redirect), (CookieJar, ApiError)> {
    // One shot either way: a state cookie is never reused.
    let cookie_state = jar.get(STATE_COOKIE_NAME).map(|c| c.value().to_string());
    let jar = jar.remove(state_cookie(String::new()));
    match finish(&state, params, cookie_state).await {
        Ok(session) => Ok((jar.add(session), Redirect::to("/"))),
        Err(e) => Err((jar, e)),
    }
}

async fn finish(
    state: &AppState,
    params: CallbackParams,
    cookie_state: Option<String>,
) -> Result<Cookie<'static>, ApiError> {
    check_state_cookie(cookie_state.as_deref(), &params.state)?;
    let oidc_guard = state.oidc.read().await;
    let oidc = oidc_guard
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("oidc is not configured".into()))?;
    let identity = oidc.finish_login(params.code, params.state).await?;
    drop(oidc_guard);
    let session_ttl = state.cluster_config.read().await.auth.session_ttl;
    let session_id = state.sessions.create(identity, session_ttl).await;
    Ok(session_cookie(session_id, session_ttl))
}
