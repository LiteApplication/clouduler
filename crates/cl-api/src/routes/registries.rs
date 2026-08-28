//! `/api/registries`: the container registry accounts the Updates view uses.
//!
//! A login is stored the same way the Cloudflare token is — sealed under a
//! reserved cluster secret name, replicated through raft, never readable back
//! over the api (`list` returns the username and nothing else). See
//! `cl_sched::registries` for the storage format and how a pull picks the
//! right one.
//!
//! Every login is checked against the registry before it is stored. A wrong
//! password that is accepted silently does not fail here; it fails later, on
//! some other node, in the middle of a pull, as an image that will not come
//! down — which is a much worse place to discover a typo.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_raft::Request as ClusterRequest;
use cl_sched::registries::{secret_name, Registries, StoredLogin};
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/registries", get(list).put(put_registry))
        .route("/api/registries/verify", post(verify_only))
        .route("/api/registries/{server}", axum::routing::delete(delete_registry))
}

/// Every registry login this node can currently open.
///
/// A node with no cluster key yet gets an empty set and falls back to
/// anonymous access, which is what it did before logins existed — better than
/// refusing to pull anything at all while the key is still being delivered.
pub(crate) async fn load(state: &AppState) -> Registries {
    let Some(key) = state.key.read().await.clone() else {
        return Registries::default();
    };
    Registries::load(&state.cluster.state().await, &key)
}

#[derive(Serialize)]
struct RegistryAccount {
    server: String,
    username: String,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Result<Json<Vec<RegistryAccount>>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;

    let registries = Registries::load(&cluster_state, &key);
    // `Registries` deliberately exposes no password accessor, so the listing
    // cannot leak one even by mistake.
    let mut accounts: Vec<RegistryAccount> = registries
        .accounts()
        .map(|(server, username)| RegistryAccount {
            server: server.to_string(),
            username: username.to_string(),
        })
        .collect();
    accounts.sort_by(|a, b| a.server.cmp(&b.server));
    Ok(Json(accounts))
}

#[derive(Deserialize)]
struct LoginRequest {
    server: String,
    username: String,
    password: String,
}

