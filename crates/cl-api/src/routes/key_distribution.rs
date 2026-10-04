//! Delivery of the cluster secret key to a newly joined node.
//!
//! A joining node never receives the plaintext key over the network. Every
//! node has its own X25519 keypair (`cl_secret::NodeKeypair`) and the key
//! travels wrapped for the recipient's public half, so only that node can open
//! it.
//!
//! There are two routes, because the node-to-node channel (raft included) is
//! itself authenticated with the cluster key, which a keyless node cannot
//! satisfy:
//!
//! - `POST /api/secrets/key/wrap` ([`wrap_for_node`]): the route a brand new
//!   node uses. `clouduler init` calls it over the admin-authenticated https
//!   api before the daemon ever starts, so the key is on disk when raft first
//!   needs it.
//! - The raft route (`spawn_wrap_for_new_nodes` / `spawn_unwrap_own_key`): a
//!   node publishes its public half into its `NodeState`, any key holder
//!   stores a wrapped copy in raft, and the recipient unwraps it locally. It
//!   only works for a node that can already speak raft, so it is a fallback
//!   for a node that lost or has not yet loaded its key file while its peers
//!   still trust it, not a way to bootstrap a new one.

use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use cl_raft::state::{Request as ClusterRequest, WrappedKeyEntry};
use cl_secret::{NodeKeypair, WrappedKey};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::RequireAdmin;
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/secrets/key/wrap", post(wrap_for_node))
}

#[derive(Deserialize)]
struct WrapRequest {
    /// Name of the node the key is for. Folded into the wrap, so the result
    /// only opens for a node claiming the same name.
    node: String,
    /// The recipient's `NodeKeypair` public half, base64 (the same encoding
    /// `NodeState::node_pubkey` carries).
    public_key: String,
}

#[derive(Serialize, Deserialize)]
pub struct WrapResponse {
    pub ephemeral_public: String,
    pub sealed: String,
    pub key_fingerprint: String,
}

/// Hand the cluster key to a joining node, wrapped for its public key.
///
/// Admin only, and served by whichever node the caller reached as long as it
/// holds the key (no leader needed: nothing is written to raft). The plaintext
/// key never appears in the response, only a copy only the holder of the
/// matching private key can open. Whoever holds an admin key can already
/// replace the whole config and deploy arbitrary containers, so this does not
/// widen what an admin can do.
async fn wrap_for_node(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<WrapRequest>,
) -> Result<Json<WrapResponse>, ApiError> {
    let node = body.node.trim();
    if node.is_empty() {
        return Err(ApiError::BadRequest("node must not be empty".into()));
    }
    let recipient = decode_pubkey(&body.public_key)
        .map_err(|e| ApiError::BadRequest(format!("public_key is malformed: {e}")))?;

    let guard = state.key.read().await;
    let Some(cluster_key) = guard.as_ref() else {
        return Err(ApiError::Conflict(
            "this node holds no cluster key, so it cannot hand one out. Ask a node that does"
                .into(),
        ));
    };
    let wrapped =
        WrappedKey::wrap_for(cluster_key, node, &recipient).map_err(ApiError::internal)?;
    info!(node = %node, "wrapped the cluster key for a joining node");
    Ok(Json(WrapResponse {
        ephemeral_public: wrapped.ephemeral_public,
        sealed: wrapped.sealed.blob,
        key_fingerprint: cluster_key.fingerprint(),
    }))
}

const WRAP_RETRY: Duration = Duration::from_secs(5);
const UNWRAP_RETRY: Duration = Duration::from_secs(5);

