//! The cluster key: loading it, creating it, and keeping it off disk elsewhere.

use std::path::Path;

use base64::Engine;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Result, SecretError};

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
        let path = path.as_ref();
        check_permissions(path)?;

        let raw = std::fs::read_to_string(path).map_err(|source| SecretError::ReadKey {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(raw.trim()).map_err(|reason| SecretError::BadKey {
            path: path.to_path_buf(),
            reason,
        })
    }

    /// Load the key, or create one if the file is missing and we are allowed to.
    ///
    /// Returns whether a key was created, so the caller can tell the user about
    /// a key they now need to back up.
    pub fn load_or_create(path: impl AsRef<Path>, create: bool) -> Result<(Self, bool)> {
        let path = path.as_ref();
        if path.exists() {
            return Ok((Self::load(path)?, false));
        }
        if !create {
            return Err(SecretError::NoKey);
        }

        let key = Self::generate();
        match key.save(path) {
            Ok(()) => Ok((key, true)),
            // Someone else created it between the check and the write. Theirs
            // is as good as ours, and taking it is the only safe move: two
            // nodes each keeping their own key would seal secrets the other
            // cannot open.
            Err(SecretError::WriteKey { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                Ok((Self::load(path)?, false))
            }
            Err(e) => Err(e),
        }
    }

    /// Write the key out with owner-only permissions.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SecretError::WriteKey {
                path: path.to_path_buf(),
                source,
            })?;
        }

        // Write a complete temp file first, then link it into place. A reader
        // arriving mid-write would otherwise see a truncated key and conclude
        // the key is corrupt. The link fails if the target exists, which is
        // what makes this an atomic create rather than an overwrite.
        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        let result = self.write_temp(&temp).and_then(|()| {
            std::fs::hard_link(&temp, path).map_err(|source| SecretError::WriteKey {
                path: path.to_path_buf(),
                source,
            })
        });

        let _ = std::fs::remove_file(&temp);
        result
    }

    fn write_temp(&self, temp: &Path) -> Result<()> {
        use std::io::Write;

        // Owner read/write, group read, from creation: the `clouduler`
        // system group needs to read this file for the cli to seal/unseal
        // secrets as any group member, and there is no window where it is
        // briefly wider than that since the mode is set at open time, not
        // chmod'd on afterwards.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o640);
        }

        let mut file = options.open(temp).map_err(|source| SecretError::WriteKey {
            path: temp.to_path_buf(),
            source,
        })?;
        writeln!(file, "{}", self.encode()).map_err(|source| SecretError::WriteKey {
            path: temp.to_path_buf(),
            source,
        })?;
        file.sync_all().map_err(|source| SecretError::WriteKey {
            path: temp.to_path_buf(),
            source,
        })
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
            return Err(format!("expected {KEY_BYTES} bytes, got a different length"));
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

/// A key other users can read is a key you have to assume is gone — except
/// group read, which is deliberately allowed: the `clouduler` system group
/// is how a non-root operator's cli reads this file to seal/unseal secrets,
/// the same way it reads `cluster.yaml` to find the daemon. Anything else
/// outside owner (group write/execute, or any "other" bit) is still refused.
fn check_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|source| SecretError::ReadKey {
            path: path.to_path_buf(),
            source,
        })?;
        const ALLOWED_GROUP_READ: u32 = 0o040;
        if meta.permissions().mode() & 0o077 & !ALLOWED_GROUP_READ != 0 {
            return Err(SecretError::KeyPermissions {
                path: path.to_path_buf(),
            });
        }
    }
    let _ = path;
    Ok(())
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
