//! Swapping the running binary for a freshly built one.
//!
//! Shared between `clouduler init` (which installs the binary a human
//! downloaded) and the self-update worker (which installs one the cluster
//! built for itself), and reachable from `cl-api`'s internal restart route
//! without that crate depending on the `clouduler` binary crate.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Copy `source` to `destination`, replacing whatever is there.
///
/// A running daemon cannot have its own executable overwritten in place
/// (`ETXTBSY`), so this writes to a temp file in the destination directory
/// and renames it into place, which swaps the directory entry instead.
pub fn install_binary(source: &Path, destination: &Path) -> std::io::Result<()> {
    if same_file(source, destination) {
        return Ok(());
    }
    let dir = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no directory", destination.display()),
        )
    })?;
    std::fs::create_dir_all(dir)?;

    let staged = dir.join(format!(
        ".{}.new",
        destination
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("clouduler"))
            .to_string_lossy()
    ));
    std::fs::copy(source, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, destination)?;
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_the_binary_copies_it_and_makes_it_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("downloaded");
        std::fs::write(&source, b"#!/bin/true\n").expect("write source");
        let destination = dir.path().join("bin").join("clouduler");

        install_binary(&source, &destination).expect("install");
        assert_eq!(std::fs::read(&destination).expect("read"), b"#!/bin/true\n");
        let mode = std::fs::metadata(&destination)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn installing_a_binary_over_itself_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clouduler");
        std::fs::write(&path, b"contents").expect("write");

        install_binary(&path, &path).expect("install");
        assert_eq!(std::fs::read(&path).expect("read"), b"contents");
    }
}
