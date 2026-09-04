//! The reserved secret names a `BackupDestination` owns: the repo password
//! (always present - restic requires one regardless of backend) and backend
//! credentials (an ssh key or S3 keypair; absent for `NodeLocal`). Kept here
//! rather than in `clouduler`, since `cl-api`'s generic secret listing needs
//! `is_reserved_restic_secret` too and cannot depend on the `clouduler`
//! binary crate. The actual sealing/opening lives in `clouduler`'s
//! `backup_secrets`, since that needs `cl_raft::node::ClusterNode`, which
//! this crate deliberately doesn't depend on.

/// The secret name a `BackupDestination` with id `destination_id` stores its
/// restic repo password under, if the caller doesn't already have one.
pub fn password_secret_name(destination_id: &str) -> String {
    format!("_clouduler_restic_password_{destination_id}")
}

pub fn credentials_secret_name(destination_id: &str) -> String {
    format!("_clouduler_restic_creds_{destination_id}")
}

/// Whether `name` is one of this feature's reserved secrets - excluded from
/// the generic `/api/secrets` listing the same way `cl_backup`'s deploy key is.
pub fn is_reserved_restic_secret(name: &str) -> bool {
    name.starts_with("_clouduler_restic_password_") || name.starts_with("_clouduler_restic_creds_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names_are_recognized_and_others_are_not() {
        assert!(is_reserved_restic_secret(&password_secret_name("d1")));
        assert!(is_reserved_restic_secret(&credentials_secret_name("d1")));
        assert!(!is_reserved_restic_secret("db_password"));
    }
}
