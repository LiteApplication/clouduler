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
///
/// The binary being replaced is kept next to it as `<name>.old` (hard-linked,
/// so it costs no space until the new one diverges and survives the rename
/// because the old inode is still linked). Only one generation is kept: each
/// install replaces the previous `.old`.
///
/// To roll a node back by hand after a bad release, on that node:
/// `mv /usr/local/bin/clouduler.old /usr/local/bin/clouduler && systemctl
/// restart clouduler`. Nothing swaps it back automatically: the unit is
/// `Restart=always` and has no notion of a "known good" binary, so a crash
/// loop on the new one needs this done by an operator.
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
    keep_previous(destination)?;
    std::fs::rename(&staged, destination)?;
    Ok(())
}

/// `<destination>.old`, in the same directory.
pub fn previous_path(destination: &Path) -> std::path::PathBuf {
    let mut name = destination
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("clouduler"))
        .to_os_string();
    name.push(".old");
    destination.with_file_name(name)
}

/// Preserves whatever is at `destination` as its `.old`. Nothing to keep on a
/// first install.
fn keep_previous(destination: &Path) -> std::io::Result<()> {
    if !destination.exists() {
        return Ok(());
    }
    let old = previous_path(destination);
    match std::fs::remove_file(&old) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if std::fs::hard_link(destination, &old).is_err() {
        std::fs::copy(destination, &old)?;
    }
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
    fn replacing_a_binary_keeps_the_previous_one_as_dot_old() {
        let dir = tempfile::tempdir().expect("tempdir");
        let destination = dir.path().join("clouduler");
        std::fs::write(&destination, b"v1").expect("write v1");
        let source = dir.path().join("downloaded");
        std::fs::write(&source, b"v2").expect("write v2");

        install_binary(&source, &destination).expect("install");
        assert_eq!(std::fs::read(&destination).expect("read"), b"v2");
        assert_eq!(
            std::fs::read(previous_path(&destination)).expect("read old"),
            b"v1"
        );

        // A second install replaces the old one rather than failing on it.
        let third = dir.path().join("third");
        std::fs::write(&third, b"v3").expect("write v3");
        install_binary(&third, &destination).expect("install again");
        assert_eq!(
            std::fs::read(previous_path(&destination)).expect("read old"),
            b"v2"
        );
    }

    #[test]
    fn a_first_install_leaves_no_dot_old() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("downloaded");
        std::fs::write(&source, b"v1").expect("write");
        let destination = dir.path().join("clouduler");
        install_binary(&source, &destination).expect("install");
        assert!(!previous_path(&destination).exists());
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
