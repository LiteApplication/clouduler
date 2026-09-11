//! The ssh keypair clouduler pushes the git backup with.
//!
//! Cluster-wide rather than per-node: leadership fails over between managers
//! and the export task always runs from whoever currently leads, so a
//! per-node key would need every manager's key registered on the git host.
//! Instead one keypair is generated on demand and stored like any other
//! cluster-owned singleton (the reserved-name convention already used for
//! e.g. the self-update github token): the public half in `configs` (plain,
//! anyone can read it to hand to the git host), the private half in
//! `secrets` (sealed with the internal `ClusterKey`, same as any other
//! secret).

use base64::Engine;
use cl_raft::state::{ClusterState, ConfigEntry, SecretEntry};
use cl_secret::{ClusterKey, Sealed};
use ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::error::{BackupError, Result};

pub const DEPLOY_KEY_PUBLIC_CONFIG: &str = "_clouduler_git_deploy_key_pub";
pub const DEPLOY_KEY_PRIVATE_SECRET: &str = "_clouduler_git_deploy_key";

/// An ssh keypair in the string forms `git2`/git hosts expect: OpenSSH
/// `authorized_keys` format for the public half, OpenSSH PEM for the private
/// half.
pub struct DeployKeypair {
    pub public_openssh: String,
    pub private_pem: String,
}

impl DeployKeypair {
    pub fn generate() -> Result<Self> {
        let key = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
            .map_err(|e| BackupError::KeyGen(e.to_string()))?;
        let public_openssh = key
            .public_key()
            .to_openssh()
            .map_err(|e| BackupError::KeyGen(e.to_string()))?;
        let private_pem = key
            .to_openssh(LineEnding::LF)
            .map_err(|e| BackupError::KeyGen(e.to_string()))?
            .to_string();
        Ok(Self {
            public_openssh,
            private_pem,
        })
    }
}

pub fn is_present(state: &ClusterState) -> bool {
    state.configs.contains_key(DEPLOY_KEY_PUBLIC_CONFIG)
        && state.secrets.contains_key(DEPLOY_KEY_PRIVATE_SECRET)
}

/// The public half, as it would go into a `configs` entry.
pub fn public_config_entry(keypair: &DeployKeypair) -> ConfigEntry {
    ConfigEntry {
        name: DEPLOY_KEY_PUBLIC_CONFIG.to_string(),
        content: base64::engine::general_purpose::STANDARD.encode(&keypair.public_openssh),
        revision: 1,
        owner: None,
    }
}

/// The private half, sealed under `key`, as it would go into a `secrets` entry.
pub fn private_secret_entry(keypair: &DeployKeypair, key: &ClusterKey) -> Result<SecretEntry> {
    let sealed = Sealed::seal(
        key,
        DEPLOY_KEY_PRIVATE_SECRET,
        keypair.private_pem.as_bytes(),
    )?;
    Ok(SecretEntry {
        name: DEPLOY_KEY_PRIVATE_SECRET.to_string(),
        sealed: sealed.blob,
        revision: 1,
        key_fingerprint: key.fingerprint(),
        owner: None,
    })
}

/// Decodes the public half out of cluster state, for display in the ui.
pub fn public_from_state(state: &ClusterState) -> Option<String> {
    let entry = state.configs.get(DEPLOY_KEY_PUBLIC_CONFIG)?;
    let bytes = entry.decode().ok()?;
    String::from_utf8(bytes).ok()
}

/// Decrypts the private half out of cluster state, for git2 auth. `None` if
/// absent or this node's key cannot open it (rather than erroring the whole
/// export over one bad decrypt).
pub fn load_private(state: &ClusterState, key: &ClusterKey) -> Option<String> {
    let entry = state.secrets.get(DEPLOY_KEY_PRIVATE_SECRET)?;
    let sealed = Sealed::from_blob(entry.sealed.clone());
    let plaintext = sealed.open(key, DEPLOY_KEY_PRIVATE_SECRET).ok()?;
    String::from_utf8(plaintext).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_keypair_round_trips_through_cluster_state() {
        let keypair = DeployKeypair::generate().expect("generate");
        let key = ClusterKey::generate();

        let mut state = ClusterState::default();
        assert!(!is_present(&state));

        let public_entry = public_config_entry(&keypair);
        state
            .configs
            .insert(public_entry.name.clone(), public_entry);
        let private_entry = private_secret_entry(&keypair, &key).expect("seal");
        state
            .secrets
            .insert(private_entry.name.clone(), private_entry);

        assert!(is_present(&state));
        assert_eq!(
            public_from_state(&state).as_deref(),
            Some(keypair.public_openssh.as_str())
        );
        assert_eq!(
            load_private(&state, &key).as_deref(),
            Some(keypair.private_pem.as_str())
        );
    }

    #[test]
    fn the_wrong_key_cannot_load_the_private_half() {
        let keypair = DeployKeypair::generate().expect("generate");
        let key = ClusterKey::generate();
        let mut state = ClusterState::default();
        let private_entry = private_secret_entry(&keypair, &key).expect("seal");
        state
            .secrets
            .insert(private_entry.name.clone(), private_entry);

        assert!(load_private(&state, &ClusterKey::generate()).is_none());
    }

    #[test]
    fn a_generated_public_key_is_ed25519() {
        let keypair = DeployKeypair::generate().expect("generate");
        assert!(keypair.public_openssh.starts_with("ssh-ed25519 "));
    }
}