/// Check a login without storing it, for the ui's "test" button.
async fn verify_only(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<StatusCode, ApiError> {
    let (server, username, password) = validated(body)?;
    verify(&state.http, &server, &username, &password).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Check the login, then seal and store it, replacing any login for the same
/// server. Rotation is a plain overwrite: there is exactly one copy, and
/// every node reads it out of raft.
async fn put_registry(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<StatusCode, ApiError> {
    let (server, username, password) = validated(body)?;
    verify(&state.http, &server, &username, &password).await?;

    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;

    let login = StoredLogin { server, username, password };
    let (name, sealed) = login.seal(&key).map_err(ApiError::internal)?;

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name,
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_registry(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(server): Path<String>,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveSecret {
            name: secret_name(&normalize_server(&server)),
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validated(body: LoginRequest) -> Result<(String, String, String), ApiError> {
    let server = normalize_server(&body.server);
    if server.is_empty() {
        return Err(ApiError::BadRequest("the registry host is empty".into()));
    }
    let username = body.username.trim().to_string();
    if username.is_empty() {
        return Err(ApiError::BadRequest("the username is empty".into()));
    }
    // Not trimmed: leading or trailing whitespace can be part of a password,
    // and silently changing one produces a login that verifies here and fails
    // everywhere else.
    if body.password.is_empty() {
        return Err(ApiError::BadRequest("the password is empty".into()));
    }
    Ok((server, username, body.password))
}

/// Reduce whatever the operator typed to the host docker itself keys
/// credentials on.
///
/// Docker Hub has four spellings in common use and only one of them is what a
/// credential store holds, so they are all folded to `docker.io` — otherwise
/// a login saved as `hub.docker.com` would simply never be found when pulling
/// `nginx`, with nothing to indicate why.
fn normalize_server(server: &str) -> String {
    let trimmed = server
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    match trimmed.as_str() {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" | "hub.docker.com" => {
            cl_docker::DOCKER_HUB.to_string()
        }
        other => other.to_string(),
    }
}

/// Where a registry's v2 api lives. Docker Hub is the one registry whose
/// credential-store name is not the host its api answers on.
fn api_base(server: &str) -> String {
    if server == cl_docker::DOCKER_HUB {
        "https://registry-1.docker.io".to_string()
    } else {
        format!("https://{server}")
    }
}

/// Ask the registry whether it accepts this login.
///
/// Follows the ordinary registry v2 handshake: `GET /v2/` either succeeds
/// outright, or answers 401 naming the scheme to use. For `Bearer` — Docker
/// Hub, Forgejo, Gitea, GHCR, GitLab — that means fetching a token from the
/// realm it names, with the login as basic auth; a wrong password is a 401
/// there. For `Basic`, the same request is simply retried with the header.
async fn verify(
    http: &reqwest::Client,
    server: &str,
    username: &str,
    password: &str,
) -> Result<(), ApiError> {
    let base = api_base(server);
    let probe = http
        .get(format!("{base}/v2/"))
        .send()
        .await
        .map_err(|e| ApiError::BadRequest(format!("cannot reach {server}: {e}")))?;

    if probe.status().is_success() {
        // An open registry answers this anonymously, so there is nothing here
        // to check the password against. Storing it is still the right move:
        // the host is real and reachable, and a registry that is public for
        // reads commonly still wants a login for private repositories.
        return Ok(());
    }
    if probe.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Err(ApiError::BadRequest(format!(
            "{server} answered {} to a registry api request, which does not look like a container registry",
            probe.status()
        )));
    }

    let challenge = probe
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let response = if let Some(realm) = bearer_realm(&challenge) {
        let mut url = reqwest::Url::parse(&realm)
            .map_err(|e| ApiError::BadRequest(format!("{server} named an unusable token url: {e}")))?;
        if let Some(service) = challenge_param(&challenge, "service") {
            url.query_pairs_mut().append_pair("service", &service);
        }
        http.get(url)
            .basic_auth(username, Some(password))
            .send()
            .await
            .map_err(|e| ApiError::BadRequest(format!("cannot reach {server}'s token endpoint: {e}")))?
    } else {
        http.get(format!("{base}/v2/"))
            .basic_auth(username, Some(password))
            .send()
            .await
            .map_err(|e| ApiError::BadRequest(format!("cannot reach {server}: {e}")))?
    };

    match response.status() {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => Err(ApiError::BadRequest(
            format!("{server} rejected these credentials"),
        )),
        other => Err(ApiError::BadRequest(format!(
            "{server} answered {other} while checking these credentials"
        ))),
    }
}

/// The `realm` of a `Bearer` challenge, or `None` for any other scheme.
fn bearer_realm(challenge: &str) -> Option<String> {
    if !challenge.trim_start().to_ascii_lowercase().starts_with("bearer") {
        return None;
    }
    challenge_param(challenge, "realm")
}

/// Pull one quoted parameter out of a `WWW-Authenticate` challenge.
///
/// Deliberately simple rather than a full RFC 7235 parser: every registry in
/// practice emits `Bearer realm="...",service="..."` with quoted values, and
/// a challenge this cannot read falls back to basic auth rather than failing.
fn challenge_param(challenge: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = challenge.find(&needle)? + needle.len();
    let rest = &challenge[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spelling_of_docker_hub_folds_to_one() {
        for spelling in [
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "hub.docker.com",
            "https://hub.docker.com/",
            "  HUB.DOCKER.COM  ",
        ] {
            assert_eq!(normalize_server(spelling), "docker.io", "{spelling}");
        }
    }

    #[test]
    fn a_private_registry_keeps_its_host_and_port() {
        assert_eq!(normalize_server("https://git.example.com/"), "git.example.com");
        assert_eq!(normalize_server("registry.local:5000"), "registry.local:5000");
    }

    #[test]
    fn docker_hub_credentials_are_checked_against_the_registry_not_the_website() {
        assert_eq!(api_base("docker.io"), "https://registry-1.docker.io");
        assert_eq!(api_base("git.example.com"), "https://git.example.com");
    }

    #[test]
    fn a_bearer_challenge_yields_its_realm_and_service() {
        let challenge = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;
        assert_eq!(bearer_realm(challenge), Some("https://auth.docker.io/token".to_string()));
        assert_eq!(
            challenge_param(challenge, "service"),
            Some("registry.docker.io".to_string())
        );
    }

    #[test]
    fn a_basic_challenge_is_not_mistaken_for_a_bearer_one() {
        assert_eq!(bearer_realm(r#"Basic realm="registry""#), None);
    }
}
