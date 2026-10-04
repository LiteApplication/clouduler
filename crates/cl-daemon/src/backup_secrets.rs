//! Sealing/opening the reserved-name secrets a restic destination owns - see
//! `cl_restic::secret_names` for what those names are and why they live in
//! that crate instead of here. This module is the other half: the actual
//! `Sealed::seal` call, which needs `cl_raft::node::ClusterNode` to
//! submit through raft, so they stay in the `clouduler` binary crate.

use anyhow::{anyhow, bail, Result};
use cl_raft::node::ClusterNode;
use cl_raft::state::{ClusterState, Request};
use cl_secret::{ClusterKey, Sealed};
use uuid::Uuid;

/// A fresh, high-entropy repo password: two random UUIDs concatenated, since
/// this workspace has no other dependency on a password-generation crate and
/// a UUIDv4's 122 random bits (x2) is far more entropy than restic's own
/// repository encryption needs.
fn generate_password() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Ensures `name` exists as a sealed secret, generating and submitting a
/// fresh password under it if it doesn't. Idempotent: a second caller racing
/// this (two nodes both about to run against the same destination) just seals
/// a password that never gets used, since `PutSecret` doesn't reject an
/// overwrite - see the doc comment on `open_password` about why the *reader*
/// treats "we sealed one nobody else picked" as harmless.
pub async fn ensure_password_secret(
    node: &ClusterNode,
    key: &ClusterKey,
    state: &ClusterState,
    name: &str,
) -> Result<()> {
    if state.secrets.contains_key(name) {
        return Ok(());
    }
    let password = generate_password();
    let sealed =
        Sealed::seal(key, name, password.as_bytes()).map_err(|e| anyhow!("sealing {name}: {e}"))?;
    let response = node
        .submit(Request::PutSecret {
            name: name.to_string(),
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
        .await?;
    if let Some(error) = response.error {
        bail!("could not store the generated restic password: {error}");
    }
    Ok(())
}

// The password/credential staging itself lives in `cl-restic` so `cl-api`
// (snapshot listing) builds repositories exactly the same way; re-exported
// here because the daemon and CLI know it by this path.
pub use cl_restic::credentials::{build_repo, open_secret, TempFileGuard};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_passwords_are_long_and_distinct() {
        let a = generate_password();
        let b = generate_password();
        assert_ne!(a, b);
        assert!(a.len() >= 32);
    }
}