/// Wrap this node's held cluster key for every node that has published a
/// public key but has no current wrapped copy yet, including one whose
/// existing copy targets a stale public key, after a keypair rotation.
///
/// Any node currently holding the plaintext key runs this, not just the
/// leader: raft forwards a non-leader's `submit` to the current leader like
/// any other write, and wrapping the same key twice for the same node is
/// harmless (last write wins, both are valid), so there is no duplicate-mint
/// race to gate against the way `spawn_bootstrap_first_key` has to.
pub fn spawn_wrap_for_new_nodes(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(WRAP_RETRY);
        loop {
            ticker.tick().await;

            let cluster_key = state.key.read().await;
            let Some(cluster_key) = cluster_key.as_ref() else {
                continue;
            };

            let cluster_state = state.cluster.state().await;
            for (name, node) in &cluster_state.nodes {
                let Some(pubkey_b64) = &node.node_pubkey else {
                    continue;
                };
                let needs_wrap = match cluster_state.wrapped_keys.get(name) {
                    None => true,
                    Some(entry) => entry.target_pubkey != *pubkey_b64,
                };
                if !needs_wrap {
                    continue;
                }

                let Ok(recipient) = decode_pubkey(pubkey_b64) else {
                    warn!(node = %name, "this node's published public key is malformed, skipping");
                    continue;
                };
                let wrapped = match WrappedKey::wrap_for(cluster_key, name, &recipient) {
                    Ok(wrapped) => wrapped,
                    Err(e) => {
                        warn!(node = %name, error = %e, "could not wrap the cluster key for this node");
                        continue;
                    }
                };
                let entry = WrappedKeyEntry {
                    ephemeral_public: wrapped.ephemeral_public,
                    sealed: wrapped.sealed.blob,
                    key_fingerprint: cluster_key.fingerprint(),
                    target_pubkey: pubkey_b64.clone(),
                };
                match state
                    .cluster
                    .submit(ClusterRequest::PutWrappedKey {
                        node: name.clone(),
                        entry,
                    })
                    .await
                {
                    Ok(response) if response.error.is_none() => {
                        info!(node = %name, "wrapped the cluster key for a node");
                    }
                    Ok(response) => {
                        warn!(node = %name, error = ?response.error, "the cluster refused a wrapped key");
                    }
                    Err(e) => {
                        warn!(node = %name, error = %e, "could not submit a wrapped key");
                    }
                }
            }
        }
    });
}

/// Watch for this node's own wrapped key showing up in raft, unwrap it with
/// the local private key, and install it live (the same effect as an operator
/// pasting it through `POST /api/secrets/key`, just automatic.
///
/// Returns immediately, with nothing to watch for, on a node that already
/// holds a key (from disk at startup, or a manual install that lands while
/// this loop is waiting) or that has no keypair to unwrap with. No
/// timeout: a node with no key and no reachable key holder is the same
/// steady state the cluster has always supported, and this loop just keeps
/// it self-healing whenever that changes.
pub fn spawn_unwrap_own_key(state: AppState, node_name: String, keypair: Option<NodeKeypair>) {
    let Some(keypair) = keypair else {
        return;
    };
    tokio::spawn(async move {
        if state.key.read().await.is_some() {
            return;
        }
        let mut ticker = tokio::time::interval(UNWRAP_RETRY);
        loop {
            ticker.tick().await;

            if state.key.read().await.is_some() {
                return;
            }

            let cluster_state = state.cluster.state().await;
            let Some(entry) = cluster_state.wrapped_keys.get(&node_name) else {
                continue;
            };
            let wrapped = WrappedKey {
                ephemeral_public: entry.ephemeral_public.clone(),
                sealed: cl_secret::Sealed::from_blob(entry.sealed.clone()),
            };
            let cluster_key = match wrapped.unwrap_with(&keypair, &node_name) {
                Ok(key) => key,
                Err(e) => {
                    warn!(error = %e, "could not unwrap this node's delivered cluster key");
                    continue;
                }
            };
            if let Err(e) = cluster_key.save(&state.key_path) {
                warn!(error = %e, "could not write the delivered cluster key to disk");
                continue;
            }
            info!(fingerprint = %cluster_key.fingerprint(), "received the cluster key automatically");
            let cluster_key = std::sync::Arc::new(cluster_key);
            *state.key.write().await = Some(cluster_key.clone());
            let _ = state.key_tx.send(Some(cluster_key));
            return;
        }
    });
}

fn decode_pubkey(encoded: &str) -> Result<[u8; cl_secret::PUBLIC_KEY_BYTES], String> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| e.to_string())?;
    decoded.try_into().map_err(|_| "wrong length".to_string())
}
