//! The web ui, shipped as a prebuilt zip beside the binary instead of inside it.
//!
//! Layout under `<data_dir>/self-update/ui`:
//!
//! - `<blake3>/` the extracted zip, named after the zip's hash;
//! - `<blake3>/.backend-version` the backend versions this ui may be served with, one per line
//!   (a ui that did not change between two releases is compatible with both);
//! - `current` a symlink to the directory a UI-only update switched to, preferred over any other
//!   directory that is compatible with the running version.
//!
//! The pairing rule: a node serves only a directory whose sidecar lists the version it is running, so
//! an updated node never serves the previous release's ui and a rolled-back one never serves the new.

use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// Directory name under `<data_dir>/self-update`.
pub const UI_DIR: &str = "ui";

const SIDECAR: &str = ".backend-version";
const CURRENT: &str = "current";
const PARTIAL_SUFFIX: &str = ".partial";

/// Refuse to inflate a zip past this many bytes (a zip bomb, not a real ui).
const MAX_UNPACKED: u64 = 1 << 30;

#[derive(Debug)]
pub enum UiStageError {
    HashMismatch { expected: String, actual: String },
    Zip(String),
    UnsafeEntry(String),
    MissingIndex,
    TooLarge,
    Io(std::io::Error),
}

impl std::fmt::Display for UiStageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HashMismatch { expected, actual } => {
                write!(
                    f,
                    "the ui zip hashes to {actual}, the release manifest says {expected}"
                )
            }
            Self::Zip(e) => write!(f, "the ui zip is unreadable: {e}"),
            Self::UnsafeEntry(name) => write!(f, "the ui zip holds an unsafe entry: {name}"),
            Self::MissingIndex => write!(f, "the ui zip has no index.html"),
            Self::TooLarge => write!(f, "the ui zip unpacks to more than {MAX_UNPACKED} bytes"),
            Self::Io(e) => write!(f, "staging the ui: {e}"),
        }
    }
}

impl std::error::Error for UiStageError {}

impl From<std::io::Error> for UiStageError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// `<data_dir>/self-update/ui`.
pub fn ui_root(data_dir: &Path) -> PathBuf {
    data_dir.join("self-update").join(UI_DIR)
}

fn read_versions(dir: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(dir.join(SIDECAR))
        .map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn write_versions(dir: &Path, versions: &BTreeSet<String>) -> std::io::Result<()> {
    let tmp = dir.join(format!("{SIDECAR}.tmp"));
    let mut body = versions.iter().cloned().collect::<Vec<_>>().join("\n");
    body.push('\n');
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, dir.join(SIDECAR))
}

/// The bundle directories under `root` (not the `current` link, not partial extractions).
fn bundles(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !name.starts_with('.') && !name.ends_with(PARTIAL_SUFFIX) && name != CURRENT
        })
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

fn serving_in(root: &Path, running_version: &str) -> Option<PathBuf> {
    if let Ok(target) = std::fs::read_link(root.join(CURRENT)) {
        if let Some(name) = target.file_name() {
            let candidate = root.join(name);
            if candidate.is_dir() && read_versions(&candidate).contains(running_version) {
                return Some(candidate);
            }
        }
    }
    bundles(root)
        .into_iter()
        .filter(|d| read_versions(d).contains(running_version))
        .max_by_key(|d| {
            std::fs::metadata(d.join(SIDECAR))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH)
        })
}

/// The directory this node serves for the backend `running_version`, or `None` when no staged ui is
/// compatible with it (the caller then shows its "waiting for an update" page).
pub fn serving_dir(data_dir: &Path, running_version: &str) -> Option<PathBuf> {
    serving_in(&ui_root(data_dir), running_version)
}

/// Whether the bundle with zip hash `hash` is already extracted.
pub fn has(data_dir: &Path, hash: &str) -> bool {
    ui_root(data_dir).join(hash).is_dir()
}

