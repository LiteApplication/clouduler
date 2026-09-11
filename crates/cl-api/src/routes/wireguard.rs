//! `/api/wireguard/*`: import a node's WireGuard config for a wireguard-kind
//! network, and `/api/wireguard-ingress`: CRUD over the ports let in through
//! it. See `crate::wireguard` for the reserved-name conventions and
//! `cl_net::wireguard` for interface lifecycle and iptables enforcement,
//! which the daemon applies from what these routes write.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use cl_config::{NetworkKind, Protocol};
use cl_net::wireguard::{parse_wg_quick, wireguard_iface_name};
use cl_raft::state::MAX_OBJECT_BYTES;
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;
use crate::wireguard::{
    wireguard_secret_name, WireguardIngressRule, WireguardIngressStore, WIREGUARD_INGRESS_CONFIG,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/wireguard/{network}/{node}",
            put(put_config).delete(remove_config),
        )
        .route("/api/wireguard-ingress", get(list).post(create))
        .route("/api/wireguard-ingress/{id}", put(update).delete(remove))
}

async fn wireguard_network(state: &AppState, network: &str) -> Result<(), ApiError> {
    let cfg = state.cluster_config.read().await;
    match cfg.networks.get(network) {
        Some(net) if net.kind == NetworkKind::Wireguard => Ok(()),
        Some(_) => Err(ApiError::BadRequest(format!(
            "{network} is not a wireguard network"
        ))),
        None => Err(ApiError::BadRequest(format!("no network named {network}"))),
    }
}

#[derive(Deserialize)]
struct PutWireguardConfigRequest {
    /// Base64 of the wg-quick `.conf` text, same shape as
    /// `PutSecretRequest::File` on the generic secrets route.
    content: String,
}

#[derive(Serialize)]
struct PutWireguardConfigResponse {
    /// The interface name the daemon will bring this tunnel up as. The
    /// caller writes this into `node.network.<network>` via `PUT
    /// /api/config` to complete the import, which is computed here rather than
    /// duplicated client side, so the two can never drift apart.
    interface: String,
}

/// Seals and stores a node's WireGuard config, same as the generic
/// `PUT /api/secrets/{name}` route but under a name reserved for this
/// feature and with the config validated as parseable first, so a malformed
/// paste fails at import time instead of silently never bringing up a
/// tunnel.
async fn put_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((network, node)): Path<(String, String)>,
    Json(body): Json<PutWireguardConfigRequest>,
) -> Result<Json<PutWireguardConfigResponse>, ApiError> {
    wireguard_network(&state, &network).await?;
    {
        let cfg = state.cluster_config.read().await;
        if !cfg.nodes.contains_key(&node) {
            return Err(ApiError::BadRequest(format!("no node named {node}")));
        }
    }

    let plaintext = cl_raft::base64_decode(&body.content).map_err(ApiError::BadRequest)?;
    if plaintext.len() > MAX_OBJECT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "wireguard config for {node} is over the {MAX_OBJECT_BYTES} byte limit"
        )));
    }
    let text = String::from_utf8(plaintext.clone())
        .map_err(|_| ApiError::BadRequest("wireguard config must be utf8".into()))?;
    parse_wg_quick(&text).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    let name = wireguard_secret_name(&network, &node);
    let key_guard = state.key.read().await;
    let key = key_guard
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let sealed = Sealed::seal(key, &name, &plaintext).map_err(ApiError::internal)?;
    let key_fingerprint = key.fingerprint();
    drop(key_guard);

    let response = state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name,
            sealed: sealed.blob,
            key_fingerprint,
            owner: None,
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::BadRequest(msg));
    }

    Ok(Json(PutWireguardConfigResponse {
        interface: wireguard_iface_name(&network),
    }))
}

async fn remove_config(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path((network, node)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::RemoveSecret {
            name: wireguard_secret_name(&network, &node),
        })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<WireguardIngressRule>> {
    let cluster_state = state.cluster.state().await;
    Json(WireguardIngressStore::load(&cluster_state).rules)
}

#[derive(Deserialize)]
struct WireguardIngressBody {
    network: String,
    port: u16,
    #[serde(default)]
    protocol: Protocol,
    target_port: u16,
}

async fn create(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<WireguardIngressBody>,
) -> Result<Json<WireguardIngressRule>, ApiError> {
    wireguard_network(&state, &body.network).await?;

    let cluster_state = state.cluster.state().await;
    let mut store = WireguardIngressStore::load(&cluster_state);
    let rule = WireguardIngressRule {
        id: crate::static_routes::new_id(),
        network: body.network,
        port: body.port,
        protocol: body.protocol,
        target_port: body.target_port,
    };
    store.rules.push(rule.clone());
    submit_store(&state, &store).await?;
    Ok(Json(rule))
}

async fn update(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<WireguardIngressBody>,
) -> Result<Json<WireguardIngressRule>, ApiError> {
    wireguard_network(&state, &body.network).await?;

    let cluster_state = state.cluster.state().await;
    let mut store = WireguardIngressStore::load(&cluster_state);
    let Some(existing) = store.rules.iter_mut().find(|r| r.id == id) else {
        return Err(ApiError::NotFound(format!(
            "no wireguard ingress rule {id}"
        )));
    };
    existing.network = body.network;
    existing.port = body.port;
    existing.protocol = body.protocol;
    existing.target_port = body.target_port;
    let updated = existing.clone();

    submit_store(&state, &store).await?;
    Ok(Json(updated))
}

async fn remove(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let cluster_state = state.cluster.state().await;
    let mut store = WireguardIngressStore::load(&cluster_state);
    let before = store.rules.len();
    store.rules.retain(|r| r.id != id);
    if store.rules.len() == before {
        return Err(ApiError::NotFound(format!(
            "no wireguard ingress rule {id}"
        )));
    }
    submit_store(&state, &store).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn submit_store(state: &AppState, store: &WireguardIngressStore) -> Result<(), ApiError> {
    let response = state
        .cluster
        .submit(ClusterRequest::PutConfig {
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
