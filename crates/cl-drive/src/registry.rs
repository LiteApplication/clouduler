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

/// Whether this node serves a drive's data, mounts it from whoever does, or
/// keeps its own replica of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveRole {
    /// A replicated drive: this node's copy is kept current by delta sync.
    Replicate,
    /// A served drive, and this is the node holding the real data.
    Serve,
    /// A served drive, held by another node; this node mounts it over FUSE.
    Mount,
}

/// Drives one node implements, mapped to their local backing directories.
pub struct DriveRegistry {
    node: String,
    drives: BTreeMap<String, PathBuf>,
    /// Serving node for each drive in `served` mode. A drive absent here is
    /// replicated. Present so `role` can tell serving from mounting without
    /// every caller having to carry the cluster config around.
    served_by: BTreeMap<String, String>,
}

impl DriveRegistry {
    /// Build from the cluster config and this node's name.
    pub fn new(config: &ClusterConfig, node: &str) -> Result<Self> {
        let entry = config
            .node(node)
            .ok_or_else(|| DriveError::Config(format!("unknown node {node}")))?;
        let served_by = config
            .drives
            .iter()
            .filter(|(_, drive)| drive.is_served())
            .map(|(name, drive)| (name.clone(), drive.served_by.clone().unwrap_or_default()))
            .collect();
        Ok(Self {
            node: node.to_string(),
            drives: entry.drives.clone(),
            served_by,
        })
    }

    /// Build directly from a name to path map, for tests and standalone tools.
    /// Every drive is treated as replicated; use `with_served_by` to mark
    /// some as served.
    pub fn from_drives(node: impl Into<String>, drives: BTreeMap<String, PathBuf>) -> Self {
        Self {
            node: node.into(),
            drives,
            served_by: BTreeMap::new(),
        }
    }

    /// Mark drives as served, mapping each to the node holding its data.
    /// For tests and standalone tools that build a registry by hand.
    pub fn with_served_by(mut self, served_by: BTreeMap<String, String>) -> Self {
        self.served_by = served_by;
        self
    }

    /// Whether this node serves, mounts, or replicates a drive. Unknown
    /// drives (this node doesn't implement it) come back as `Replicate`,
    /// same as the default for an ordinary drive; callers that care check
    /// `implements` first.
    pub fn role(&self, drive: &str) -> DriveRole {
        match self.served_by.get(drive) {
            Some(server) if server == &self.node => DriveRole::Serve,
            Some(_) => DriveRole::Mount,
            None => DriveRole::Replicate,
        }
    }

    /// Whether a drive is served (either by this node or another), as
    /// opposed to replicated. The sync scheduler uses this to skip served
    /// drives entirely: a served drive has one copy, so periodic delta sync
    /// makes no sense for it.
    pub fn is_served(&self, drive: &str) -> bool {
        self.served_by.contains_key(drive)
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

/// Resolve `rel` under `root` the way `resolve_relative` does, then guard
/// against a symlink already inside the drive pointing back out of it.
/// `resolve_relative` only looks at path components, since a sync target
/// may not exist yet; a served drive's paths usually do exist, and a
/// crafted request could walk through a symlinked directory that
/// component-checking alone can't see. Canonicalizing the deepest existing
/// ancestor and checking it's still under the drive root catches that,
/// while still allowing paths whose final component doesn't exist yet
/// (`create`, `mkdir`, a rename target).
pub fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let candidate = resolve_relative(root, rel)?;

    let canonical_root = root.canonicalize().map_err(|source| DriveError::Io { path: root.to_path_buf(), source })?;

    let mut existing = candidate.as_path();
    while !existing.exists() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => break,
        }
    }
    let canonical_existing =
        existing.canonicalize().map_err(|source| DriveError::Io { path: existing.to_path_buf(), source })?;

    if !canonical_existing.starts_with(&canonical_root) {
        return Err(DriveError::PathEscape { path: rel.to_string() });
    }
    Ok(candidate)
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

    #[test]
    fn role_is_replicate_for_a_drive_with_no_serving_node() {
        let reg = registry();
        assert_eq!(reg.role("fast"), DriveRole::Replicate);
        assert!(!reg.is_served("fast"));
    }

    #[test]
    fn role_is_serve_on_the_serving_node_and_mount_elsewhere() {
        let serving = DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/srv/fast"))]),
        )
        .with_served_by(BTreeMap::from([("fast".to_string(), "n1".to_string())]));
        assert_eq!(serving.role("fast"), DriveRole::Serve);
        assert!(serving.is_served("fast"));

        let mounting = DriveRegistry::from_drives(
            "n2",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/mnt/fast"))]),
        )
        .with_served_by(BTreeMap::from([("fast".to_string(), "n1".to_string())]));
        assert_eq!(mounting.role("fast"), DriveRole::Mount);
        assert!(mounting.is_served("fast"));
    }

    #[test]
    fn resolve_in_root_accepts_a_plain_path() {
        let tmp = tempfile::tempdir().unwrap();
        fs_write(tmp.path().join("a.txt"), b"hi");
        let resolved = resolve_in_root(tmp.path(), "a.txt").unwrap();
        assert_eq!(resolved, tmp.path().join("a.txt"));
    }

    #[test]
    fn resolve_in_root_allows_a_path_that_does_not_exist_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_in_root(tmp.path(), "new/file.txt").unwrap();
        assert_eq!(resolved, tmp.path().join("new/file.txt"));
    }

    #[test]
    fn resolve_in_root_rejects_dot_dot_before_touching_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_in_root(tmp.path(), "../escape").unwrap_err();
        assert!(matches!(err, DriveError::PathEscape { .. }));
    }

    #[test]
    fn resolve_in_root_rejects_an_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_in_root(tmp.path(), "/etc/passwd").unwrap_err();
        assert!(matches!(err, DriveError::PathEscape { .. }));
    }

    #[test]
    fn resolve_in_root_rejects_a_symlinked_directory_that_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        fs_write(outside.join("secret.txt"), b"shadow");
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let err = resolve_in_root(&root, "escape/secret.txt").unwrap_err();
        assert!(matches!(err, DriveError::PathEscape { .. }));
    }

    #[test]
    fn resolve_in_root_rejects_a_symlink_that_escapes_even_as_the_final_component() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside.txt");
        std::fs::create_dir(&root).unwrap();
        fs_write(&outside, b"shadow");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let err = resolve_in_root(&root, "link").unwrap_err();
        assert!(matches!(err, DriveError::PathEscape { .. }));
    }

    fn fs_write(path: impl AsRef<Path>, data: &[u8]) {
        std::fs::write(path, data).unwrap();
    }
}
