//! Server side session storage for cookie based (OIDC and api key) logins.
//!
//! Sessions live in raft (`ClusterState::sessions`), so a login survives a restart and a leader change. Only a SHA-256 of the
//! session id is stored: the id is the cookie, and replicated state is readable through `/api/state` and sits in every raft
//! log and snapshot. Not part of the git backup export.
//!
//! Lookups are local reads, so a session may be a replication delay away from visible on another node.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cl_raft::{ClusterNode, Request, SessionRecord};
use sha2::{Digest, Sha256};

use super::{Identity, Role};
use crate::error::ApiError;

const SESSION_ID_LEN: usize = 32;
pub const COOKIE_NAME: &str = "cl_session";

pub struct SessionStore {
    cluster: Arc<ClusterNode>,
}

impl SessionStore {
    pub fn new(cluster: Arc<ClusterNode>) -> Self {
        Self { cluster }
    }

    pub async fn create(&self, identity: Identity, ttl: Duration) -> Result<String, ApiError> {
        let id = random_id();
        let now = unix_now();
        let request = Request::PutSession {
            key: hash_id(&id),
            session: SessionRecord {
                subject: identity.subject,
                username: identity.username,
                admin: identity.role.is_admin(),
                expires_at: now.saturating_add(ttl.as_secs().min(i64::MAX as u64) as i64),
            },
            now,
        };
        let response = self
            .cluster
            .submit(request)
            .await
            .map_err(|e| ApiError::Unavailable(format!("could not store the session: {e}")))?;
        match response.error {
            Some(e) => Err(ApiError::Internal(format!(
                "could not store the session: {e}"
            ))),
            None => Ok(id),
        }
    }

    /// An expired one is simply not found; `prune_expired` removes it later.
    pub async fn get(&self, id: &str) -> Option<Identity> {
        let key = hash_id(id);
        let now = unix_now();
        self.cluster
            .read(|state| {
                let session = state.sessions.get(&key)?;
                (session.expires_at > now).then(|| Identity {
                    subject: session.subject.clone(),
                    username: session.username.clone(),
                    role: if session.admin {
                        Role::Admin
                    } else {
                        Role::Viewer
                    },
                })
            })
            .await
    }

    /// `get` ignores an expired session but cannot delete it. Writes nothing when there is nothing to drop.
    pub async fn prune_expired(&self) {
        let now = unix_now();
        let any = self
            .cluster
            .read(|state| state.sessions.values().any(|s| s.expires_at <= now))
            .await;
        if any {
            let _ = self.cluster.submit(Request::PruneSessions { now }).await;
        }
    }

    pub async fn remove(&self, id: &str) {
        let _ = self
            .cluster
            .submit(Request::RemoveSession { key: hash_id(id) })
            .await;
    }
}

fn hash_id(id: &str) -> String {
    Sha256::digest(id.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn random_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut out = String::with_capacity(SESSION_ID_LEN);
    while out.len() < SESSION_ID_LEN {
        let mut buf = [0u8; 32];
        rand::fill(&mut buf);
        for byte in buf {
            if out.len() == SESSION_ID_LEN {
                break;
            }
            if (byte as usize) < (256 / ALPHABET.len()) * ALPHABET.len() {
                out.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
            }
        }
    }
    out
}
