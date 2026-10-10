//! Per-session focus: which stack an AI client is currently working on.
//!
//! Focus limits what the MCP's report and write tools touch, so it has to
//! outlive a single call, but it is not worth persisting: a client that loses
//! its session simply focuses again. It lives in memory on the node serving
//! `/mcp`, which is always the leader (followers relay to it), so a session
//! never straddles nodes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A session nobody has touched for this long is forgotten.
const IDLE_EXPIRY: Duration = Duration::from_secs(24 * 3600);

struct Entry {
    /// Who set the focus. An `Mcp-Session-Id` is a bearer-ish handle, so a
    /// focus is only honoured for the identity that created it: a different
    /// caller presenting the same id starts with no focus.
    owner: String,
    stack: Option<String>,
    last_used: Instant,
}

#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Entry>>,
}

impl Sessions {
    pub fn focus(&self, session: &str, owner: &str) -> Option<String> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut map);
        let entry = map.get_mut(session)?;
        if entry.owner != owner {
            return None;
        }
        entry.last_used = Instant::now();
        entry.stack.clone()
    }

    pub fn set_focus(&self, session: &str, owner: &str, stack: Option<String>) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut map);
        map.insert(
            session.to_string(),
            Entry {
                owner: owner.to_string(),
                stack,
                last_used: Instant::now(),
            },
        );
    }
}

fn prune(map: &mut HashMap<String, Entry>) {
    map.retain(|_, e| e.last_used.elapsed() < IDLE_EXPIRY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_is_per_session_and_per_owner() {
        let s = Sessions::default();
        assert_eq!(s.focus("a", "alice"), None);
        s.set_focus("a", "alice", Some("web".into()));
        assert_eq!(s.focus("a", "alice").as_deref(), Some("web"));
        assert_eq!(s.focus("b", "alice"), None);
        assert_eq!(s.focus("a", "mallory"), None);
        s.set_focus("a", "alice", None);
        assert_eq!(s.focus("a", "alice"), None);
    }
}
