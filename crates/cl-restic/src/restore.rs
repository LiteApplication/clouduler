//! Pure logic for the three restore granularities (whole drive, one volume,
//! every volume a service uses): the `restic restore --include` filter each
//! needs. Orchestration lives in `clouduler`'s `backup_restore.rs`, which has the
//! docker and raft access this crate lacks.

/// The `--include` filter for restoring one volume out of a whole-drive snapshot,
/// relative to the snapshot's source directory (see `restic::ResticRepo::restore`).
/// `None` means no filter: a whole-drive restore, or the drive-root volume `.`.
///
/// Include patterns are globs, so metacharacters in the name are escaped.
pub fn include_path_for_volume(volume: &str) -> Option<String> {
    if volume == "." {
        return None;
    }
    let mut pattern = String::with_capacity(volume.len() + 1);
    pattern.push('/');
    for c in volume.chars() {
        if matches!(c, '\\' | '*' | '?' | '[') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    Some(pattern)
}

/// A snapshot argument that is safe to hand to restic: `latest` or a hex id
/// (any unambiguous prefix). Rejects a leading dash (an option) and `:` (the
/// `snapshot:subfolder` syntax, which would restore from an arbitrary path).
pub fn validate_snapshot_id(id: &str) -> Result<(), String> {
    if id == "latest" {
        return Ok(());
    }
    if (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(format!(
        "{id:?} is not a snapshot id (expected \"latest\" or a hex id)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_include_path_is_rooted() {
        assert_eq!(
            include_path_for_volume("gitlab-data").as_deref(),
            Some("/gitlab-data")
        );
        assert_eq!(include_path_for_volume("a/b").as_deref(), Some("/a/b"));
    }

    #[test]
    fn the_drive_root_volume_needs_no_filter() {
        assert_eq!(include_path_for_volume("."), None);
    }

    #[test]
    fn glob_characters_in_a_volume_name_are_escaped() {
        assert_eq!(
            include_path_for_volume("we*ird[1]?").as_deref(),
            Some("/we\\*ird\\[1]\\?")
        );
    }

    #[test]
    fn snapshot_ids_are_hex_or_latest() {
        assert!(validate_snapshot_id("latest").is_ok());
        assert!(validate_snapshot_id("e66d410d").is_ok());
        assert!(validate_snapshot_id(&"a".repeat(64)).is_ok());
        for bad in [
            "",
            "abc",
            "--help",
            "-r",
            "e66d410d:/etc",
            "xyz12345",
            "latest:/etc",
        ] {
            assert!(
                validate_snapshot_id(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }
}
