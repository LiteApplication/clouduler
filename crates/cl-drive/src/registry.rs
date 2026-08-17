//! Which drives this node implements, and safe path resolution within them.
//!
//! A volume name comes from a stack file, so it must never be trusted enough
//! to let a mount escape its drive root. Validation is purely component-wise
//! (reject `..`, empty and absolute components) rather than canonicalizing,
//! since the target path may not exist yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cl_config::ClusterConfig;
use cl_raft::VolumeId;

use crate::error::{DriveError, Result};

/// Drives one node implements, mapped to their local backing directories.
pub struct DriveRegistry {
    node: String,
    drives: BTreeMap<String, PathBuf>,
}

impl DriveRegistry {
    /// Build from the cluster config and this node's name.
    pub fn new(config: &ClusterConfig, node: &str) -> Result<Self> {
        let entry = config
            .node(node)
            .ok_or_else(|| DriveError::Config(format!("unknown node {node}")))?;
        Ok(Self {
            node: node.to_string(),
            drives: entry.drives.clone(),
        })
    }

    /// Build directly from a name to path map, for tests and standalone tools.
    pub fn from_drives(node: impl Into<String>, drives: BTreeMap<String, PathBuf>) -> Self {
        Self {
            node: node.into(),
            drives,
        }
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
    }

    pub fn drives(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.drives.iter().map(|(name, path)| (name.as_str(), path.as_path()))
    }

    /// Local directory backing `drive` on this node.
    pub fn root(&self, drive: &str) -> Result<&Path> {
        self.drives
            .get(drive)
            .map(PathBuf::as_path)
            .ok_or_else(|| DriveError::NotImplemented {
                drive: drive.to_string(),
                node: self.node.clone(),
            })
    }

    /// The local path for a volume, without touching the filesystem.
    pub fn volume_path(&self, id: &VolumeId) -> Result<PathBuf> {
        let root = self.root(&id.drive)?;
        resolve_relative(root, &id.volume)
    }

    /// The local path for a volume, creating its directory if missing.
    pub fn ensure_volume(&self, id: &VolumeId) -> Result<PathBuf> {
        let path = self.volume_path(id)?;
        std::fs::create_dir_all(&path).map_err(|source| DriveError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }
}

/// Join `rel` onto `root`, rejecting anything that could climb out: empty
/// components, `.`/`..`, and leading slashes (which show up as an empty
/// first component once split).
pub fn resolve_relative(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Err(DriveError::PathEscape { path: rel.to_string() });
    }
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\0') {
            return Err(DriveError::PathEscape { path: rel.to_string() });
        }
        path.push(part);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> DriveRegistry {
        DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([
                ("fast".to_string(), PathBuf::from("/srv/fast")),
                ("bulk".to_string(), PathBuf::from("/srv/bulk")),
            ]),
        )
    }

    #[test]
    fn implements_only_configured_drives() {
        let reg = registry();
        assert!(reg.implements("fast"));
        assert!(!reg.implements("cold"));
    }

    #[test]
    fn root_of_unknown_drive_is_rejected() {
        let err = registry().root("cold").unwrap_err();
        assert!(matches!(err, DriveError::NotImplemented { .. }));
    }

    #[test]
    fn volume_path_stays_under_the_drive_root() {
        let reg = registry();
        let id = VolumeId::new("fast", "gitlab-data");
        assert_eq!(reg.volume_path(&id).unwrap(), PathBuf::from("/srv/fast/gitlab-data"));
    }

    #[test]
    fn nested_volume_names_are_allowed() {
        let reg = registry();
        let id = VolumeId::new("fast", "a/b");
        assert_eq!(reg.volume_path(&id).unwrap(), PathBuf::from("/srv/fast/a/b"));
    }

    #[test]
    fn dot_dot_is_rejected() {
        let reg = registry();
        for name in ["..", "../etc", "a/../../etc", "a/..", "..data"] {
            let id = VolumeId::new("fast", name);
            let result = reg.volume_path(&id);
            if name == "..data" {
                // Not a traversal: ".." only matters as a whole path component.
                assert!(result.is_ok(), "{name} should be a legal literal name");
            } else {
                assert!(matches!(result, Err(DriveError::PathEscape { .. })), "{name} should be rejected");
            }
        }
    }

    #[test]
    fn absolute_and_empty_components_are_rejected() {
        let reg = registry();
        for name in ["/etc/passwd", "a//b", ""] {
            let id = VolumeId::new("fast", name);
            assert!(matches!(reg.volume_path(&id), Err(DriveError::PathEscape { .. })), "{name} should be rejected");
        }
    }

    #[test]
    fn ensure_volume_creates_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), tmp.path().to_path_buf())]),
        );
        let id = VolumeId::new("fast", "data");
        let path = reg.ensure_volume(&id).unwrap();
        assert!(path.is_dir());
    }
}
