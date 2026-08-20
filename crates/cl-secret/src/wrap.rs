//! Wrapping the cluster key to one node's public key, so it can be delivered
//! through the (untrusted-for-this-purpose) replicated database.
//!
//! A fresh ephemeral X25519 keypair is generated per wrap. The ephemeral
//! secret is Diffie-Hellman'd with the recipient's public key, and both
//! public keys plus the node's name are folded into the key derivation along
//! with the shared secret, so the actual sealing key is unique to this
//! (sender, recipient, node name) triple. The cluster key bytes are then
//! sealed with the existing `Sealed` primitive, under an associated-data
//! string prefixed to keep this separate from ordinary secret sealing (a
//! wrapped key and a stack secret could otherwise collide if a node and a
//! secret ever shared a name).

use base64::Engine;
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::Zeroize;

use crate::error::{Result, SecretError};
use crate::key::{ClusterKey, KEY_BYTES};
use crate::keypair::NodeKeypair;
use crate::seal::Sealed;

const KDF_CONTEXT: &[u8] = b"clouduler-key-wrap-v1";

/// The cluster key, encrypted to one node's public key. Safe to store in the
/// replicated database: only the node holding the matching private key can
/// recover the plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedKey {
    /// Base64 of the one-time ephemeral public key used for this wrap.
    pub ephemeral_public: String,
    pub sealed: Sealed,
}

impl WrappedKey {
    /// Encrypt `cluster_key` so only the holder of `recipient_public` (and
    /// only for this `node_name`) can recover it.
    pub fn wrap_for(
        cluster_key: &ClusterKey,
        node_name: &str,
        recipient_public: &[u8; 32],
    ) -> Result<Self> {
        let ephemeral = EphemeralSecret::random();
        let ephemeral_public = PublicKey::from(&ephemeral);
        let recipient = PublicKey::from(*recipient_public);
        let shared = ephemeral.diffie_hellman(&recipient);

        let symmetric = derive_symmetric_key(
            shared.as_bytes(),
            ephemeral_public.as_bytes(),
            recipient_public,
            node_name,
        );
        let sealed = Sealed::seal(
            &ClusterKey::from_bytes(symmetric),
            &wrap_ad(node_name),
            cluster_key.bytes(),
        )?;

        Ok(WrappedKey {
            ephemeral_public: base64::engine::general_purpose::STANDARD
                .encode(ephemeral_public.to_bytes()),
            sealed,
        })
    }

    /// Recover the cluster key, using this node's own keypair.
    pub fn unwrap_with(&self, keypair: &NodeKeypair, node_name: &str) -> Result<ClusterKey> {
        let ephemeral_public_bytes = decode_public(&self.ephemeral_public)?;
        let ephemeral_public = PublicKey::from(ephemeral_public_bytes);
        let shared = keypair.secret().diffie_hellman(&ephemeral_public);

        let recipient_public = keypair.public_key();
        let symmetric = derive_symmetric_key(
            shared.as_bytes(),
            &ephemeral_public_bytes,
            &recipient_public,
            node_name,
        );
        let plaintext = self
            .sealed
            .open(&ClusterKey::from_bytes(symmetric), &wrap_ad(node_name))?;

        let bytes: [u8; KEY_BYTES] = plaintext
            .as_slice()
            .try_into()
            .map_err(|_| SecretError::Malformed("unwrapped key was not 32 bytes".into()))?;
        Ok(ClusterKey::from_bytes(bytes))
    }
}

/// Associated-data string for the wrap step, kept distinct from the plain
/// secret name a `SecretEntry` would use.
fn wrap_ad(node_name: &str) -> String {
    format!("node-key-wrap:{node_name}")
}

fn decode_public(encoded: &str) -> Result<[u8; 32]> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| SecretError::Malformed(format!("not valid base64: {e}")))?;
    decoded
        .try_into()
        .map_err(|_| SecretError::Malformed("expected a 32 byte public key".into()))
}

/// Derive the one-time symmetric key for a wrap: the DH shared secret alone
/// is not used directly, both public keys and the node name are folded in
/// too, so the sealing key is unique to this exact exchange.
fn derive_symmetric_key(
    shared_secret: &[u8; 32],
    ephemeral_public: &[u8; 32],
    recipient_public: &[u8; 32],
    node_name: &str,
) -> [u8; KEY_BYTES] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(KDF_CONTEXT);
    hasher.update(ephemeral_public);
    hasher.update(recipient_public);
    hasher.update(node_name.as_bytes());
    hasher.update(shared_secret);
    let mut out = *hasher.finalize().as_bytes();
    let key = out;
    out.zeroize();
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrapped_key_round_trips_through_two_real_keypairs() {
        let cluster_key = ClusterKey::generate();
        let recipient = NodeKeypair::generate();

        let wrapped =
            WrappedKey::wrap_for(&cluster_key, "node-2", &recipient.public_key()).expect("wrap");
        let unwrapped = wrapped.unwrap_with(&recipient, "node-2").expect("unwrap");

        assert_eq!(unwrapped.encode(), cluster_key.encode());
    }

    #[test]
    fn the_wrong_recipient_keypair_cannot_unwrap_it() {
        let cluster_key = ClusterKey::generate();
        let recipient = NodeKeypair::generate();
        let stranger = NodeKeypair::generate();

        let wrapped =
            WrappedKey::wrap_for(&cluster_key, "node-2", &recipient.public_key()).expect("wrap");
        let err = wrapped.unwrap_with(&stranger, "node-2").unwrap_err();
        assert!(matches!(err, SecretError::Decrypt));
    }

    #[test]
    fn a_wrapped_key_cannot_be_moved_to_another_node_name() {
        let cluster_key = ClusterKey::generate();
        let recipient = NodeKeypair::generate();

        let wrapped =
            WrappedKey::wrap_for(&cluster_key, "node-2", &recipient.public_key()).expect("wrap");
        let err = wrapped.unwrap_with(&recipient, "node-3").unwrap_err();
        assert!(matches!(err, SecretError::Decrypt));
    }

    #[test]
    fn wrapping_twice_gives_different_ciphertext() {
        let cluster_key = ClusterKey::generate();
        let recipient = NodeKeypair::generate();

        let a =
            WrappedKey::wrap_for(&cluster_key, "node-2", &recipient.public_key()).expect("wrap");
        let b =
            WrappedKey::wrap_for(&cluster_key, "node-2", &recipient.public_key()).expect("wrap");
        assert_ne!(a.sealed.blob, b.sealed.blob);
        assert_ne!(a.ephemeral_public, b.ephemeral_public);
    }
}
