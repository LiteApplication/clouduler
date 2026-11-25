//! HMAC signing for node-to-node internal HTTP calls (self-update artifact
//! transfer and remote restart).
//!
//! Those routes are not reached by a browser and no node holds a plaintext
//! admin api key long term, so they are not gated by the normal session/api
//! key `Identity` extractor. Instead the caller proves it holds the cluster
//! key: a node without it already cannot decrypt secrets or run containers
//! that need them, so requiring it here is a consistent trust boundary, not a
//! new one.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::key::ClusterKey;

/// HMAC accepts keys of any length, so construction cannot fail.
pub(crate) fn hmac_sha256(key: &ClusterKey) -> Hmac<Sha256> {
    <Hmac<Sha256> as KeyInit>::new_from_slice(key.bytes())
        .expect("any length key is valid for hmac-sha256")
}

/// Compute the signature for one internal call. `label` scopes the signature
/// to a specific route so a captured header for one internal endpoint cannot
/// be replayed against another.
pub fn sign(key: &ClusterKey, label: &str) -> String {
    let mut mac = hmac_sha256(key);
    mac.update(label.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time check of a signature presented by a caller.
pub fn verify(key: &ClusterKey, label: &str, presented: &str) -> bool {
    let mut mac = hmac_sha256(key);
    mac.update(label.as_bytes());
    let Ok(presented_bytes) = hex::decode(presented) else {
        return false;
    };
    mac.verify_slice(&presented_bytes).is_ok()
}

/// How far a timestamp may be from the verifier's clock, in either
/// direction. Also how long the server remembers a signature for replay
/// rejection, so the two windows line up exactly.
pub const SKEW_SECS: u64 = 60;

/// Hex sha256 of a request body, as bound into a signature. An empty body
/// hashes the empty string, so bodiless routes need no special case.
pub fn body_sha256_hex(body: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(body))
}

/// Seconds since the unix epoch, the clock timestamps are read against.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Length of a v3 nonce in hex characters (16 random bytes).
pub const NONCE_HEX_LEN: usize = 32;

/// A fresh random v3 nonce: 16 random bytes, hex encoded. Make one per
/// request and per retry.
pub fn new_nonce() -> String {
    let mut bytes = [0u8; NONCE_HEX_LEN / 2];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

/// Whether `nonce` is shaped like one from [`new_nonce`].
pub fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == NONCE_HEX_LEN && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

fn v3_label(method: &str, path_and_query: &str, ts: u64, nonce: &str, body_sha256: &str) -> String {
    format!("v3\n{method}\n{path_and_query}\n{ts}\n{nonce}\n{body_sha256}")
}

/// v3 signature: binds the method, path and query, a timestamp, a
/// per-request nonce and the body hash, so a captured header is only good for
/// one identical request inside the skew window, and two identical requests
/// made in the same second still sign differently (the server's replay cache
/// does not mistake the second for a replay of the first).
pub fn sign_v3(
    key: &ClusterKey,
    method: &str,
    path_and_query: &str,
    ts: u64,
    nonce: &str,
    body_sha256: &str,
) -> String {
    sign(
        key,
        &v3_label(method, path_and_query, ts, nonce, body_sha256),
    )
}

/// Verify a v3 signature, that `nonce` is well formed, and that `ts` is
/// within [`SKEW_SECS`] of `now`.
pub fn verify_v3(key: &ClusterKey, req: &V3Request<'_>, presented: &str, now: u64) -> bool {
    if req.ts.abs_diff(now) > SKEW_SECS || !valid_nonce(req.nonce) {
        return false;
    }
    verify(
        key,
        &v3_label(
            req.method,
            req.path_and_query,
            req.ts,
            req.nonce,
            req.body_sha256,
        ),
        presented,
    )
}

/// The signed parts of a v3 request, as the verifier sees them.
#[derive(Debug, Clone, Copy)]
pub struct V3Request<'a> {
    pub method: &'a str,
    pub path_and_query: &'a str,
    pub ts: u64,
    pub nonce: &'a str,
    pub body_sha256: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(nonce: &'a str, body_sha256: &'a str) -> V3Request<'a> {
        V3Request {
            method: "GET",
            path_and_query: "/a",
            ts: 1000,
            nonce,
            body_sha256,
        }
    }

    #[test]
    fn a_signature_verifies_against_the_same_key_and_label() {
        let key = ClusterKey::generate();
        let sig = sign(&key, "restart:n2");
        assert!(verify(&key, "restart:n2", &sig));
    }

    #[test]
    fn a_signature_does_not_verify_under_a_different_label() {
        let key = ClusterKey::generate();
        let sig = sign(&key, "restart:n2");
        assert!(!verify(&key, "artifact:x86_64", &sig));
    }

    #[test]
    fn a_signature_from_a_different_key_is_rejected() {
        let key = ClusterKey::generate();
        let other = ClusterKey::generate();
        let sig = sign(&key, "restart:n2");
        assert!(!verify(&other, "restart:n2", &sig));
    }

    #[test]
    fn garbage_input_is_rejected_not_panicked_on() {
        let key = ClusterKey::generate();
        assert!(!verify(&key, "restart:n2", "not hex!!"));
    }

    #[test]
    fn v3_roundtrips_and_binds_the_nonce() {
        let key = ClusterKey::generate();
        let h = body_sha256_hex(b"");
        let (n1, n2) = (new_nonce(), new_nonce());
        assert!(valid_nonce(&n1));
        assert_ne!(n1, n2);
        let sig = sign_v3(&key, "GET", "/a", 1000, &n1, &h);
        assert!(verify_v3(&key, &req(&n1, &h), &sig, 1000));
        assert!(!verify_v3(&key, &req(&n2, &h), &sig, 1000));
        assert!(!verify_v3(&key, &req(&n1, &h), &sig, 1061));
    }

    #[test]
    fn v3_rejects_any_changed_component() {
        let key = ClusterKey::generate();
        let h = body_sha256_hex(b"{}");
        let n = new_nonce();
        let sig = sign_v3(&key, "POST", "/a", 1000, &n, &h);
        let ok = V3Request {
            method: "POST",
            path_and_query: "/a",
            ts: 1000,
            nonce: &n,
            body_sha256: &h,
        };
        assert!(verify_v3(&key, &ok, &sig, 1000));
        assert!(!verify_v3(
            &key,
            &V3Request {
                method: "GET",
                ..ok
            },
            &sig,
            1000
        ));
        assert!(!verify_v3(
            &key,
            &V3Request {
                path_and_query: "/b",
                ..ok
            },
            &sig,
            1000
        ));
        let other = body_sha256_hex(b"{\"instances\":[]}");
        assert!(!verify_v3(
            &key,
            &V3Request {
                body_sha256: &other,
                ..ok
            },
            &sig,
            1000
        ));
        assert!(!verify_v3(&key, &V3Request { ts: 1001, ..ok }, &sig, 1001));
    }

    #[test]
    fn v3_rejects_a_malformed_nonce() {
        let key = ClusterKey::generate();
        let h = body_sha256_hex(b"");
        for bad in ["", "abc", &"g".repeat(32), &"a".repeat(33)] {
            let sig = sign_v3(&key, "GET", "/a", 1000, bad, &h);
            assert!(!verify_v3(&key, &req(bad, &h), &sig, 1000));
        }
    }
}
