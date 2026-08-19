//! Calling the internal, HMAC-authenticated self-update routes on a peer node.
//!
//! Used by the orchestrator to trigger a restart and by a worker to fetch the
//! artifact its arch's builder produced. Both go over the node's normal https
//! api listener, just a different route and a different auth header.

use cl_secret::ClusterKey;

/// Header carrying the signature. Named after the scheme, not the route, so
/// every internal route can share one header name.
pub const AUTH_HEADER: &str = "X-Clouduler-Internal-Auth";

fn artifact_path(arch: &str) -> String {
    format!("/api/self-update/internal/artifact/{arch}")
}

const RESTART_PATH: &str = "/api/self-update/internal/restart";

/// Fetch the binary a peer built for `arch`. `base_url` is that node's https
/// api origin, e.g. `https://10.10.0.2:8443`.
pub async fn fetch_artifact(
    client: &reqwest::Client,
    base_url: &str,
    arch: &str,
    key: &ClusterKey,
) -> Result<Vec<u8>, String> {
    let path = artifact_path(arch);
    let signature = cl_secret::internal_auth::sign(key, &path);
    let resp = client
        .get(format!("{base_url}{path}"))
        .header(AUTH_HEADER, signature)
        .send()
        .await
        .map_err(|e| format!("requesting artifact from {base_url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{base_url}{path} answered {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("reading artifact body: {e}"))
}

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
        return Err(format!("{base_url}{RESTART_PATH} answered {}", resp.status()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_artifact_path_is_scoped_to_its_arch() {
        assert_eq!(artifact_path("x86_64"), "/api/self-update/internal/artifact/x86_64");
        assert_ne!(artifact_path("x86_64"), artifact_path("aarch64"));
    }
}
