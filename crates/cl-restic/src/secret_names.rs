//! The reserved secret names a `BackupDestination` owns: the repo password
//! (always present) and backend credentials (ssh key or S3 keypair; absent for
//! `NodeLocal`). Kept here because `cl-api`'s secret listing needs
//! `is_reserved_restic_secret` and cannot depend on the `clouduler` binary
//! crate. Sealing/opening lives in `clouduler`'s `backup_secrets`.

/// The secret name a destination stores its restic repo password under.
pub fn password_secret_name(destination_id: &str) -> String {
    format!("_clouduler_restic_password_{destination_id}")
}

pub fn credentials_secret_name(destination_id: &str) -> String {
    format!("_clouduler_restic_creds_{destination_id}")
}

/// Whether `name` is reserved, so it is excluded from the generic `/api/secrets`
/// listing like `cl_backup`'s deploy key.
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
