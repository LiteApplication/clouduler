//! A node's own X25519 keypair, used to receive the cluster key wrapped to it.

use std::path::Path;

use base64::Engine;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::Result;
use crate::keyfile;
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
        keyfile::load(path.as_ref(), Self::parse)
    }

    /// Load the keypair, or create one if missing. Returns whether one was
    /// created. Unlike `ClusterKey`, creation is never refused.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<(Self, bool)> {
        keyfile::load_or_create(
            path.as_ref(),
            |p| Self::load(p),
            Self::generate,
            |key, p| key.save(p),
        )
    }

    /// Write the keypair out owner read/write, group read.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        keyfile::save(path.as_ref(), &self.encode())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SecretError;

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
