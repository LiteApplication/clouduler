//! Sealing and opening a secret value.

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use zeroize::Zeroize;

use crate::error::{Result, SecretError};
use crate::key::ClusterKey;

pub const NONCE_BYTES: usize = 24;

/// A sealed secret, in the form stored in the database.
///
/// `name` is bound in as associated data, so a sealed value cannot be moved to
/// a different secret name without the decryption failing. Without that, anyone
/// who can edit a stack could point a service's `db_password` at the ciphertext
/// of some other secret and have the cluster helpfully decrypt it into their
/// container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// Base64 of nonce followed by ciphertext.
    pub blob: String,
}

impl Sealed {
    /// Encrypt `plaintext` for the secret called `name`.
    pub fn seal(key: &ClusterKey, name: &str, plaintext: &[u8]) -> Result<Self> {
        let cipher = XChaCha20Poly1305::new(key.bytes().into());

        let mut nonce_bytes = [0u8; NONCE_BYTES];
        rand::fill(&mut nonce_bytes);
        let nonce = chacha20poly1305::XNonce::from(nonce_bytes);

        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| SecretError::Malformed("encryption failed".into()))?;

        let mut framed = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        framed.extend_from_slice(&nonce_bytes);
        framed.extend_from_slice(&ciphertext);

        let blob = base64::engine::general_purpose::STANDARD.encode(&framed);
        framed.zeroize();
        Ok(Sealed { blob })
    }

    /// Decrypt, checking it was sealed under this exact name.
    pub fn open(&self, key: &ClusterKey, name: &str) -> Result<Vec<u8>> {
        let framed = base64::engine::general_purpose::STANDARD
            .decode(self.blob.trim())
            .map_err(|e| SecretError::Malformed(format!("not valid base64: {e}")))?;

        if framed.len() <= NONCE_BYTES {
            return Err(SecretError::Malformed("too short to hold a nonce".into()));
        }

        let (nonce_bytes, ciphertext) = framed.split_at(NONCE_BYTES);
        let mut nonce = [0u8; NONCE_BYTES];
        nonce.copy_from_slice(nonce_bytes);
        let cipher = XChaCha20Poly1305::new(key.bytes().into());

        cipher
            .decrypt(
                &chacha20poly1305::XNonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: name.as_bytes(),
                },
            )
            .map_err(|_| SecretError::Decrypt)
    }

    pub fn from_blob(blob: impl Into<String>) -> Self {
        Sealed { blob: blob.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_round_trips() {
        let key = ClusterKey::generate();
        let sealed = Sealed::seal(&key, "db_password", b"hunter2").expect("seal");
        assert_eq!(sealed.open(&key, "db_password").expect("open"), b"hunter2");
    }

    #[test]
    fn the_wrong_key_cannot_open_it() {
        let sealed = Sealed::seal(&ClusterKey::generate(), "db", b"hunter2").expect("seal");
        let err = sealed.open(&ClusterKey::generate(), "db").unwrap_err();
        assert!(matches!(err, SecretError::Decrypt));
    }

    #[test]
    fn a_secret_cannot_be_moved_to_another_name() {
        let key = ClusterKey::generate();
        let sealed = Sealed::seal(&key, "db_password", b"hunter2").expect("seal");
        // Renaming the secret must not silently hand over the old value.
        let err = sealed.open(&key, "api_token").unwrap_err();
        assert!(matches!(err, SecretError::Decrypt));
    }

    #[test]
    fn tampering_is_detected() {
        let key = ClusterKey::generate();
        let sealed = Sealed::seal(&key, "db", b"hunter2").expect("seal");

        let mut raw = base64::engine::general_purpose::STANDARD
            .decode(&sealed.blob)
            .expect("decode");
        let last = raw.len() - 1;
        raw[last] ^= 0xff;

        let tampered = Sealed::from_blob(base64::engine::general_purpose::STANDARD.encode(&raw));
        assert!(matches!(tampered.open(&key, "db"), Err(SecretError::Decrypt)));
    }

    #[test]
    fn sealing_twice_gives_different_ciphertext() {
        let key = ClusterKey::generate();
        let a = Sealed::seal(&key, "db", b"hunter2").expect("seal");
        let b = Sealed::seal(&key, "db", b"hunter2").expect("seal");
        // A fresh nonce each time, so identical secrets are not identifiable.
        assert_ne!(a.blob, b.blob);
        assert_eq!(a.open(&key, "db").expect("open"), b.open(&key, "db").expect("open"));
    }

    #[test]
    fn malformed_blobs_are_reported_as_such() {
        let key = ClusterKey::generate();
        assert!(matches!(
            Sealed::from_blob("not base64!!").open(&key, "db"),
            Err(SecretError::Malformed(_))
        ));
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 8]);
        assert!(matches!(
            Sealed::from_blob(short).open(&key, "db"),
            Err(SecretError::Malformed(_))
        ));
    }

    #[test]
    fn empty_secrets_are_allowed() {
        let key = ClusterKey::generate();
        let sealed = Sealed::seal(&key, "empty", b"").expect("seal");
        assert!(sealed.open(&key, "empty").expect("open").is_empty());
    }

    #[test]
    fn large_secrets_round_trip() {
        let key = ClusterKey::generate();
        let big = vec![7u8; 512 * 1024];
        let sealed = Sealed::seal(&key, "cert", &big).expect("seal");
        assert_eq!(sealed.open(&key, "cert").expect("open"), big);
    }
}
