//! Opening a destination's sealed password/credentials out of cluster state
//! and staging them for one restic invocation. Shared by the daemon's backup
//! runner, restore, and the api's snapshot listing - everything that has to
//! talk to a repository builds its `ResticRepo` here, so S3 keys and ssh
//! identities are always wired up the same way.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cl_raft::backup_validate::validate_destination;
use cl_raft::state::{BackupDestination, BackupDestinationKind, ClusterState};
use cl_secret::{ClusterKey, Sealed};

use crate::destination::{remote_repo, ssh_command_args_with_key};
use crate::error::{ResticError, Result};
use crate::restic::ResticRepo;

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

/// Deletes the file (or directory tree) it names when dropped. Every temp
/// credential file written out of sealed cluster state is guarded by one, so
/// it never outlives the restic invocation that needed it.
pub struct TempFileGuard(pub PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if std::fs::remove_file(&self.0).is_err() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Creates `dir` (and any missing parents) as `0700` and tightens `dir`
/// itself if it already existed with looser permissions.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Writes `contents` to `path` without ever exposing it: the bytes go into a
/// fresh `0600` file created with `O_EXCL` (which also refuses a planted
/// symlink) next to `path`, which is then renamed into place - rename
/// replaces a symlink at `path` rather than following it. There is no moment
/// at which the secret sits in a world-readable file, unlike
/// `fs::write` followed by a `chmod`.
pub fn write_secret_file(path: &Path, contents: &str) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp = parent.join(format!(
            ".{file_name}.{}.{nanos}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let written = file
            .write_all(contents.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not pick an unused temp file name",
    ))
}

fn unique_dir_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn failed(message: impl Into<String>) -> ResticError {
    ResticError::Failed(message.into())
}

/// Builds a ready-to-use `ResticRepo` for `destination`: opens its password
/// (required - restic needs one regardless of backend) and credentials (if
/// any) out of sealed cluster state and stages them as `0600` files in a
/// fresh `0700` directory under `cred_dir` (unique per call, so two
/// concurrent uses of one destination never delete each other's files). Keep
/// the returned guards alive for as long as the repo is used - dropping them
/// deletes the files.
pub fn build_repo(
    state: &ClusterState,
    key: &ClusterKey,
    cred_dir: &Path,
    destination: &BackupDestination,
) -> Result<(ResticRepo, Vec<TempFileGuard>)> {
    // Re-checked here, not only when the destination was stored: this is the
    // last point before its fields are turned into a command line.
    validate_destination(destination).map_err(|e| {
        failed(format!(
            "backup destination {} is not usable: {e}",
            destination.id
        ))
    })?;

    create_private_dir(cred_dir)?;
    let work_dir = cred_dir.join(unique_dir_name());
    create_private_dir(&work_dir)?;
    let guards = vec![TempFileGuard(work_dir.clone())];

    let password = open_secret(state, key, &destination.password_secret).ok_or_else(|| {
        failed(format!(
            "cannot open the restic repo password for destination {}",
            destination.id
        ))
    })?;
    let password_path = work_dir.join("password");
    write_secret_file(&password_path, &password)?;

    let mut extra_args = ssh_command_args_with_key(destination, None);
    let mut credentials: Vec<(String, String)> = Vec::new();
    match &destination.kind {
        BackupDestinationKind::Ssh { .. } => {
            if let Some(secret_name) = &destination.credentials_secret {
                let pem = open_secret(state, key, secret_name).ok_or_else(|| {
                    failed(format!(
                        "cannot open the ssh key for destination {}",
                        destination.id
                    ))
                })?;
                let key_path = work_dir.join("ssh_key");
                write_secret_file(&key_path, &pem)?;
                extra_args = ssh_command_args_with_key(destination, Some(&key_path));
            }
        }
        BackupDestinationKind::S3 { .. } => {
            if let Some(secret_name) = &destination.credentials_secret {
                let creds = open_secret(state, key, secret_name).ok_or_else(|| {
                    failed(format!(
                        "cannot open the s3 credentials for destination {}",
                        destination.id
                    ))
                })?;
                let mut lines = creds.lines();
                credentials.push((
                    "AWS_ACCESS_KEY_ID".to_string(),
                    lines.next().unwrap_or_default().to_string(),
                ));
                credentials.push((
                    "AWS_SECRET_ACCESS_KEY".to_string(),
                    lines.next().unwrap_or_default().to_string(),
                ));
            }
        }
        BackupDestinationKind::NodeLocal { .. } => {}
    }

    let resolved = remote_repo(destination);
    Ok((
        ResticRepo::new(resolved, extra_args, &password_path, &credentials),
        guards,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::SecretEntry;

    fn state_with(key: &ClusterKey, name: &str, plaintext: &[u8]) -> ClusterState {
        let mut state = ClusterState::default();
        let sealed = Sealed::seal(key, name, plaintext).unwrap();
        state.secrets.insert(
            name.into(),
            SecretEntry {
                name: name.into(),
                sealed: sealed.blob,
                revision: 1,
                key_fingerprint: key.fingerprint(),
                owner: None,
            },
        );
        state
    }

    #[test]
    fn open_secret_round_trips_through_seal() {
        let key = ClusterKey::generate();
        let state = state_with(&key, "_clouduler_restic_password_d1", b"hunter2");
        assert_eq!(
            open_secret(&state, &key, "_clouduler_restic_password_d1").as_deref(),
            Some("hunter2")
        );
        assert_eq!(
            open_secret(
                &state,
                &ClusterKey::generate(),
                "_clouduler_restic_password_d1"
            ),
            None
        );
        assert_eq!(open_secret(&state, &key, "missing"), None);
    }

    #[test]
    fn a_secret_file_is_born_0600_in_a_0700_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("b");
        create_private_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let path = dir.join("password");
        write_secret_file(&path, "s3cret").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "s3cret");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Only the final file is left behind, no temp files.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn writing_a_secret_replaces_a_planted_symlink_instead_of_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        let path = tmp.path().join("password");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        write_secret_file(&path, "s3cret").unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "s3cret");
        assert!(!std::fs::symlink_metadata(&path).unwrap().is_symlink());
    }

    #[test]
    fn an_ssh_destination_with_a_key_gets_an_identity_and_cleans_up() {
        let key = ClusterKey::generate();
        let mut state = state_with(&key, "_clouduler_restic_password_d1", b"pw");
        let creds = state_with(&key, "_clouduler_restic_creds_d1", b"PEM");
        state.secrets.extend(creds.secrets);
        let destination = BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::Ssh {
                host: "h.example.com".into(),
                port: 22,
                user: "u".into(),
                path: "/srv".into(),
            },
            password_secret: "_clouduler_restic_password_d1".into(),
            credentials_secret: Some("_clouduler_restic_creds_d1".into()),
        };
        let tmp = tempfile::tempdir().unwrap();
        let (repo, guards) = build_repo(&state, &key, tmp.path(), &destination).unwrap();
        let joined = repo.extra_args.join(" ");
        assert!(joined.contains("-i "), "identity missing: {joined}");
        assert!(joined.contains("u@h.example.com"));
        drop(guards);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_destination_with_an_injecting_host_never_builds() {
        let key = ClusterKey::generate();
        let state = state_with(&key, "_clouduler_restic_password_d1", b"pw");
        let destination = BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::Ssh {
                host: "-oProxyCommand=evil".into(),
                port: 22,
                user: "u".into(),
                path: "/srv".into(),
            },
            password_secret: "_clouduler_restic_password_d1".into(),
            credentials_secret: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        assert!(build_repo(&state, &key, tmp.path(), &destination).is_err());
    }
}
