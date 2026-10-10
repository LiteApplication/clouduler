//! Authentication: api keys and OIDC, resolved to one [`Identity`].

pub mod internal;
pub mod keys;
pub mod middleware;
pub mod oauth;
pub mod oidc;
pub mod session;

use serde::Serialize;

/// Who is making the request and what they may do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    /// Api key id, or the OIDC subject claim. Stable, but not necessarily
    /// human-readable -- see `username` for what to display.
    pub subject: String,
    /// What to show in the ui: the api key id, or the configured OIDC
    /// username claim (falling back to `subject` when that claim is absent).
    pub username: String,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Viewer,
}

impl Role {
    pub fn is_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
}

pub use middleware::RequireAdmin;
