//! Reading and writing the key files shared by [`crate::ClusterKey`] and
//! [`crate::NodeKeypair`]: one base64 line, owner read/write plus group read,
//! created atomically.

use std::path::Path;

use crate::error::{Result, SecretError};

/// A key other users can read is a key you have to assume is gone, except group
/// read, which is allowed so the `clouduler` group can run the cli. Group
/// write/execute and any "other" bit are refused.
fn check_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|source| SecretError::ReadKey {
            path: path.to_path_buf(),
            source,
        })?;
        const ALLOWED_GROUP_READ: u32 = 0o040;
        if meta.permissions().mode() & 0o077 & !ALLOWED_GROUP_READ != 0 {
            return Err(SecretError::KeyPermissions {
                path: path.to_path_buf(),
            });
        }
    }
    let _ = path;
    Ok(())
}

/// Check permissions, read the file and hand its trimmed text to `parse`.
pub(crate) fn load<T>(
    path: &Path,
    parse: impl FnOnce(&str) -> std::result::Result<T, String>,
) -> Result<T> {
    check_permissions(path)?;

    let raw = std::fs::read_to_string(path).map_err(|source| SecretError::ReadKey {
        path: path.to_path_buf(),
        source,
    })?;
    parse(raw.trim()).map_err(|reason| SecretError::BadKey {
        path: path.to_path_buf(),
        reason,
    })
}

/// Load the key at `path` if the file exists, else create one with `generate`
/// and `save` it. Returns whether a key was created.
pub(crate) fn load_or_create<T>(
    path: &Path,
    load: impl Fn(&Path) -> Result<T>,
    generate: impl FnOnce() -> T,
    save: impl FnOnce(&T, &Path) -> Result<()>,
) -> Result<(T, bool)> {
    if path.exists() {
        return Ok((load(path)?, false));
    }

    let key = generate();
    match save(&key, path) {
        Ok(()) => Ok((key, true)),
        // Someone else created it between the check and the write. Theirs
        // is as good as ours, and taking it is the only safe move: two
        // nodes each keeping their own key would seal secrets the other
        // cannot open.
        Err(SecretError::WriteKey { source, .. })
            if source.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            Ok((load(path)?, false))
        }
        Err(e) => Err(e),
    }
}

/// Write `line` to `path` with owner-only (plus group read) permissions.
pub(crate) fn save(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SecretError::WriteKey {
            path: path.to_path_buf(),
            source,
        })?;
    }

    // Write a complete temp file first, then link it into place. A reader
    // arriving mid-write would otherwise see a truncated key and conclude
    // the key is corrupt. The link fails if the target exists, which is
    // what makes this an atomic create rather than an overwrite.
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
    let result = write_temp(&temp, line).and_then(|()| {
        std::fs::hard_link(&temp, path).map_err(|source| SecretError::WriteKey {
            path: path.to_path_buf(),
            source,
        })
    });

    let _ = std::fs::remove_file(&temp);
    result
}

fn write_temp(temp: &Path, line: &str) -> Result<()> {
    use std::io::Write;

    // Owner read/write, group read, from creation: the `clouduler`
    // system group needs to read this file for the cli to seal/unseal
    // secrets as any group member, and there is no window where it is
    // briefly wider than that since the mode is set at open time, not
    // chmod'd on afterwards.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o640);
    }

    let mut file = options.open(temp).map_err(|source| SecretError::WriteKey {
        path: temp.to_path_buf(),
        source,
    })?;
    writeln!(file, "{line}").map_err(|source| SecretError::WriteKey {
        path: temp.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| SecretError::WriteKey {
        path: temp.to_path_buf(),
        source,
    })
}
