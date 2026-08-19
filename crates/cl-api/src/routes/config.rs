//! `/api/config`: the cluster config file, edited with an auto-rollback safety net.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_config::{ClusterConfig, StackFile};
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::config_edit::{connection_sensitive_change, PendingEdit, PendingStatus, DEFAULT_ROLLBACK_AFTER};
use crate::error::ApiError;
use crate::redirects::{RedirectStore, REDIRECTS_CONFIG};
use crate::server::AppState;
use crate::static_routes::{StaticRouteStore, STATIC_ROUTES_CONFIG};
use crate::wireguard::{WireguardIngressStore, WIREGUARD_INGRESS_CONFIG};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/validate", post(validate_config))
        .route("/api/config/confirm", post(confirm))
        .route("/api/config/rollback", post(rollback))
        .route("/api/config/pending", get(pending))
        .route("/api/config/networks/{name}/rename", post(rename_network))
}

async fn get_config(_identity: Identity, State(state): State<AppState>) -> Json<ClusterConfig> {
    Json(state.cluster_config.read().await.clone())
}

#[derive(Deserialize)]
struct PutConfigRequest {
    config: ClusterConfig,
    /// Humantime string, e.g. "60s". Defaults to 60 seconds.
    rollback_after: Option<String>,
}

async fn put_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PutConfigRequest>,
) -> Result<StatusCode, ApiError> {
    body.config.validate().map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let rollback_after = match body.rollback_after {
        Some(text) => humantime::parse_duration(&text)
            .map_err(|e| ApiError::BadRequest(format!("bad rollback_after: {e}")))?,
        None => DEFAULT_ROLLBACK_AFTER,
    };

    let previous = state.cluster_config.read().await.clone();
    apply_config(&state, &body.config).await?;

    // Only arm the rollback watchdog for edits that can cut off the
    // connection this request came over. Anything else takes effect right
    // away: a mistyped failover timer doesn't need a 60 second grace period.
    if connection_sensitive_change(&previous, &body.config) {
        let mut pending = state.pending_edit.lock().await;
        *pending = Some(PendingEdit::arm(previous, state.clock(), rollback_after));
        drop(pending);

        spawn_rollback_watchdog(state, rollback_after);
    }
    Ok(StatusCode::OK)
}

/// Waits out the rollback window, then restores the previous config unless it
/// was confirmed or rolled back manually in the meantime.
fn spawn_rollback_watchdog(state: AppState, after: std::time::Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let mut pending = state.pending_edit.lock().await;
        let Some(edit) = pending.as_mut() else { return };
        if !edit.expire(state.clock()) {
            return;
        }
        let restored = edit.previous().clone();
        drop(pending);

        match apply_config(&state, &restored).await {
            Ok(()) => {
                tracing::warn!("config edit was not confirmed in time, rolled back automatically")
            }
            Err(e) => tracing::error!(error = %e, "automatic config rollback failed"),
        }
    });
}

/// Put a config into effect: through raft first, so every node's daemon writes
/// its own copy, then locally so the node answering this request is current
/// straight away rather than after its next watcher tick.
///
/// Raft goes first on purpose. Without a quorum the edit must not land anywhere,
/// or this node comes back from a restart with a config the cluster never
/// agreed to.
pub(super) async fn apply_config(state: &AppState, config: &ClusterConfig) -> Result<(), ApiError> {
    let yaml = serde_yaml::to_string(config).map_err(ApiError::internal)?;

    let expect_generation = state
        .cluster
        .state()
        .await
        .cluster_config
        .map(|c| c.generation)
        .unwrap_or(0);

    let response = state
        .cluster
        .submit(cl_raft::Request::PutClusterConfig {
            yaml: yaml.clone(),
            expect_generation,
        })
        .await
        .map_err(|e| ApiError::Internal(format!("replicating the config: {e}")))?;
    if let Some(error) = response.error {
        return Err(ApiError::Conflict(error));
    }

    std::fs::write(&state.config_path, yaml).map_err(ApiError::internal)?;
    *state.cluster_config.write().await = config.clone();
    Ok(())
}

#[derive(Deserialize)]
struct RenameNetworkRequest {
    new_name: String,
}