/// Record that the already extracted bundle `hash` may be served with `backend_version`. The zip is
/// identical; only the pairing changed.
pub fn mark_compatible(data_dir: &Path, hash: &str, backend_version: &str) -> std::io::Result<()> {
    let dir = ui_root(data_dir).join(hash);
    if !dir.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no staged ui {hash}"),
        ));
    }
    let mut versions = read_versions(&dir);
    if versions.insert(backend_version.to_string()) || !dir.join(SIDECAR).exists() {
        write_versions(&dir, &versions)?;
    }
    Ok(())
}

/// Points `current` at bundle `hash`: a symlink swap, so a reader sees the old or the new, never
/// neither. The previous bundle stays on disk.
pub fn activate(data_dir: &Path, hash: &str) -> std::io::Result<()> {
    activate_in(&ui_root(data_dir), hash)
}

fn activate_in(root: &Path, hash: &str) -> std::io::Result<()> {
    let tmp = root.join(format!(".{CURRENT}.tmp"));
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::os::unix::fs::symlink(hash, &tmp)?;
    std::fs::rename(&tmp, root.join(CURRENT))
}

/// Verifies `zip_bytes` against `expect_blake3`, extracts it next to its final place, and renames
/// it in, so a half-extracted ui is never visible. If the bundle already exists only its sidecar
/// gains `backend_version`. Returns the bundle directory.
pub fn stage_zip(
    data_dir: &Path,
    zip_bytes: &[u8],
    expect_blake3: &str,
    backend_version: &str,
) -> Result<PathBuf, UiStageError> {
    let actual = blake3::hash(zip_bytes).to_hex().to_string();
    if actual != expect_blake3 {
        return Err(UiStageError::HashMismatch {
            expected: expect_blake3.to_string(),
            actual,
        });
    }
    let root = ui_root(data_dir);
    std::fs::create_dir_all(&root)?;
    let final_dir = root.join(expect_blake3);
    if final_dir.is_dir() {
        mark_compatible(data_dir, expect_blake3, backend_version)?;
        adopt_if_unset(&root, expect_blake3);
        return Ok(final_dir);
    }

    let partial = root.join(format!("{expect_blake3}{PARTIAL_SUFFIX}"));
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    let result = extract(zip_bytes, &partial).and_then(|()| {
        write_versions(&partial, &BTreeSet::from([backend_version.to_string()]))?;
        Ok(())
    });
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&partial, &final_dir) {
        let _ = std::fs::remove_dir_all(&partial);
        // Another stager won the race with the same bytes.
        if final_dir.is_dir() {
            mark_compatible(data_dir, expect_blake3, backend_version)?;
        } else {
            return Err(e.into());
        }
    }
    adopt_if_unset(&root, expect_blake3);
    Ok(final_dir)
}

/// A node with no usable `current` link (a fresh install) starts pointing at what it staged.
fn adopt_if_unset(root: &Path, hash: &str) {
    if !root.join(CURRENT).exists() {
        if let Err(e) = activate_in(root, hash) {
            tracing::warn!(error = %e, "cannot point the ui link at the staged ui");
        }
    }
}

fn extract(zip_bytes: &[u8], dest: &Path) -> Result<(), UiStageError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .map_err(|e| UiStageError::Zip(e.to_string()))?;
    std::fs::create_dir_all(dest)?;
    let mut unpacked: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| UiStageError::Zip(e.to_string()))?;
        let raw_name = entry.name().to_string();
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| UiStageError::UnsafeEntry(raw_name.clone()))?;
        if raw_name.split('/').any(|part| part == "..") {
            return Err(UiStageError::UnsafeEntry(raw_name));
        }
        // S_IFMT: a symlink (or any special file) could point out of the directory.
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err(UiStageError::UnsafeEntry(raw_name));
            }
        }
        let target = dest.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut contents = Vec::new();
        entry
            .by_ref()
            .take(MAX_UNPACKED - unpacked + 1)
            .read_to_end(&mut contents)?;
        unpacked += contents.len() as u64;
        if unpacked > MAX_UNPACKED {
            return Err(UiStageError::TooLarge);
        }
        std::fs::write(&target, contents)?;
    }
    if !dest.join("index.html").is_file() {
        return Err(UiStageError::MissingIndex);
    }
    Ok(())
}

