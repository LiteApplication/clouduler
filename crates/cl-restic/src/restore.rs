//! Pure logic for the three restore granularities (whole drive, one volume,
//! every volume a service uses): what `restic restore --include` filter each
//! one needs. Orchestration - stopping containers, actually invoking
//! `restic::ResticRepo::restore`, restarting - lives in `clouduler`'s own
//! `backup_restore.rs`, since it needs docker and raft access this crate
//! deliberately doesn't have.

/// The `--include` filter for restoring one volume out of a whole-drive
/// snapshot. `None` (a whole-drive restore) needs no filter at all - see
/// `restic::ResticRepo::restore`'s `include` parameter.
pub fn include_path_for_volume(volume: &str) -> String {
    format!("/{volume}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_include_path_is_rooted() {
        assert_eq!(include_path_for_volume("gitlab-data"), "/gitlab-data");
    }
}