/// Rename a network everywhere it is referenced: the cluster config (the
/// `networks` key, every node's address map, `web.network`), every deployed
/// stack that names it, and the three out-of-band stores that hold a plain
/// `network: String` (redirects, static routes, wireguard ingress).
///
/// Not one atomic transaction -- each piece is its own raft log entry -- but
/// each step is atomic on its own and a failure partway through is surfaced
/// immediately rather than silently leaving a mismatched reference.
async fn rename_network(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(old_name): Path<String>,
    Json(body): Json<RenameNetworkRequest>,
) -> Result<StatusCode, ApiError> {
    let new_name = body.new_name.trim().to_string();
    if new_name.is_empty() {
        return Err(ApiError::BadRequest("new_name must not be empty".into()));
    }
    if new_name == old_name {
        return Ok(StatusCode::OK);
    }

    let mut config = state.cluster_config.read().await.clone();
    if !config.networks.contains_key(&old_name) {
        return Err(ApiError::BadRequest(format!("no network named {old_name}")));
    }
    if config.networks.contains_key(&new_name) {
        return Err(ApiError::BadRequest(format!("a network named {new_name} already exists")));
    }

    let net = config.networks.remove(&old_name).expect("checked above");
    let is_wireguard = net.kind == cl_config::NetworkKind::Wireguard;
    config.networks.insert(new_name.clone(), net);
    let node_names: Vec<String> = config.nodes.keys().cloned().collect();
    for node in config.nodes.values_mut() {
        if let Some(addr) = node.network.remove(&old_name) {
            node.network.insert(new_name.clone(), addr);
        }
    }
    if config.web.network == old_name {
        config.web.network = new_name.clone();
    }
    config.validate().map_err(|e| ApiError::BadRequest(e.to_string()))?;
    apply_config(&state, &config).await?;

    // Each node's imported wireguard config is sealed under a secret name
    // scoped by network name, so it must move along with the rename or the
    // tunnel silently stops finding its config.
    if is_wireguard {
        rename_wireguard_secrets(&state, &old_name, &new_name, &node_names).await?;
    }

    let cluster_state = state.cluster.state().await;
    for (stack_name, stack) in &cluster_state.stacks {
        let Ok(mut file) = StackFile::parse(&stack.spec) else { continue };
        if !rename_network_in_stack(&mut file, &old_name, &new_name) {
            continue;
        }
        let spec = serde_yaml::to_string(&file).map_err(ApiError::internal)?;
        let response = state
            .cluster
            .submit(cl_raft::Request::DeployStack { name: stack_name.clone(), spec })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!("renaming network in stack {stack_name}: {msg}")));
        }
    }

    rename_in_redirects(&state, &old_name, &new_name).await?;
    rename_in_static_routes(&state, &old_name, &new_name).await?;
    rename_in_wireguard_ingress(&state, &old_name, &new_name).await?;

    Ok(StatusCode::OK)
}

/// Renames `old` to `new` everywhere a stack file can name a network: the
/// top-level `networks` block, each service's `networks:` list, and every
/// `x-clouduler.proxy`/`expose` entry's `network` field. Returns whether
/// anything changed, so the caller can skip stacks that never mention it.
fn rename_network_in_stack(file: &mut StackFile, old: &str, new: &str) -> bool {
    let mut changed = false;
    if let Some(net) = file.networks.remove(old) {
        file.networks.insert(new.to_string(), net);
        changed = true;
    }
    for svc in file.services.values_mut() {
        for n in svc.networks.iter_mut() {
            if n == old {
                *n = new.to_string();
                changed = true;
            }
        }
        let Some(ext) = svc.x_clouduler.as_mut() else { continue };
        for route in ext.proxy.iter_mut() {
            if route.network == old {
                route.network = new.to_string();
                changed = true;
            }
        }
        for expose in ext.expose.iter_mut() {
            if expose.network == old {
                expose.network = new.to_string();
                changed = true;
            }
        }
    }
    changed
}

/// Moves every node's sealed wireguard config secret from the old network's
/// reserved name to the new one, so the tunnel keeps finding it after the
/// rename. Submitted as-is (still sealed, no key needed here) -- a
/// put-then-remove per node, since there is no rename request.
async fn rename_wireguard_secrets(
    state: &AppState,
    old: &str,
    new: &str,
    nodes: &[String],
) -> Result<(), ApiError> {
    for node in nodes {
        let old_name = crate::wireguard::wireguard_secret_name(old, node);
        let cluster_state = state.cluster.state().await;
        let Some(entry) = cluster_state.secrets.get(&old_name).cloned() else { continue };
        drop(cluster_state);

        let new_name = crate::wireguard::wireguard_secret_name(new, node);
        let response = state
            .cluster
            .submit(cl_raft::Request::PutSecret {
                name: new_name,
                sealed: entry.sealed,
                key_fingerprint: entry.key_fingerprint,
                owner: entry.owner,
            })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!("moving wireguard config for {node}: {msg}")));
        }

        let response = state
            .cluster
            .submit(cl_raft::Request::RemoveSecret { name: old_name })
            .await
            .map_err(ApiError::not_leader)?;
        if let Some(msg) = response.error {
            return Err(ApiError::Internal(format!("removing old wireguard config for {node}: {msg}")));
        }
    }
    Ok(())
}

