//! Syntax checks for restic backup destinations. Lives here (not in
//! `cl-restic`, which depends on this crate) so the state machine can reject
//! a bad destination at write time while `cl-restic` re-checks the same
//! rules right before it builds a command line from one.
//!
//! Why this matters: an `Ssh` destination's `user` and `host` are
//! interpolated into restic's `sftp.command=ssh {user}@{host} ...`, which
//! restic splits on whitespace and runs. A host of `-oProxyCommand=...` (or a
//! user containing a space) would be a remote-command-execution primitive for
//! anyone who can create a destination.

use crate::state::{BackupDestination, BackupDestinationKind};

/// Why `value` is unacceptable as the `what` field of a destination, or
/// `Ok(())`.
fn check_token(what: &str, value: &str, extra_ok: &[char]) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(format!("{what} {value:?} must not start with a dash"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || extra_ok.contains(&c))
    {
        return Err(format!(
            "{what} {value:?} contains characters that are not allowed"
        ));
    }
    Ok(())
}

/// Free-form but still inert: no leading dash (restic/ssh would read it as an
/// option), no whitespace or control characters (the value ends up inside
/// whitespace-split command lines and repository strings).
fn check_plain(what: &str, value: &str, allow_spaces: bool) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if value.starts_with('-') {
        return Err(format!("{what} {value:?} must not start with a dash"));
    }
    if value
        .chars()
        .any(|c| c.is_control() || (!allow_spaces && c.is_whitespace()))
    {
        return Err(format!(
            "{what} {value:?} must not contain whitespace or control characters"
        ));
    }
    Ok(())
}

pub fn validate_ssh_user(user: &str) -> Result<(), String> {
    check_token("ssh user", user, &['.', '_', '-'])
}

pub fn validate_ssh_host(host: &str) -> Result<(), String> {
    // Colon for bare IPv6 literals.
    check_token("ssh host", host, &['.', '_', '-', ':'])
}

/// Every free-text field of `destination`. Called by the state machine on
/// `PutBackupDestination` and again by `cl-restic` before it builds a
/// command from a stored one.
pub fn validate_destination(destination: &BackupDestination) -> Result<(), String> {
    match &destination.kind {
        BackupDestinationKind::NodeLocal { path, .. } => check_plain("path", path, true),
        BackupDestinationKind::Ssh {
            host,
            port,
            user,
            path,
        } => {
            validate_ssh_user(user)?;
            validate_ssh_host(host)?;
            if *port == 0 {
                return Err("ssh port must not be 0".to_string());
            }
            check_plain("path", path, false)
        }
        BackupDestinationKind::S3 {
            bucket,
            endpoint,
            region,
            prefix,
        } => {
            check_plain("bucket", bucket, false)?;
            check_plain("endpoint", endpoint, false)?;
            if let Some(region) = region {
                check_token("region", region, &['.', '_', '-'])?;
            }
            if let Some(prefix) = prefix {
                check_plain("prefix", prefix, false)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(host: &str, user: &str, path: &str) -> BackupDestination {
        BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::Ssh {
                host: host.into(),
                port: 22,
                user: user.into(),
                path: path.into(),
            },
            password_secret: "p".into(),
            credentials_secret: None,
        }
    }

    #[test]
    fn ordinary_ssh_destinations_pass() {
        assert!(
            validate_destination(&ssh("backup.example.com", "clouduler", "/srv/restic")).is_ok()
        );
        assert!(validate_destination(&ssh("fe80::1", "u_1", "restic")).is_ok());
    }

    #[test]
    fn option_injection_through_ssh_fields_is_rejected() {
        assert!(validate_destination(&ssh("-oProxyCommand=touch", "u", "/p")).is_err());
        assert!(validate_destination(&ssh("h", "-oProxyCommand=x", "/p")).is_err());
        assert!(validate_destination(&ssh("h", "u v", "/p")).is_err());
        assert!(validate_destination(&ssh("h x", "u", "/p")).is_err());
        assert!(validate_destination(&ssh("h", "u", "-/p")).is_err());
        assert!(validate_destination(&ssh("h", "u", "/p with space")).is_err());
        assert!(validate_destination(&ssh("h\n", "u", "/p")).is_err());
    }

    #[test]
    fn s3_fields_are_checked_and_node_local_allows_spaces() {
        let s3 = |endpoint: &str, prefix: Option<&str>| BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::S3 {
                bucket: "b".into(),
                endpoint: endpoint.into(),
                region: None,
                prefix: prefix.map(str::to_string),
            },
            password_secret: "p".into(),
            credentials_secret: None,
        };
        assert!(validate_destination(&s3("s3.example.com", Some("a/b"))).is_ok());
        assert!(validate_destination(&s3("-x", None)).is_err());
        assert!(validate_destination(&s3("s3.example.com", Some("a b"))).is_err());

        let local = BackupDestination {
            id: "d1".into(),
            name: "d1".into(),
            kind: BackupDestinationKind::NodeLocal {
                node: "n".into(),
                path: "/mnt/my backups".into(),
            },
            password_secret: "p".into(),
            credentials_secret: None,
        };
        assert!(validate_destination(&local).is_ok());
    }
}
