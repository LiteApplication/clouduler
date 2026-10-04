//! Turning a `cl_raft::state::BackupDestination` into the repository string
//! and environment restic needs to reach it.

use std::path::Path;

use cl_raft::state::{BackupDestination, BackupDestinationKind};

/// A resolved restic repository target: the `-r`/`RESTIC_REPOSITORY` string,
/// plus any extra environment restic needs beyond the password (every
/// destination needs one regardless of kind - the runner supplies that
/// separately from the sealed secret, see `restic::ResticRepo::new`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepo {
    pub repository: String,
    pub extra_env: Vec<(String, String)>,
}

/// The repository string for data already local to this node - either
/// genuinely a `NodeLocal` destination on the node holding the drive, or
/// staged there by a one-off sync (see the crate doc comment). `local_path`
/// is that on-disk directory.
pub fn local_repo(local_path: &Path) -> ResolvedRepo {
    ResolvedRepo {
        repository: format!("local:{}", local_path.display()),
        extra_env: Vec::new(),
    }
}

/// The repository string for a destination reached directly over the
/// network: `Ssh` via restic's own `sftp:` backend, or `S3`. A `NodeLocal`
/// destination that already IS this node falls back to `local_repo`.
pub fn remote_repo(destination: &BackupDestination) -> ResolvedRepo {
    match &destination.kind {
        BackupDestinationKind::NodeLocal { path, .. } => local_repo(Path::new(path)),
        BackupDestinationKind::Ssh {
            host, user, path, ..
        } => ResolvedRepo {
            repository: format!("sftp:{user}@{host}:{path}"),
            extra_env: Vec::new(),
        },
        BackupDestinationKind::S3 {
            bucket,
            endpoint,
            region,
            prefix,
        } => {
            let mut repository = format!("s3:{endpoint}/{bucket}");
            if let Some(prefix) = prefix {
                repository.push('/');
                repository.push_str(prefix);
            }
            let extra_env = region
                .as_ref()
                .map(|region| vec![("AWS_DEFAULT_REGION".to_string(), region.clone())])
                .unwrap_or_default();
            ResolvedRepo {
                repository,
                extra_env,
            }
        }
    }
}

/// Extra `restic` arguments an `Ssh` destination needs when its port isn't
/// the default 22 - restic's `sftp:` shorthand has no port syntax, so a
/// non-default port has to be routed through `-o sftp.command`.
pub fn ssh_command_args(destination: &BackupDestination) -> Vec<String> {
    ssh_command_args_with_key(destination, None)
}

/// Like `ssh_command_args`, additionally pointing ssh at the private key
/// `identity` (a `0600` file staged from the destination's sealed
/// credentials) with `-i`. Without an identity and on the default port the
/// shorthand needs nothing, so the result is empty.
///
/// `user` and `host` land in a command line restic splits on whitespace, so
/// they must already have passed `cl_raft::backup_validate` (no leading dash,
/// no whitespace) - `credentials::build_repo` enforces that before calling
/// this.
pub fn ssh_command_args_with_key(
    destination: &BackupDestination,
    identity: Option<&Path>,
) -> Vec<String> {
    let BackupDestinationKind::Ssh {
        host, port, user, ..
    } = &destination.kind
    else {
        return Vec::new();
    };
    if *port == 22 && identity.is_none() {
        return Vec::new();
    }
    let mut command = format!("ssh {user}@{host} -p {port}");
    if let Some(identity) = identity {
        command.push_str(&format!(" -i {}", identity.display()));
    }
    command.push_str(" -s sftp");
    vec!["-o".to_string(), format!("sftp.command={command}")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(port: u16) -> BackupDestination {
        BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::Ssh {
                host: "backup.example.com".into(),
                port,
                user: "clouduler".into(),
                path: "/srv/restic".into(),
            },
            password_secret: "_clouduler_restic_password_d1".into(),
            credentials_secret: Some("_clouduler_restic_creds_d1".into()),
        }
    }

    #[test]
    fn ssh_repo_uses_the_sftp_backend() {
        let resolved = remote_repo(&ssh(22));
        assert_eq!(
            resolved.repository,
            "sftp:clouduler@backup.example.com:/srv/restic"
        );
        assert!(ssh_command_args(&ssh(22)).is_empty());
    }

    #[test]
    fn a_non_default_ssh_port_needs_an_explicit_sftp_command() {
        let args = ssh_command_args(&ssh(2222));
        assert_eq!(
            args,
            vec![
                "-o".to_string(),
                "sftp.command=ssh clouduler@backup.example.com -p 2222 -s sftp".to_string()
            ]
        );
    }

    #[test]
    fn an_identity_adds_a_dash_i_even_on_the_default_port() {
        let args = ssh_command_args_with_key(&ssh(22), Some(Path::new("/k/ssh_key")));
        assert_eq!(
            args,
            vec![
                "-o".to_string(),
                "sftp.command=ssh clouduler@backup.example.com -p 22 -i /k/ssh_key -s sftp"
                    .to_string()
            ]
        );
    }

    #[test]
    fn s3_repo_includes_the_prefix_when_set() {
        let destination = BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::S3 {
                bucket: "backups".into(),
                endpoint: "s3.example.com".into(),
                region: Some("us-east-1".into()),
                prefix: Some("clouduler/fast".into()),
            },
            password_secret: "_clouduler_restic_password_d1".into(),
            credentials_secret: Some("_clouduler_restic_creds_d1".into()),
        };
        let resolved = remote_repo(&destination);
        assert_eq!(
            resolved.repository,
            "s3:s3.example.com/backups/clouduler/fast"
        );
        assert_eq!(
            resolved.extra_env,
            vec![("AWS_DEFAULT_REGION".to_string(), "us-east-1".to_string())]
        );
    }

    #[test]
    fn node_local_resolves_to_a_local_repo() {
        let destination = BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::NodeLocal {
                node: "n1".into(),
                path: "/srv/backups".into(),
            },
            password_secret: "_clouduler_restic_password_d1".into(),
            credentials_secret: None,
        };
        let resolved = remote_repo(&destination);
        assert_eq!(resolved.repository, "local:/srv/backups");
    }
}