async fn rename_in_redirects(state: &AppState, old: &str, new: &str) -> Result<(), ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut store = RedirectStore::load(&cluster_state);
    let mut changed = false;
    for rule in store.rules.iter_mut() {
        if rule.network == old {
            rule.network = new.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let response = state
        .cluster
        .submit(cl_raft::Request::PutConfig {
            name: REDIRECTS_CONFIG.to_string(),
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

async fn rename_in_static_routes(state: &AppState, old: &str, new: &str) -> Result<(), ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut store = StaticRouteStore::load(&cluster_state);
    let mut changed = false;
    for rule in store.routes.iter_mut() {
        if rule.network == old {
            rule.network = new.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let response = state
        .cluster
        .submit(cl_raft::Request::PutConfig {
            name: STATIC_ROUTES_CONFIG.to_string(),
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

async fn rename_in_wireguard_ingress(state: &AppState, old: &str, new: &str) -> Result<(), ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut store = WireguardIngressStore::load(&cluster_state);
    let mut changed = false;
    for rule in store.rules.iter_mut() {
        if rule.network == old {
            rule.network = new.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let response = state
        .cluster
        .submit(cl_raft::Request::PutConfig {
            name: WIREGUARD_INGRESS_CONFIG.to_string(),
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

#[derive(Deserialize)]
struct ValidateRequest {
    config: ClusterConfig,
}

#[derive(Serialize)]
struct ValidationError {
    /// Dotted config path of the offending key, empty when the failure is not
    /// tied to one.
    path: String,
    message: String,
}

#[derive(Serialize)]
struct ValidationResponse {
    valid: bool,
    errors: Vec<ValidationError>,
}

/// Check a config without applying it.
///
/// Takes the same `{"config": ...}` envelope as `PUT /api/config` so the two
/// can be called with one body, and answers 200 with the outcome rather than a
/// 4xx: a config that fails validation is an expected answer to this question,
/// not a bad request. Validation short circuits on the first problem, so
/// `errors` holds at most one entry.
async fn validate_config(
    _identity: Identity,
    Json(body): Json<ValidateRequest>,
) -> Json<ValidationResponse> {
    match body.config.validate() {
        Ok(()) => Json(ValidationResponse {
            valid: true,
            errors: Vec::new(),
        }),
        Err(cl_config::ConfigError::Invalid { at, msg }) => Json(ValidationResponse {
            valid: false,
            errors: vec![ValidationError { path: at, message: msg }],
        }),
        Err(other) => Json(ValidationResponse {
            valid: false,
            errors: vec![ValidationError {
                path: String::new(),
                message: other.to_string(),
            }],
        }),
    }
}

async fn confirm(_admin: RequireAdmin, State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let mut pending = state.pending_edit.lock().await;
    let edit = pending
        .as_mut()
        .ok_or_else(|| ApiError::Conflict("no config edit is pending".into()))?;
    if !edit.confirm() {
        return Err(ApiError::Conflict("pending edit already settled".into()));
    }
    Ok(StatusCode::OK)
}

async fn rollback(_admin: RequireAdmin, State(state): State<AppState>) -> Result<StatusCode, ApiError> {
    let mut pending = state.pending_edit.lock().await;
    let edit = pending
        .as_mut()
        .ok_or_else(|| ApiError::Conflict("no config edit is pending".into()))?;
    if !edit.rollback_manually() {
        return Err(ApiError::Conflict("pending edit already settled".into()));
    }
    let restored = edit.previous().clone();
    drop(pending);

    apply_config(&state, &restored).await?;
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct PendingBody {
    armed: bool,
    seconds_left: Option<u64>,
}

async fn pending(_identity: Identity, State(state): State<AppState>) -> Json<PendingBody> {
    let pending = state.pending_edit.lock().await;
    let body = match pending.as_ref() {
        Some(edit) if edit.status() == PendingStatus::Armed => PendingBody {
            armed: true,
            seconds_left: Some(edit.remaining(state.clock()).as_secs()),
        },
        _ => PendingBody {
            armed: false,
            seconds_left: None,
        },
    };
    Json(body)
}
