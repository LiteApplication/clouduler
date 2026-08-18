//! `/api/keys`: api key management, plus the first-launch bootstrap key.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use cl_raft::Request as ClusterRequest;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::auth::keys::{self, ApiKeyStore, API_KEYS_CONFIG};
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/keys", get(list).post(create))
        .route("/api/keys/{id}", delete(remove))
}

#[derive(Serialize)]
struct KeySummary {
    id: String,
    name: Option<String>,
    created_at: i64,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<Vec<KeySummary>> {
    let cluster_state = state.cluster.state().await;
    let store = ApiKeyStore::load(&cluster_state);
    Json(
        store
            .keys
            .into_iter()
            .map(|k| KeySummary {
                id: k.id,
                name: k.name,
                created_at: k.created_at,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct CreateKeyRequest {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Serialize)]
struct CreateKeyResponse {
    id: String,
    /// The full secret, shown exactly once.
    key: String,
}

async fn create(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<CreateKeyRequest>,
) -> Result<Json<CreateKeyResponse>, ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut store = ApiKeyStore::load(&cluster_state);
    let new_key = keys::generate(body.name, now());
    let id = new_key.record.id.clone();
    store.keys.push(new_key.record);

    submit_store(&state, &store).await?;
    Ok(Json(CreateKeyResponse {
        id,
        key: new_key.secret,
    }))
}

async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut store = ApiKeyStore::load(&cluster_state);
    let before = store.keys.len();
    store.keys.retain(|k| k.id != id);
    if store.keys.len() == before {
        return Err(ApiError::NotFound(format!("no key {id}")));
    }
    submit_store(&state, &store).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn submit_store(state: &AppState, store: &ApiKeyStore) -> Result<(), ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: API_KEYS_CONFIG.to_string(),
            content: store.encode(),
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Internal(msg));
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Mint a key on first launch, when the cluster holds none yet.
///
/// This runs as a task rather than once at startup because of the ordering on a
/// brand new cluster: the api comes up before any leader has been elected, and
/// a write with no leader to forward to just fails. Doing it once meant the
/// only way into a fresh cluster was never created at all.
///
/// Only the leader mints, and it rechecks the store on every attempt, so
/// several managers starting together produce one key rather than one each.
pub fn spawn_bootstrap_first_key(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(BOOTSTRAP_RETRY);
        loop {
            ticker.tick().await;

            let cluster_state = state.cluster.state().await;
            let store = ApiKeyStore::load(&cluster_state);
            if !store.keys.is_empty() {
                return;
            }
            if !state.cluster.is_leader() {
                continue;
            }

            let new_key = keys::generate(Some("bootstrap".to_string()), now());
            let mut store = store;
            store.keys.push(new_key.record);

            match submit_store(&state, &store).await {
                Ok(()) => {
                    // The one time this is ever shown, so it has to be
                    // impossible to miss in a wall of startup logs.
                    info!(
                        "\n\n=====================================================\n\
                         no api keys exist yet: generated one for initial access\n\
                         \n    {}\n\n\
                         save it now, it will not be shown again\n\
                         =====================================================\n",
                        new_key.secret
                    );
                    return;
                }
                // Leadership can be lost between the check and the write.
                // Another manager will pick it up, or we will on the next tick.
                Err(e) => tracing::debug!(error = %e, "bootstrap api key not stored yet, retrying"),
            }
        }
    });
}

const BOOTSTRAP_RETRY: std::time::Duration = std::time::Duration::from_secs(2);
