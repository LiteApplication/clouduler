//! Server side session storage for cookie based (OIDC) logins.
//!
//! Sessions live in memory only. Losing them on restart just means everyone
//! logs in again, which is a fine trade for not putting session state through
//! raft.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use super::Identity;

const SESSION_ID_LEN: usize = 32;
pub const COOKIE_NAME: &str = "cl_session";

struct Session {
    identity: Identity,
    expires_at: Instant,
}

#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session and return its id, to be set as a cookie value.
    pub async fn create(&self, identity: Identity, ttl: Duration) -> String {
        let id = random_id();
        let session = Session {
            identity,
            expires_at: Instant::now() + ttl,
        };
        self.sessions.lock().await.insert(id.clone(), session);
        id
    }

    /// Look up a session, dropping it if it has expired.
    pub async fn get(&self, id: &str) -> Option<Identity> {
        let mut sessions = self.sessions.lock().await;
        match sessions.get(id) {
            Some(session) if session.expires_at > Instant::now() => Some(session.identity.clone()),
            Some(_) => {
                sessions.remove(id);
                None
            }
            None => None,
        }
    }

    /// Drop every expired session. `get` only evicts a session when it is
    /// presented again, so one never seen again would otherwise stay forever.
    pub async fn prune_expired(&self) {
        let now = Instant::now();
        self.sessions
            .lock()
            .await
            .retain(|_, session| session.expires_at > now);
    }

    pub async fn remove(&self, id: &str) {
        self.sessions.lock().await.remove(id);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    fn identity() -> Identity {
        Identity {
            subject: "user@example.com".into(),
            username: "user@example.com".into(),
            role: Role::Viewer,
        }
    }

    #[tokio::test]
    async fn a_created_session_can_be_looked_up() {
        let store = SessionStore::new();
        let id = store.create(identity(), Duration::from_secs(60)).await;
        let found = store.get(&id).await.expect("session present");
        assert_eq!(found.subject, "user@example.com");
    }

    #[tokio::test]
    async fn an_expired_session_is_gone() {
        let store = SessionStore::new();
        let id = store.create(identity(), Duration::from_millis(1)).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(store.get(&id).await.is_none());
    }

    #[tokio::test]
    async fn removing_a_session_logs_it_out() {
        let store = SessionStore::new();
        let id = store.create(identity(), Duration::from_secs(60)).await;
        store.remove(&id).await;
        assert!(store.get(&id).await.is_none());
    }

    #[tokio::test]
    async fn pruning_drops_only_expired_sessions() {
        let store = SessionStore::new();
        let old = store.create(identity(), Duration::from_millis(1)).await;
        let live = store.create(identity(), Duration::from_secs(60)).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        store.prune_expired().await;
        let sessions = store.sessions.lock().await;
        assert!(!sessions.contains_key(&old));
        assert!(sessions.contains_key(&live));
    }
}
