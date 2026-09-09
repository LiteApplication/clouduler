//! Pointing dockerd's `data-root` at a node-chosen directory.
//!
//! `data-root` is dockerd's own setting, not something clouduler's daemon
//! reads at runtime, so the only place to act on it is here: write it into
//! `/etc/docker/daemon.json` (preserving whatever else is already there) and
//! tell the operator to restart docker. Any data already sitting under the
//! previous root is left alone — moving it is destructive and dockerd handles
//! neither the move nor a merge of two roots, so doing that automatically
//! would risk containers or images the operator still wants.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

pub const DAEMON_JSON_PATH: &str = "/etc/docker/daemon.json";

/// Set (or leave alone) `data-root` in the docker daemon config at `path`.
/// Returns `true` if the file was created or changed.
pub fn set_data_root(path: &Path, data_root: &Path) -> Result<bool> {
    let mut doc = read(path)?;
    let target = Value::String(data_root.to_string_lossy().into_owned());

    if doc.get("data-root") == Some(&target) {
        return Ok(false);
    }
    doc.insert("data-root".to_string(), target);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(&doc).context("serializing daemon.json")?;
    std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

fn read(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Map::new()),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} does not parse as json", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn default_path() -> PathBuf {
    PathBuf::from(DAEMON_JSON_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_data_root_into_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");

        let changed = set_data_root(&path, Path::new("/mnt/docker")).unwrap();
        assert!(changed);

        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["data-root"], "/mnt/docker");
    }

    #[test]
    fn preserves_other_keys_already_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        std::fs::write(&path, r#"{"log-driver": "json-file"}"#).unwrap();

        set_data_root(&path, Path::new("/mnt/docker")).unwrap();

        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["log-driver"], "json-file");
        assert_eq!(doc["data-root"], "/mnt/docker");
    }

    #[test]
    fn setting_the_same_root_again_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");

        assert!(set_data_root(&path, Path::new("/mnt/docker")).unwrap());
        assert!(!set_data_root(&path, Path::new("/mnt/docker")).unwrap());
    }

    #[test]
    fn changing_the_root_updates_the_existing_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");

        set_data_root(&path, Path::new("/mnt/docker")).unwrap();
        assert!(set_data_root(&path, Path::new("/mnt/docker2")).unwrap());

        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["data-root"], "/mnt/docker2");
    }

    #[test]
    fn an_empty_file_is_treated_like_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.json");
        std::fs::write(&path, "").unwrap();

        assert!(set_data_root(&path, Path::new("/mnt/docker")).unwrap());
    }
}
