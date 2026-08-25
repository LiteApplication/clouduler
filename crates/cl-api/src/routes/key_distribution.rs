//! Automatic delivery of the cluster secret key to a newly joined node.
//!
//! A joining node has no way to receive the plaintext key over the network --
//! that would defeat the point of keeping it out of the database. Instead
//! every node has its own X25519 keypair (`cl_secret::NodeKeypair`); a node
//! publishes its public half into its `NodeState` (`daemon::report_self`),
//! and any node that already holds the plaintext key wraps a copy for it
//! (`spawn_wrap_for_new_nodes`) and stores only that ciphertext in raft. The
//! recipient then unwraps its own copy locally with its private key
//! (`spawn_unwrap_own_key`) and never has to be told the key by a person.

use std::time::Duration;

use cl_raft::state::{Request as ClusterRequest, WrappedKeyEntry};
use cl_secret::{NodeKeypair, WrappedKey};
use tracing::{info, warn};

use crate::server::AppState;

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
