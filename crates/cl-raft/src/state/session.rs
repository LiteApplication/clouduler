//! Login session and proxy auth handlers.

use super::*;

impl ClusterState {
    pub(super) fn sweep_proxy_auth(&mut self, now: i64) {
        self.proxy_sessions.retain(|_, s| s.expires_at > now);
        self.proxy_tokens.retain(|_, t| t.expires_at > now);
    }

    pub(super) fn apply_session(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::PutSession { key, session, now } => {
                self.sessions.retain(|_, s| s.expires_at > *now);
                self.sessions.insert(key.clone(), session.clone());
                Ok(())
            }

            Request::RemoveSession { key } => {
                self.sessions.remove(key);
                Ok(())
            }

            Request::PruneSessions { now } => {
                self.sessions.retain(|_, s| s.expires_at > *now);
                self.sweep_proxy_auth(*now);
                Ok(())
            }

            Request::PutProxySession { key, session, now } => {
                self.sweep_proxy_auth(*now);
                self.proxy_sessions.insert(key.clone(), session.clone());
                Ok(())
            }

            Request::RemoveProxySession { key } => {
                self.proxy_sessions.remove(key);
                self.proxy_sessions.retain(|_, s| &s.parent != key);
                self.proxy_tokens.retain(|_, t| &t.parent != key);
                Ok(())
            }

            Request::PutProxyToken { key, token, now } => {
                self.sweep_proxy_auth(*now);
                self.proxy_tokens.insert(key.clone(), token.clone());
                Ok(())
            }

            Request::RedeemProxyToken {
                token_key,
                host,
                session_key,
                now,
            } => {
                let Some(token) = self.proxy_tokens.get(token_key) else {
                    return Err("unknown or already used token".into());
                };
                if token.expires_at <= *now {
                    return Err("token expired".into());
                }
                if &token.host != host {
                    return Err("token was issued for another host".into());
                }
                if token.session_expires_at <= *now {
                    return Err("login expired".into());
                }
                if !token.parent.is_empty()
                    && self
                        .proxy_sessions
                        .get(&token.parent)
                        .is_none_or(|p| p.expires_at <= *now)
                {
                    return Err("signed out".into());
                }
                let Some(token) = self.proxy_tokens.remove(token_key) else {
                    return Err("unknown or already used token".into());
                };
                self.sweep_proxy_auth(*now);
                self.proxy_sessions.insert(
                    session_key.clone(),
                    ProxySessionRecord {
                        host: token.host,
                        subject: token.subject,
                        username: token.username,
                        groups: token.groups,
                        expires_at: token.session_expires_at,
                        parent: token.parent,
                    },
                );
                Ok(())
            }

            _ => unreachable!("apply_session was handed a request routed elsewhere"),
        }
    }
}
