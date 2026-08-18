//! `/api/cloudflare/token`: setting the Cloudflare API token from the web ui.
//!
//! The token is write-only over the api. It is stored in a file next to the
//! cluster config, mode 0600, and only its presence is ever reported back —
//! there is no route that returns the value, so a stolen session cannot read
//! out a token someone else installed.
//!
//! Only the path lands in the replicated config; the file itself is local, the
//! same arrangement the cluster secret key uses. A second manager that also
//! answers ACME challenges needs its own copy of the file.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use cl_config::CloudflareConfig;
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

/// File name the token is written under, inside the config directory.
const TOKEN_FILE: &str = "cloudflare.token";

pub fn router() -> Router<AppState> {
    Router::new().route("/api/cloudflare/token", get(token_status).put(set_token))
}

#[derive(Serialize)]
struct TokenStatus {
    /// True once a token file is configured, which is what the ui keys the
    /// "you cannot see or change this here" state off.
    configured: bool,
    /// Where it lives, for an operator who needs to replace it by hand. Never
    /// the token itself.
    path: Option<String>,
    /// False when the config names a file that is not actually readable on this
    /// node, which is otherwise a silent ACME failure much later on.
    present_on_this_node: bool,
}

async fn token_status(_identity: Identity, State(state): State<AppState>) -> Json<TokenStatus> {
    let config = state.cluster_config.read().await;
    let path = config.cloudflare.as_ref().and_then(|cf| cf.api_token_file.clone());
    Json(TokenStatus {
        configured: path.is_some(),
        present_on_this_node: path.as_ref().is_some_and(|p| p.is_file()),
        path: path.map(|p| p.display().to_string()),
    })
}

#[derive(Deserialize)]
struct SetTokenRequest {
    token: String,
}

/// Write the token and point the config at it.
///
/// Refuses when a token file is already configured. Rotating one is a
/// deliberate act that should go through the file on disk, not a form that
/// might be submitted by a page left open on someone's second monitor.
async fn set_token(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<SetTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let token = body.token.trim().to_string();
    if token.is_empty() {
        return Err(ApiError::BadRequest("the token is empty".into()));
    }

    let mut config = state.cluster_config.read().await.clone();
    let cloudflare = config
        .cloudflare
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("cloudflare is not enabled in the config".into()))?;
    if cloudflare.api_token_file.is_some() {
        return Err(ApiError::Conflict(
            "a token file is already configured; replace it on disk to rotate the token".into(),
        ));
    }

    let path = token_path(&state.config_path);
    write_token(&path, &token)?;

    config.cloudflare = Some(CloudflareConfig {
        api_token_file: Some(path),
        zones: cloudflare.zones.clone(),
    });
    // Straight through, no rollback window: this cannot cut off the network the
    // caller is talking over, and an armed timer would silently undo it.
    super::config::apply_config(&state, &config).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn token_path(config_path: &std::path::Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(TOKEN_FILE)
}

/// Create the file with 0600 from the start rather than writing then chmod'ing:
/// cl_dns refuses a world readable token, and the gap between the two calls is
/// long enough for another process to open it.
fn write_token(path: &std::path::Path, token: &str) -> Result<(), ApiError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(ApiError::internal)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(ApiError::internal)?;
    writeln!(file, "{token}").map_err(ApiError::internal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_lands_next_to_the_config_file() {
        let path = token_path(std::path::Path::new("/etc/clouduler/cluster.yaml"));
        assert_eq!(path, PathBuf::from("/etc/clouduler/cloudflare.token"));
    }

    #[test]
    fn the_written_token_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cloudflare.token");

        write_token(&path, "cf-token-123").expect("write");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "cf-token-123");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "cl_dns rejects a world readable token");
    }
}
