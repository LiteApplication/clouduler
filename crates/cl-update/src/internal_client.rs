//! Calling the internal, HMAC-authenticated self-update restart route on a
//! peer node.
//!
//! Used by the orchestrator to trigger a restart once a node has staged a
//! freshly downloaded binary. Goes over the node's normal https api
//! listener, just a different route and a different auth header.

use cl_secret::ClusterKey;

/// Legacy signature header: an HMAC over the path alone, so a captured copy is
/// a permanent bearer token. Servers no longer read it; clients still send it
/// only so a not-yet-upgraded node in a mixed-version cluster keeps accepting
/// them. Drop it (and `cl_secret::internal_auth::sign` here) once no node
/// older than the v2 scheme can be in the cluster.
pub const AUTH_HEADER: &str = "X-Clouduler-Internal-Auth";
/// Unix timestamp the v2 signature was made at.
pub const TIMESTAMP_HEADER: &str = "X-Clouduler-Internal-Timestamp";
/// Hex sha256 of the request body the v2 signature commits to.
pub const BODY_SHA256_HEADER: &str = "X-Clouduler-Internal-Body-Sha256";
/// v2 signature over method, path and query, timestamp and body hash.
pub const SIGNATURE_HEADER: &str = "X-Clouduler-Internal-Signature-V2";

/// Every header an internal request carries, freshly signed. Each call makes a
/// new timestamp, so a retry must call this again rather than reuse headers:
/// the server rejects a signature it has already seen.
pub fn auth_headers(
    key: &ClusterKey,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    let ts = cl_secret::internal_auth::unix_now();
    let body_sha256 = cl_secret::internal_auth::body_sha256_hex(body);
    let v2 = cl_secret::internal_auth::sign_v2(key, method, path_and_query, ts, &body_sha256);
    vec![
        (
            AUTH_HEADER,
            cl_secret::internal_auth::sign(key, path_and_query),
        ),
        (TIMESTAMP_HEADER, ts.to_string()),
        (BODY_SHA256_HEADER, body_sha256),
        (SIGNATURE_HEADER, v2),
    ]
}

/// Sign `request` as an internal call. `method` and `body` must be exactly
/// what `request` sends.
pub fn authenticate(
    request: reqwest::RequestBuilder,
    key: &ClusterKey,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> reqwest::RequestBuilder {
    auth_headers(key, method, path_and_query, body)
        .into_iter()
        .fold(request, |request, (name, value)| {
            request.header(name, value)
        })
}

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
    let resp = authenticate(
        client.post(format!("{base_url}{RESTART_PATH}")),
        key,
        "POST",
        RESTART_PATH,
        b"",
    )
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
