//! HMAC signing for node-to-node internal HTTP calls (self-update artifact
//! transfer and remote restart).
//!
//! Those routes are not reached by a browser and no node holds a plaintext
//! admin api key long term, so they are not gated by the normal session/api
//! key `Identity` extractor. Instead the caller proves it holds the cluster
//! key: a node without it already cannot decrypt secrets or run containers
//! that need them, so requiring it here is a consistent trust boundary, not a
//! new one.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::key::ClusterKey;

/// Compute the signature for one internal call. `label` scopes the signature
/// to a specific route so a captured header for one internal endpoint cannot
/// be replayed against another.
pub fn sign(key: &ClusterKey, label: &str) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.bytes())
        .expect("any length key is valid for hmac-sha256");
    mac.update(label.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time check of a signature presented by a caller.
pub fn verify(key: &ClusterKey, label: &str, presented: &str) -> bool {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.bytes())
        .expect("any length key is valid for hmac-sha256");
    mac.update(label.as_bytes());
    let Ok(presented_bytes) = hex::decode(presented) else {
        return false;
    };
    mac.verify_slice(&presented_bytes).is_ok()
}

/// How far a v2 timestamp may be from the verifier's clock, in either
/// direction. Also how long the server remembers a signature for replay
/// rejection, so the two windows line up exactly.
pub const SKEW_SECS: u64 = 60;

/// Hex sha256 of a request body, as bound into a v2 signature. An empty body
/// hashes the empty string, so bodiless routes need no special case.
pub fn body_sha256_hex(body: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(body))
}

/// Seconds since the unix epoch, the clock v2 timestamps are read against.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn v2_label(method: &str, path_and_query: &str, ts: u64, body_sha256: &str) -> String {
    format!("v2\n{method}\n{path_and_query}\n{ts}\n{body_sha256}")
}

/// v2 signature: unlike [`sign`] it binds the method, the body and a
/// timestamp, so a captured header is only good for one identical request
/// inside the skew window (and the server additionally rejects repeats).
pub fn sign_v2(
    key: &ClusterKey,
    method: &str,
    path_and_query: &str,
    ts: u64,
    body_sha256: &str,
) -> String {
    sign(key, &v2_label(method, path_and_query, ts, body_sha256))
}

/// Verify a v2 signature and that `ts` is within [`SKEW_SECS`] of `now`.
pub fn verify_v2(
    key: &ClusterKey,
    method: &str,
    path_and_query: &str,
    ts: u64,
    body_sha256: &str,
    presented: &str,
    now: u64,
) -> bool {
    if ts.abs_diff(now) > SKEW_SECS {
        return false;
    }
    verify(
        key,
        &v2_label(method, path_and_query, ts, body_sha256),
        presented,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn v2_roundtrips() {
        let key = ClusterKey::generate();
        let h = body_sha256_hex(b"");
        let sig = sign_v2(&key, "POST", "/a?b=1", 1000, &h);
        assert!(verify_v2(&key, "POST", "/a?b=1", 1000, &h, &sig, 1000));
        assert!(verify_v2(&key, "POST", "/a?b=1", 1000, &h, &sig, 1060));
    }

    #[test]
    fn v2_rejects_any_changed_component() {
        let key = ClusterKey::generate();
        let h = body_sha256_hex(b"{}");
        let sig = sign_v2(&key, "POST", "/a", 1000, &h);
        assert!(!verify_v2(&key, "GET", "/a", 1000, &h, &sig, 1000));
        assert!(!verify_v2(&key, "POST", "/b", 1000, &h, &sig, 1000));
        let other = body_sha256_hex(b"{\"instances\":[]}");
        assert!(!verify_v2(&key, "POST", "/a", 1000, &other, &sig, 1000));
        assert!(!verify_v2(&key, "POST", "/a", 1001, &h, &sig, 1001));
    }

    #[test]
    fn v2_rejects_expired_and_future_timestamps() {
        let key = ClusterKey::generate();
        let h = body_sha256_hex(b"");
        let sig = sign_v2(&key, "GET", "/a", 1000, &h);
        assert!(!verify_v2(&key, "GET", "/a", 1000, &h, &sig, 1061));
        assert!(!verify_v2(&key, "GET", "/a", 1000, &h, &sig, 939));
    }

    #[test]
    fn a_legacy_signature_is_not_a_v2_signature() {
        let key = ClusterKey::generate();
        let h = body_sha256_hex(b"");
        let legacy = sign(&key, "/a");
        assert!(!verify_v2(&key, "GET", "/a", 1000, &h, &legacy, 1000));
    }
}