/// After a new binary is confirmed (or a rollback done): keeps only the bundle served for
/// `keep_version` (its sidecar trimmed to that version) and deletes every other bundle and any
/// `*.partial` extraction.
pub fn prune(data_dir: &Path, keep_version: &str) {
    prune_root(&ui_root(data_dir), keep_version);
}

/// [`prune`] for a ui root (`<data_dir>/self-update/ui`) already in hand.
pub fn prune_root(root: &Path, keep_version: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(PARTIAL_SUFFIX) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let keep = serving_in(root, keep_version);
    for dir in bundles(root) {
        if Some(&dir) == keep.as_ref() {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "cannot remove an unused ui");
        }
    }
    if let Some(keep) = keep {
        let _ = write_versions(&keep, &BTreeSet::from([keep_version.to_string()]));
        if let Some(name) = keep.file_name() {
            let _ = activate_in(root, &name.to_string_lossy());
        }
    }
}

/// After a UI-only update (same backend version): removes the bundles that only ever served
/// `version` and are not the one being served now. Bundles staged for another version stay.
pub fn prune_superseded(data_dir: &Path, version: &str) {
    let root = ui_root(data_dir);
    let keep = serving_in(&root, version);
    for dir in bundles(&root) {
        if Some(&dir) == keep.as_ref() {
            continue;
        }
        let versions = read_versions(&dir);
        if versions.len() == 1 && versions.contains(version) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn build_zip(entries: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body, mode) in entries {
            let mut options = SimpleFileOptions::default();
            if let Some(mode) = mode {
                options = options.unix_permissions(*mode);
            }
            writer.start_file(*name, options).expect("start");
            writer.write_all(body).expect("write");
        }
        writer.finish().expect("finish").into_inner()
    }

    fn good_zip(marker: &str) -> Vec<u8> {
        build_zip(&[
            ("index.html", marker.as_bytes(), None),
            ("assets/app.js", b"js", None),
        ])
    }

    fn hash(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    #[test]
    fn staging_extracts_pairs_and_serves() {
        let data = tempfile::tempdir().unwrap();
        let zip = good_zip("one");
        let h = hash(&zip);
        let dir = stage_zip(data.path(), &zip, &h, "1.0.0").unwrap();
        assert_eq!(dir, ui_root(data.path()).join(&h));
        assert_eq!(std::fs::read(dir.join("assets/app.js")).unwrap(), b"js");
        assert_eq!(serving_dir(data.path(), "1.0.0"), Some(dir));
        assert_eq!(serving_dir(data.path(), "1.0.1"), None);
        assert!(!ui_root(data.path()).join(format!("{h}.partial")).exists());
    }

    #[test]
    fn a_corrupt_download_fails_the_hash_check() {
        let data = tempfile::tempdir().unwrap();
        let zip = good_zip("one");
        let err = stage_zip(data.path(), &zip, &hash(b"something else"), "1.0.0").unwrap_err();
        assert!(matches!(err, UiStageError::HashMismatch { .. }));
        assert!(bundles(&ui_root(data.path())).is_empty());
    }

    #[test]
    fn extraction_rejects_parent_traversal_and_symlinks() {
        let data = tempfile::tempdir().unwrap();
        for entries in [
            vec![
                ("index.html", &b"x"[..], None),
                ("../evil", &b"x"[..], None),
            ],
            vec![("index.html", &b"x"[..], None), ("/abs", &b"x"[..], None)],
        ] {
            let zip = build_zip(&entries);
            let err = stage_zip(data.path(), &zip, &hash(&zip), "1.0.0").expect_err(&format!(
                "{:?}",
                entries.iter().map(|e| e.0).collect::<Vec<_>>()
            ));
            assert!(matches!(err, UiStageError::UnsafeEntry(_)), "{err}");
        }
        assert!(bundles(&ui_root(data.path())).is_empty());
        assert!(!data.path().join("self-update/evil").exists());
        let partials = std::fs::read_dir(ui_root(data.path())).unwrap().count();
        assert_eq!(partials, 0, "failed extractions are cleaned up");
    }

    #[test]
    fn extraction_rejects_symlink_entries() {
        let data = tempfile::tempdir().unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("index.html", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"x").unwrap();
        writer
            .add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        let zip = writer.finish().unwrap().into_inner();
        let err = stage_zip(data.path(), &zip, &hash(&zip), "1.0.0").unwrap_err();
        assert!(matches!(err, UiStageError::UnsafeEntry(_)), "{err}");
        assert!(bundles(&ui_root(data.path())).is_empty());
    }

    #[test]
    fn a_zip_without_index_html_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let zip = build_zip(&[("other.html", b"x", None)]);
        let err = stage_zip(data.path(), &zip, &hash(&zip), "1.0.0").unwrap_err();
        assert!(matches!(err, UiStageError::MissingIndex));
    }

    #[test]
    fn an_existing_bundle_only_gains_the_new_pairing() {
        let data = tempfile::tempdir().unwrap();
        let zip = good_zip("one");
        let h = hash(&zip);
        stage_zip(data.path(), &zip, &h, "1.0.0").unwrap();
        assert!(has(data.path(), &h));
        mark_compatible(data.path(), &h, "1.1.0").unwrap();
        // Still served by the old binary until it restarts, and by the new one at once.
        assert!(serving_dir(data.path(), "1.0.0").is_some());
        assert!(serving_dir(data.path(), "1.1.0").is_some());
        // Staging the same zip again is also just a pairing.
        stage_zip(data.path(), &zip, &h, "1.2.0").unwrap();
        assert!(serving_dir(data.path(), "1.2.0").is_some());
    }

    #[test]
    fn the_symlink_swap_is_atomic_and_keeps_the_previous_bundle() {
        let data = tempfile::tempdir().unwrap();
        let (a, b) = (good_zip("a"), good_zip("b"));
        let (ha, hb) = (hash(&a), hash(&b));
        let dir_a = stage_zip(data.path(), &a, &ha, "1.0.0").unwrap();
        let dir_b = stage_zip(data.path(), &b, &hb, "1.0.0").unwrap();
        // Staging does not switch: the link still points at what was serving.
        assert_eq!(serving_dir(data.path(), "1.0.0"), Some(dir_a.clone()));
        activate(data.path(), &hb).unwrap();
        assert_eq!(serving_dir(data.path(), "1.0.0"), Some(dir_b.clone()));
        assert!(dir_a.is_dir(), "the previous bundle is kept");
        assert!(!ui_root(data.path()).join(".current.tmp").exists());
        prune_superseded(data.path(), "1.0.0");
        assert!(!dir_a.exists());
        assert!(dir_b.is_dir());
    }

    #[test]
    fn prune_keeps_only_the_bundle_of_the_confirmed_version() {
        let data = tempfile::tempdir().unwrap();
        let (old, new) = (good_zip("old"), good_zip("new"));
        let (h_old, h_new) = (hash(&old), hash(&new));
        let dir_old = stage_zip(data.path(), &old, &h_old, "1.0.0").unwrap();
        let dir_new = stage_zip(data.path(), &new, &h_new, "2.0.0").unwrap();
        std::fs::create_dir_all(ui_root(data.path()).join("abc.partial")).unwrap();
        // Before the new binary is confirmed, each version has its own bundle.
        assert_eq!(serving_dir(data.path(), "1.0.0"), Some(dir_old.clone()));
        assert_eq!(serving_dir(data.path(), "2.0.0"), Some(dir_new.clone()));
        prune(data.path(), "2.0.0");
        assert!(!dir_old.exists());
        assert!(dir_new.is_dir());
        assert!(!ui_root(data.path()).join("abc.partial").exists());
        assert_eq!(serving_dir(data.path(), "2.0.0"), Some(dir_new));
    }

    #[test]
    fn a_rolled_back_node_drops_the_unconfirmed_bundle() {
        let data = tempfile::tempdir().unwrap();
        let (old, new) = (good_zip("old"), good_zip("new"));
        let dir_old = stage_zip(data.path(), &old, &hash(&old), "1.0.0").unwrap();
        let dir_new = stage_zip(data.path(), &new, &hash(&new), "2.0.0").unwrap();
        prune(data.path(), "1.0.0");
        assert!(dir_old.is_dir());
        assert!(!dir_new.exists());
        assert_eq!(serving_dir(data.path(), "2.0.0"), None);
    }
}
