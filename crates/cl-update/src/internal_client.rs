//! Calling the internal, HMAC-authenticated self-update restart route on a
//! peer node.
//!
//! Used by the orchestrator to trigger a restart once a node has staged a
//! freshly downloaded binary. Goes over the node's normal https api
//! listener, just a different route and a different auth header.

use cl_secret::ClusterKey;

/// Header carrying the signature. Named after the scheme, not the route, so
/// every internal route can share one header name.
pub const AUTH_HEADER: &str = "X-Clouduler-Internal-Auth";

const RESTART_PATH: &str = "/api/self-update/internal/restart";

/// Ask a peer to swap its binary and restart. Fire-and-forget in effect: a
/// success response only means the node accepted the request before it
/// starts shutting down, not that it has restarted yet, so the caller has to
/// separately watch for the node going away and coming back.
pub async fn trigger_restart(
    client: &reqwest::Client,
    base_url: &str,
    key: &ClusterKey,
) -> Result<(), String> {
    let signature = cl_secret::internal_auth::sign(key, RESTART_PATH);
    let resp = client
        .post(format!("{base_url}{RESTART_PATH}"))
        .header(AUTH_HEADER, signature)
        .send()
        .await
        .map_err(|e| format!("requesting restart on {base_url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "{base_url}{RESTART_PATH} answered {}",
            resp.status()
        ));
    }
    Ok(())
}
