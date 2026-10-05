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

/// Per-request random nonce the v3 signature commits to.
pub const NONCE_HEADER: &str = "X-Clouduler-Internal-Nonce";
/// v3 signature over method, path and query, timestamp, nonce and body hash.
/// Unlike v2 it is unique per request even within one second.
pub const SIGNATURE_V3_HEADER: &str = "X-Clouduler-Internal-Signature-V3";

/// Every header an internal request carries, freshly signed. Each call makes a
/// new timestamp and nonce, so a retry must call this again rather than reuse
/// headers: the server rejects a signature it has already seen.
///
/// The v2 signature is sent alongside v3 so a not-yet-upgraded server mid
/// rollout still accepts the request. Drop it once every node runs a
/// v3-capable build.
pub fn auth_headers(
    key: &ClusterKey,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    let ts = cl_secret::internal_auth::unix_now();
    let body_sha256 = cl_secret::internal_auth::body_sha256_hex(body);
    let nonce = cl_secret::internal_auth::new_nonce();
    let v2 = cl_secret::internal_auth::sign_v2(key, method, path_and_query, ts, &body_sha256);
    let v3 =
        cl_secret::internal_auth::sign_v3(key, method, path_and_query, ts, &nonce, &body_sha256);
    vec![
        (
            AUTH_HEADER,
            cl_secret::internal_auth::sign(key, path_and_query),
        ),
        (TIMESTAMP_HEADER, ts.to_string()),
        (BODY_SHA256_HEADER, body_sha256),
        (SIGNATURE_HEADER, v2),
        (NONCE_HEADER, nonce),
        (SIGNATURE_V3_HEADER, v3),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(headers: &'a [(&'static str, String)], name: &str) -> &'a str {
        headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("missing {name}"))
    }

    #[test]
    fn auth_headers_carry_both_v2_and_v3_signatures() {
        let key = ClusterKey::generate();
        let h = auth_headers(&key, "GET", "/a?b=1", b"");
        let ts: u64 = get(&h, TIMESTAMP_HEADER).parse().unwrap();
        let body = get(&h, BODY_SHA256_HEADER);
        let nonce = get(&h, NONCE_HEADER);
        let now = ts;
        assert!(cl_secret::internal_auth::verify_v2(
            &key,
            "GET",
            "/a?b=1",
            ts,
            body,
            get(&h, SIGNATURE_HEADER),
            now
        ));
        assert!(cl_secret::internal_auth::verify_v3(
            &key,
            "GET",
            "/a?b=1",
            ts,
            nonce,
            body,
            get(&h, SIGNATURE_V3_HEADER),
            now
        ));
    }

    #[test]
    fn every_call_uses_a_fresh_nonce() {
        let key = ClusterKey::generate();
        let a = auth_headers(&key, "GET", "/a", b"");
        let b = auth_headers(&key, "GET", "/a", b"");
        assert_ne!(get(&a, NONCE_HEADER), get(&b, NONCE_HEADER));
        assert_ne!(get(&a, SIGNATURE_V3_HEADER), get(&b, SIGNATURE_V3_HEADER));
    }
}
