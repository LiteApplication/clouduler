//! Re-encrypting secret plaintext for the admin's age recipient.
//!
//! The cluster's own `ClusterKey` never leaves the cluster, so a secret bound
//! for the git backup is decrypted with it and re-sealed under an age
//! recipient the admin supplies instead. Only the admin holds the matching
//! identity (private key); clouduler never sees it, except transiently in
//! the offline `restore` command, and never persists it.

use std::io::{Read, Write};
use std::str::FromStr;

use age::secrecy::SecretString;

use crate::error::{BackupError, Result};

pub fn parse_recipient(s: &str) -> Result<age::x25519::Recipient> {
    age::x25519::Recipient::from_str(s.trim()).map_err(|e| BackupError::BadRecipient(e.to_string()))
}

pub fn parse_identity(s: &str) -> Result<age::x25519::Identity> {
    age::x25519::Identity::from_str(s.trim()).map_err(|e| BackupError::BadIdentity(e.to_string()))
}

pub fn encrypt(recipient: &age::x25519::Recipient, plaintext: &[u8]) -> Result<Vec<u8>> {
    let recipient: &dyn age::Recipient = recipient;
    let encryptor = age::Encryptor::with_recipients(std::iter::once(recipient))
        .map_err(|e| BackupError::Encrypt(e.to_string()))?;

    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| BackupError::Encrypt(e.to_string()))?;
    writer
        .write_all(plaintext)
        .map_err(|e| BackupError::Encrypt(e.to_string()))?;
    writer.finish().map_err(|e| BackupError::Encrypt(e.to_string()))?;
    Ok(out)
}

pub fn decrypt(identity: &age::x25519::Identity, ciphertext: &[u8]) -> Result<Vec<u8>> {
    let decryptor =
        age::Decryptor::new(ciphertext).map_err(|e| BackupError::Decrypt(e.to_string()))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| BackupError::Decrypt(e.to_string()))?;
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| BackupError::Decrypt(e.to_string()))?;
    Ok(out)
}

/// Reads an age identity file (as produced by `age-keygen`), skipping blank
/// lines and `#` comments, and returning the first `AGE-SECRET-KEY-1...` line.
pub fn parse_identity_file(contents: &str) -> Result<age::x25519::Identity> {
    contents
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or_else(|| BackupError::BadIdentity("no identity found".into()))
        .and_then(parse_identity)
}

/// Kept for callers that need to zeroize the identity string explicitly.
pub fn secret_string(s: &str) -> SecretString {
    SecretString::from(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_round_trips() {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let ciphertext = encrypt(&recipient, b"hunter2").expect("encrypt");
        let plaintext = decrypt(&identity, &ciphertext).expect("decrypt");
        assert_eq!(plaintext, b"hunter2");
    }

    #[test]
    fn the_wrong_identity_cannot_open_it() {
        let recipient = age::x25519::Identity::generate().to_public();
        let ciphertext = encrypt(&recipient, b"hunter2").expect("encrypt");
        let wrong = age::x25519::Identity::generate();
        assert!(decrypt(&wrong, &ciphertext).is_err());
    }

    #[test]
    fn empty_plaintext_round_trips() {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let ciphertext = encrypt(&recipient, b"").expect("encrypt");
        assert_eq!(decrypt(&identity, &ciphertext).expect("decrypt"), b"");
    }

    #[test]
    fn a_malformed_recipient_is_rejected() {
        assert!(parse_recipient("not-an-age-key").is_err());
    }

    #[test]
    fn recipients_round_trip_through_their_string_form() {
        let identity = age::x25519::Identity::generate();
        let recipient_str = identity.to_public().to_string();
        let parsed = parse_recipient(&recipient_str).expect("parse");
        let ciphertext = encrypt(&parsed, b"hunter2").expect("encrypt");
        assert_eq!(decrypt(&identity, &ciphertext).expect("decrypt"), b"hunter2");
    }
}
