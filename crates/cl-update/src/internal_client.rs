//! Calling the internal HMAC-authenticated restart route on a peer node.

use cl_secret::ClusterKey;

/// Unix timestamp the signature was made at.
pub const TIMESTAMP_HEADER: &str = "X-Clouduler-Internal-Timestamp";
/// Hex sha256 of the request body the signature commits to.
pub const BODY_SHA256_HEADER: &str = "X-Clouduler-Internal-Body-Sha256";
/// Per-request random nonce the v3 signature commits to.
pub const NONCE_HEADER: &str = "X-Clouduler-Internal-Nonce";
/// v3 signature over method, path and query, timestamp, nonce and body hash.
/// Unique per request even within one second.
pub const SIGNATURE_V3_HEADER: &str = "X-Clouduler-Internal-Signature-V3";

/// Every header an internal request carries, freshly signed. Call again on
/// retry: the server rejects a signature it has already seen.
pub fn auth_headers(
    key: &ClusterKey,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    let ts = cl_secret::internal_auth::unix_now();
    let body_sha256 = cl_secret::internal_auth::body_sha256_hex(body);
    let nonce = cl_secret::internal_auth::new_nonce();
    let v3 =
        cl_secret::internal_auth::sign_v3(key, method, path_and_query, ts, &nonce, &body_sha256);
    vec![
        (TIMESTAMP_HEADER, ts.to_string()),
        (BODY_SHA256_HEADER, body_sha256),
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

/// Ask a peer to swap its binary and restart. Success only means the request
/// was accepted; the caller must watch for the node leaving and returning.
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
    fn auth_headers_carry_a_valid_v3_signature() {
        let key = ClusterKey::generate();
        let h = auth_headers(&key, "GET", "/a?b=1", b"");
        let ts: u64 = get(&h, TIMESTAMP_HEADER).parse().unwrap();
        let body = get(&h, BODY_SHA256_HEADER);
        let nonce = get(&h, NONCE_HEADER);
        let now = ts;
        assert!(cl_secret::internal_auth::verify_v3(
            &key,
            &cl_secret::internal_auth::V3Request {
                method: "GET",
                path_and_query: "/a?b=1",
                ts,
                nonce,
                body_sha256: body,
            },
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
