//! Sealing/opening the reserved-name secrets a restic destination owns - see
//! `cl_restic::secret_names` for what those names are and why they live in
//! that crate instead of here. This module is the other half: the actual
//! `Sealed::seal`/`open` calls, which need `cl_raft::node::ClusterNode` to
//! submit through raft, so they stay in the `clouduler` binary crate.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use cl_raft::node::ClusterNode;
use cl_raft::state::{BackupDestination, BackupDestinationKind, ClusterState, Request};
use cl_restic::destination::{remote_repo, ssh_command_args};
use cl_restic::restic::ResticRepo;
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
pub async fn ensure_password_secret(node: &ClusterNode, key: &ClusterKey, state: &ClusterState, name: &str) -> Result<()> {
    if state.secrets.contains_key(name) {
        return Ok(());
    }
    let password = generate_password();
    let sealed = Sealed::seal(key, name, password.as_bytes()).map_err(|e| anyhow!("sealing {name}: {e}"))?;
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

/// Opens a sealed secret this destination owns. `None` if absent (a
/// destination with no `credentials_secret`) or if it exists but this node's
/// key cannot open it - callers should treat that the same as "no
/// credentials available yet" rather than fail the whole run outright, since
/// a key-propagation lag heals itself on the next run.
pub fn open_secret(state: &ClusterState, key: &ClusterKey, name: &str) -> Option<String> {
    let entry = state.secrets.get(name)?;
    let sealed = Sealed::from_blob(entry.sealed.clone());
    let plaintext = sealed.open(key, name).ok()?;
    String::from_utf8(plaintext).ok()
}

/// Deletes the file it names when dropped. Every temp credential file this
/// module writes out of sealed cluster state is guarded by one, so it never
/// outlives the restic invocation that needed it.
pub struct TempFileGuard(pub PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Builds a ready-to-use `ResticRepo` for `destination`: opens its password
/// (required - restic needs one regardless of backend) and credentials (if
/// any) out of sealed cluster state and stages them as `0600` temp files
/// under `cred_dir`. Keep the returned guards alive for as long as the repo
/// is used - dropping them deletes the files.
pub fn build_repo(
    state: &ClusterState,
    key: &ClusterKey,
    cred_dir: &Path,
    destination: &BackupDestination,
) -> Result<(ResticRepo, Vec<TempFileGuard>)> {
    std::fs::create_dir_all(cred_dir)?;
    let password = open_secret(state, key, &destination.password_secret)
        .ok_or_else(|| anyhow!("cannot open the restic repo password for destination {}", destination.id))?;
    let password_path = cred_dir.join("password");
    write_secret_file(&password_path, &password)?;
    let mut guards = vec![TempFileGuard(password_path.clone())];

    let mut extra_args = ssh_command_args(destination);
    let mut credentials: Vec<(String, String)> = Vec::new();
    match &destination.kind {
        BackupDestinationKind::Ssh { host, port, user, .. } => {
            if let Some(secret_name) = &destination.credentials_secret {
                let pem = open_secret(state, key, secret_name)
                    .ok_or_else(|| anyhow!("cannot open the ssh key for destination {}", destination.id))?;
                let key_path = cred_dir.join("ssh_key");
                write_secret_file(&key_path, &pem)?;
                extra_args = vec![
                    "-o".to_string(),
                    format!("sftp.command=ssh {user}@{host} -p {port} -i {} -s sftp", key_path.display()),
                ];
                guards.push(TempFileGuard(key_path));
            }
        }
        BackupDestinationKind::S3 { .. } => {
            if let Some(secret_name) = &destination.credentials_secret {
                let creds = open_secret(state, key, secret_name)
                    .ok_or_else(|| anyhow!("cannot open the s3 credentials for destination {}", destination.id))?;
                let mut lines = creds.lines();
                credentials.push(("AWS_ACCESS_KEY_ID".to_string(), lines.next().unwrap_or_default().to_string()));
                credentials.push(("AWS_SECRET_ACCESS_KEY".to_string(), lines.next().unwrap_or_default().to_string()));
            }
        }
        BackupDestinationKind::NodeLocal { .. } => {}
    }

    let resolved = remote_repo(destination);
    Ok((ResticRepo::new(resolved, extra_args, &password_path, &credentials), guards))
}

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

    #[test]
    fn open_secret_round_trips_through_seal() {
        let key = ClusterKey::generate();
        let mut state = ClusterState::default();
        let sealed = Sealed::seal(&key, "_clouduler_restic_password_d1", b"hunter2").unwrap();
        state.secrets.insert(
            "_clouduler_restic_password_d1".into(),
            cl_raft::state::SecretEntry {
                name: "_clouduler_restic_password_d1".into(),
                sealed: sealed.blob,
                revision: 1,
                key_fingerprint: key.fingerprint(),
                owner: None,
            },
        );
        assert_eq!(
            open_secret(&state, &key, "_clouduler_restic_password_d1").as_deref(),
            Some("hunter2")
        );
        assert_eq!(open_secret(&state, &ClusterKey::generate(), "_clouduler_restic_password_d1"), None);
        assert_eq!(open_secret(&state, &key, "missing"), None);
    }
}
