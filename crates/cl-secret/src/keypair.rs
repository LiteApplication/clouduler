//! A node's own X25519 keypair, used to receive the cluster key wrapped to it.

use std::path::Path;

use base64::Engine;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::{Result, SecretError};

pub const PUBLIC_KEY_BYTES: usize = 32;

/// A node's own asymmetric keypair. Unlike [`crate::ClusterKey`], every node
/// always has one of these — there is no "receiving node" distinction, so
/// `load_or_create` never refuses to create it.
///
/// Not `Clone` or `Debug`-of-the-private-half, for the same reason as
/// `ClusterKey`: the private key must not be able to reach a log line.
pub struct NodeKeypair {
    secret: StaticSecret,
}

impl std::fmt::Debug for NodeKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeKeypair")
            .field("public", &self.public_key_encoded())
            .finish()
    }
}

impl NodeKeypair {
    /// A fresh random keypair.
    pub fn generate() -> Self {
        Self {
            secret: StaticSecret::random(),
        }
    }

    pub(crate) fn secret(&self) -> &StaticSecret {
        &self.secret
    }

    /// Read a keypair file. Same format as `ClusterKey`: base64 on one line.
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

    /// Load the keypair, or create one if the file is missing.
    ///
    /// Every node always has its own keypair, so unlike `ClusterKey` there is
    /// no case where creating one is refused. Returns whether a keypair was
    /// created, mirroring `ClusterKey::load_or_create`.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<(Self, bool)> {
        let path = path.as_ref();
        if path.exists() {
            return Ok((Self::load(path)?, false));
        }

        let keypair = Self::generate();
        match keypair.save(path) {
            Ok(()) => Ok((keypair, true)),
            // Someone else created it between the check and the write —
            // adopt theirs, same reasoning as ClusterKey::load_or_create.
            Err(SecretError::WriteKey { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                Ok((Self::load(path)?, false))
            }
            Err(e) => Err(e),
        }
    }

    /// Write the keypair out owner read/write, group read.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SecretError::WriteKey {
                path: path.to_path_buf(),
                source,
            })?;
        }

        // Atomic create via tmp + hard_link, same as ClusterKey::save.
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

    /// Base64 of the private scalar, for the keypair file.
    fn encode(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.secret.to_bytes())
    }

    fn parse(encoded: &str) -> std::result::Result<Self, String> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| format!("not valid base64: {e}"))?;

        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| "expected 32 bytes, got a different length".to_string())?;
        Ok(Self {
            secret: StaticSecret::from(bytes),
        })
    }

    /// This node's public key, to publish into the cluster roster.
    pub fn public_key(&self) -> [u8; PUBLIC_KEY_BYTES] {
        PublicKey::from(&self.secret).to_bytes()
    }

    /// Base64 form of the public key, for `NodeState`/logs.
    pub fn public_key_encoded(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.public_key())
    }
}

/// A key other users can read is a key you have to assume is gone. Same check
/// as `ClusterKey`'s, including the group-read exception — see its doc comment.
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
    fn keypairs_round_trip_through_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node.key");

        let keypair = NodeKeypair::generate();
        keypair.save(&path).expect("save");

        let loaded = NodeKeypair::load(&path).expect("load");
        assert_eq!(loaded.public_key(), keypair.public_key());
    }

    #[test]
    fn generated_keypairs_differ() {
        let a = NodeKeypair::generate();
        let b = NodeKeypair::generate();
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn saving_creates_a_group_readable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join("node.key");

        NodeKeypair::generate().save(&path).expect("save");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o640,
                "keypair file must be owner read/write, group read, and nothing for others"
            );
        }
    }

    #[test]
    fn a_group_readable_keypair_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node.key");
        std::fs::write(&path, NodeKeypair::generate().encode()).expect("write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
            NodeKeypair::load(&path).expect("a group-read keypair must load");
        }
    }

    #[test]
    fn a_world_readable_keypair_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node.key");
        std::fs::write(&path, NodeKeypair::generate().encode()).expect("write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            let err = NodeKeypair::load(&path).unwrap_err();
            assert!(matches!(err, SecretError::KeyPermissions { .. }));
        }
    }

    #[test]
    fn load_or_create_makes_a_keypair_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node.key");

        let (first, created) = NodeKeypair::load_or_create(&path).expect("create");
        assert!(created, "the first call must create the keypair");

        let (second, created) = NodeKeypair::load_or_create(&path).expect("load");
        assert!(!created, "the second call must reuse it");
        assert_eq!(first.public_key(), second.public_key());
    }

    #[test]
    fn a_lost_creation_race_adopts_the_winner_keypair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("node.key");

        let winner = NodeKeypair::generate();
        winner.save(&path).expect("save");

        let (adopted, created) = NodeKeypair::load_or_create(&path).expect("adopt");
        assert!(!created);
        assert_eq!(adopted.public_key(), winner.public_key());
    }
}
