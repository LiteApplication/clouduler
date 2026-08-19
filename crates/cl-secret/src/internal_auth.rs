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
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.bytes()).expect("any length key is valid for hmac-sha256");
    mac.update(label.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time check of a signature presented by a caller.
pub fn verify(key: &ClusterKey, label: &str, presented: &str) -> bool {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.bytes()).expect("any length key is valid for hmac-sha256");
    mac.update(label.as_bytes());
    let Ok(presented_bytes) = hex::decode(presented) else {
        return false;
    };
    mac.verify_slice(&presented_bytes).is_ok()
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
}
