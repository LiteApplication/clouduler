//! Generating the ed25519 deploy key clouduler uses to pull the self-update
//! source repo.
//!
//! ed25519 rather than rsa: every modern git host accepts it, keys are tiny,
//! and generation is fast enough to do inline in an api handler.

use ssh_key::{Algorithm, LineEnding, PrivateKey};

/// An ed25519 keypair, rendered in OpenSSH text form.
///
/// The private half is what gets sealed into the cluster secrets; the public
/// half is what the operator pastes into the git host as a deploy key.
pub struct Keypair {
    pub private_openssh: String,
    pub public_openssh: String,
}

/// Generate a fresh ed25519 keypair.
pub fn generate() -> Result<Keypair, String> {
    let mut rng = rand_core::OsRng;
    let key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|e| format!("generating ed25519 key: {e}"))?;

    let private_openssh = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("rendering private key: {e}"))?
        .to_string();
    let public_openssh = key
        .public_key()
        .to_openssh()
        .map_err(|e| format!("rendering public key: {e}"))?;

    Ok(Keypair {
        private_openssh,
        public_openssh,
    })
}

/// Recover the public half of a keypair from its OpenSSH private text.
///
/// Used when the ssh-key endpoint is called a second time: the public key is
/// never stored on its own, only computed at generation time and again here,
/// so a node that lost track of it (restarted before the operator copied it
/// down) can still recover it as long as it holds the cluster key that opens
/// the sealed private half.
pub fn derive_public(private_openssh: &str) -> Result<String, String> {
    let key = PrivateKey::from_openssh(private_openssh)
        .map_err(|e| format!("parsing private key: {e}"))?;
    key.public_key()
        .to_openssh()
        .map_err(|e| format!("rendering public key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_keypair_round_trips_through_openssh_parsing() {
        let pair = generate().expect("generate");
        assert!(pair.private_openssh.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(pair.public_openssh.starts_with("ssh-ed25519 "));

        let parsed_private =
            PrivateKey::from_openssh(&pair.private_openssh).expect("parse private key");
        assert_eq!(
            parsed_private.public_key().to_openssh().expect("public"),
            pair.public_openssh
        );
    }

    #[test]
    fn two_generated_keypairs_differ() {
        let a = generate().expect("generate");
        let b = generate().expect("generate");
        assert_ne!(a.private_openssh, b.private_openssh);
        assert_ne!(a.public_openssh, b.public_openssh);
    }

    #[test]
    fn the_public_key_can_be_recovered_from_the_private_half() {
        let pair = generate().expect("generate");
        assert_eq!(derive_public(&pair.private_openssh).expect("derive"), pair.public_openssh);
    }
}
