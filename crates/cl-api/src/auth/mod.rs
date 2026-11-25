pub mod internal;
pub mod keys;
pub mod middleware;
pub mod oauth;
pub mod oidc;
pub mod session;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Identity {
    /// Api key id, or the OIDC subject claim; stable but not human-readable (see `username`).
    pub subject: String,
    /// The api key id, or the OIDC username claim (falling back to `subject`).
    pub username: String,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, rename = "UserRole"))]
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
