//! The cluster key: loading it, creating it, and keeping it off disk elsewhere.

use std::path::Path;

use base64::Engine;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Result, SecretError};
use crate::keyfile;

pub const KEY_BYTES: usize = 32;

/// The symmetric key every secret in the cluster is sealed with.
///
/// Zeroized on drop. It is deliberately not `Clone`, `Debug` or serializable,
/// so it cannot end up in a log line, an api response or the database.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct ClusterKey {
    bytes: [u8; KEY_BYTES],
}

/// Prints the fingerprint, never the key. Deriving Debug here would be a way
/// for the key to reach a log line.
impl std::fmt::Debug for ClusterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterKey")
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

impl ClusterKey {
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self { bytes }
    }

    pub(crate) fn bytes(&self) -> &[u8; KEY_BYTES] {
        &self.bytes
    }

    /// A fresh random key.
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_BYTES];
        rand::fill(&mut bytes);
        Self { bytes }
    }

    /// Read a key file. The file holds the key base64 encoded on one line, so
    /// it can be pasted into the web ui when rebuilding a cluster.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        keyfile::load(path.as_ref(), Self::parse)
    }

    /// Load the key, or create one if the file is missing and we are allowed to.
    ///
    /// Returns whether a key was created, so the caller can tell the user about
    /// a key they now need to back up.
    pub fn load_or_create(path: impl AsRef<Path>, create: bool) -> Result<(Self, bool)> {
        let path = path.as_ref();
        if !path.exists() && !create {
            return Err(SecretError::NoKey);
        }
        keyfile::load_or_create(
            path,
            |p| Self::load(p),
            Self::generate,
            |key, p| key.save(p),
        )
    }

    /// Write the key out with owner-only permissions.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        keyfile::save(path.as_ref(), &self.encode())
    }

    /// Base64 form, for the key file and for showing the operator once.
    pub fn encode(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.bytes)
    }

    /// Parse the base64 form, as uploaded through the web ui.
    pub fn parse(encoded: &str) -> std::result::Result<Self, String> {
        let mut decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| format!("not valid base64: {e}"))?;

        if decoded.len() != KEY_BYTES {
            decoded.zeroize();
            return Err(format!(
                "expected {KEY_BYTES} bytes, got a different length"
            ));
        }

        let mut bytes = [0u8; KEY_BYTES];
        bytes.copy_from_slice(&decoded);
        decoded.zeroize();
        Ok(Self { bytes })
    }

    /// Short fingerprint, safe to log and to show in the ui. Lets an operator
    /// check two nodes hold the same key without either revealing it.
    pub fn fingerprint(&self) -> String {
        // Truncated hash of the key. Eight bytes is plenty to spot a mismatch
        // and far too little to work backwards from.
        let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.bytes {
            acc ^= byte as u64;
            acc = acc.wrapping_mul(0x1000_0000_01b3);
        }
        format!("{acc:016x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_base64() {
        let key = ClusterKey::generate();
        let parsed = ClusterKey::parse(&key.encode()).expect("parse");
        assert_eq!(parsed.bytes(), key.bytes());
        assert_eq!(parsed.fingerprint(), key.fingerprint());
    }

    #[test]
    fn generated_keys_differ() {
        let a = ClusterKey::generate();
        let b = ClusterKey::generate();
        assert_ne!(a.bytes(), b.bytes());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn a_wrong_length_key_is_rejected() {
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        let err = ClusterKey::parse(&short).unwrap_err();
        assert!(err.contains("expected 32 bytes"));
    }

    #[test]
    fn junk_is_rejected() {
        assert!(ClusterKey::parse("not base64!!").is_err());
    }

    #[test]
    fn saving_creates_a_group_readable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join("secrets.key");

        let key = ClusterKey::generate();
        key.save(&path).expect("save");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o640,
                "key file must be owner read/write, group read, and nothing for others"
            );
        }

        let loaded = ClusterKey::load(&path).expect("load");
        assert_eq!(loaded.bytes(), key.bytes());
    }

    #[test]
    fn a_group_readable_key_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.key");
        std::fs::write(&path, ClusterKey::generate().encode()).expect("write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
            ClusterKey::load(&path).expect("a group-read key must load");
        }
    }

    #[test]
    fn a_world_readable_key_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.key");
        std::fs::write(&path, ClusterKey::generate().encode()).expect("write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            let err = ClusterKey::load(&path).unwrap_err();
            assert!(matches!(err, SecretError::KeyPermissions { .. }));
        }
    }

    #[test]
    fn a_group_writable_key_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.key");
        std::fs::write(&path, ClusterKey::generate().encode()).expect("write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Group read plus group write: only the read bit is allowed.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).expect("chmod");
            let err = ClusterKey::load(&path).unwrap_err();
            assert!(matches!(err, SecretError::KeyPermissions { .. }));
        }
    }

    #[test]
    fn load_or_create_makes_a_key_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.key");

        let (first, created) = ClusterKey::load_or_create(&path, true).expect("create");
        assert!(created, "the first call must create the key");

        let (second, created) = ClusterKey::load_or_create(&path, true).expect("load");
        assert!(!created, "the second call must reuse it");
        assert_eq!(first.bytes(), second.bytes());
    }

    #[test]
    fn load_or_create_refuses_to_invent_a_key_when_told_not_to() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.key");
        assert!(matches!(
            ClusterKey::load_or_create(&path, false),
            Err(SecretError::NoKey)
        ));
    }

    #[test]
    fn a_lost_creation_race_adopts_the_winner_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.key");

        // Stand in for another node that got there first.
        let winner = ClusterKey::generate();
        winner.save(&path).expect("save");

        let (adopted, created) = ClusterKey::load_or_create(&path, true).expect("adopt");
        assert!(!created);
        assert_eq!(adopted.bytes(), winner.bytes());
    }

    #[test]
    fn saving_over_an_existing_key_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets.key");
        ClusterKey::generate().save(&path).expect("save");
        // Overwriting would make every stored secret unreadable.
        assert!(ClusterKey::generate().save(&path).is_err());
    }
}
