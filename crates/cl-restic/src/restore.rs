//! Pure logic for the three restore granularities (whole drive, one volume,
//! every volume a service uses): what `restic restore --include` filter each
//! one needs. Orchestration - stopping containers, actually invoking
//! `restic::ResticRepo::restore`, restarting - lives in `clouduler`'s own
//! `backup_restore.rs`, since it needs docker and raft access this crate
//! deliberately doesn't have.

/// The `--include` filter for restoring one volume out of a whole-drive
/// snapshot, relative to the snapshot's source directory (see
/// `restic::ResticRepo::restore` for why it is relative). `None` means "no
/// filter": a whole-drive restore, or the drive-root volume `.` (a
/// whole-drive mount), which *is* the whole drive.
///
/// restic include patterns are globs, so the glob metacharacters in a volume
/// name are backslash-escaped to match literally.
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

/// A snapshot argument that is safe to hand to restic: the literal `latest`
/// or a hex id (restic accepts any unambiguous prefix, 4+ chars in practice).
/// Rejects anything else - in particular a leading dash (an option) or a
/// `:` (restic's `snapshot:subfolder` syntax, which would let a caller
/// restore from a path of their choosing).
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
