//! Which drives this node implements, and safe path resolution within them.
//!
//! A volume name comes from a stack file, so it must never be trusted enough
//! to let a mount escape its drive root. Validation is purely component-wise
//! (reject `..`, empty and absolute components) rather than canonicalizing,
//! since the target path may not exist yet.

use std::collections::{BTreeMap, BTreeSet};
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
    /// A served drive, and this is the currently active server: the first
    /// node in `served_by` that is reachable.
    Serve,
    /// A served drive, and this node is one of the other candidates in
    /// `served_by`: it keeps a synced backup copy but isn't answering
    /// requests right now, and takes over if the active server drops out.
    Standby,
    /// A served drive, held by another node; this node mounts it over FUSE.
    Mount,
}

/// Drives one node implements, mapped to their local backing directories.
pub struct DriveRegistry {
    node: String,
    drives: BTreeMap<String, PathBuf>,
    /// Candidate serving nodes for each drive in `served` mode, in priority
    /// order. A drive absent here is replicated. Present so `role` can tell
    /// serving from mounting without every caller having to carry the
    /// cluster config around.
    served_by: BTreeMap<String, Vec<String>>,
}

impl DriveRegistry {
    /// Build from the cluster config and this node's name.
    ///
    /// A served drive is placeable on any node, not just the ones that
    /// declare it (`cl_sched::place::rejection`'s carve-out for `served_drives`,
    /// and `ClusterState::check_placement`'s matching one) - a node with no
    /// static entry mounts the drive over the network instead of holding a
    /// copy. Without a synthetic entry here, that node's registry disagreed
    /// with the placement checks that had already let an instance land on it:
    /// `implements`/`root` said `NotImplemented` for a drive placement had
    /// just approved, which is exactly what turned a legitimate served-drive
    /// migration into a `does not implement drive` failure. The synthetic
    /// path mirrors `cl_sched::run::Scheduler::drive_roots`'s own convention
    /// for the same case (`data_dir/volume-mounts/<name>`) so the two agree.
    pub fn new(config: &ClusterConfig, node: &str) -> Result<Self> {
        let entry = config
            .node(node)
            .ok_or_else(|| DriveError::Config(format!("unknown node {node}")))?;
        let served_by = config
            .drives
            .iter()
            .filter(|(_, drive)| drive.is_served())
            .map(|(name, drive)| (name.clone(), drive.served_by.clone()))
            .collect();
        let mut drives = entry.drives.clone();
        let data_dir = entry.data_dir(config);
        for (name, drive) in &config.drives {
            if drive.is_served() && !drives.contains_key(name) {
                drives.insert(name.clone(), data_dir.join("volume-mounts").join(name));
            }
        }
        Ok(Self {
            node: node.to_string(),
            drives,
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

    /// Mark drives as served, mapping each to its candidate serving nodes in
    /// priority order. For tests and standalone tools that build a registry
    /// by hand.
    pub fn with_served_by(mut self, served_by: BTreeMap<String, Vec<String>>) -> Self {
        self.served_by = served_by;
        self
    }

    /// The candidate serving nodes for a served drive, in priority order. An
    /// empty slice for a drive this registry doesn't know as served.
    pub fn served_candidates(&self, drive: &str) -> &[String] {
        self.served_by.get(drive).map_or(&[], Vec::as_slice)
    }

    /// The node currently serving a drive: the first candidate not in
    /// `unreachable`. `None` if every candidate is unreachable, or this
    /// isn't a served drive.
    pub fn active_server<'a>(&'a self, drive: &str, unreachable: &BTreeSet<String>) -> Option<&'a str> {
        self.served_candidates(drive).iter().map(String::as_str).find(|n| !unreachable.contains(*n))
    }

    /// Whether this node serves, mounts, backs up, or replicates a drive.
    /// Unknown drives (this node doesn't implement it) come back as
    /// `Replicate`, same as the default for an ordinary drive; callers that
    /// care check `implements` first.
    pub fn role(&self, drive: &str, unreachable: &BTreeSet<String>) -> DriveRole {
        let candidates = self.served_candidates(drive);
        if candidates.is_empty() {
            return DriveRole::Replicate;
        }
        if !candidates.iter().any(|n| n == &self.node) {
            return DriveRole::Mount;
        }
        match self.active_server(drive, unreachable) {
            Some(active) if active == self.node => DriveRole::Serve,
            _ => DriveRole::Standby,
        }
    }

    /// Whether a drive is served (either by this node or another), as
    /// opposed to replicated.
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

    /// Where a `NodeLocal` backup destination stages `drive` when this node
    /// does not implement it: a hidden directory next to the destination's
    /// own folder (`destination_path`, that destination's configured path),
    /// not under this node's data dir - so the staged copy, which can be the
    /// whole drive, lands on whatever volume the destination itself lives
    /// on rather than filling up the root filesystem. Always available,
    /// unlike `root`: staging a drive this node doesn't hold is exactly the
    /// case this path exists for.
    pub fn staging_root_for(&self, destination_path: &Path, drive: &str) -> PathBuf {
        let hidden = destination_path
            .file_name()
            .map(|name| format!(".{}-staging", name.to_string_lossy()))
            .unwrap_or_else(|| ".backup-staging".to_string());
        destination_path.with_file_name(hidden).join(drive)
    }

    /// `staging_root_for`, created if missing - the destination side of a
    /// `NodeLocal`-elsewhere backup needs somewhere for restic to point at
    /// even before any volume has landed there yet.
    pub fn ensure_staging_root(&self, destination_path: &Path, drive: &str) -> Result<PathBuf> {
        let path = self.staging_root_for(destination_path, drive);
        std::fs::create_dir_all(&path).map_err(|source| DriveError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// The staging path for one volume of a drive this node does not
    /// implement, creating its directory if missing - the receive side of a
    /// `NodeLocal`-elsewhere backup push (see `transport::receive`'s
    /// `staging` handling). Mirrors `ensure_volume`, rooted next to the
    /// destination instead of under `drives`.
    pub fn ensure_staging_volume(&self, destination_path: &Path, id: &VolumeId) -> Result<PathBuf> {
        let root = self.staging_root_for(destination_path, &id.drive);
        let path = resolve_relative(&root, &id.volume)?;
        std::fs::create_dir_all(&path).map_err(|source| DriveError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }
}

/// Join `rel` onto `root`, rejecting anything that could climb out: empty
/// components, `.`/`..`, and leading slashes (which show up as an empty
/// first component once split). `rel == "."` is the one exception: it names
/// the drive's own root directory (a whole-drive mount), not a subpath of
/// it, so it resolves to `root` itself rather than being rejected as a
/// no-op path component.
pub fn resolve_relative(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Err(DriveError::PathEscape { path: rel.to_string() });
    }
    if rel == "." {
        return Ok(root.to_path_buf());
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
    fn staging_root_is_available_for_a_drive_this_node_does_not_implement() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new());
        assert!(!reg.implements("cold"));

        let destination_path = tmp.path().join("backups");
        let id = VolumeId::new("cold", "data");
        let path = reg.ensure_staging_volume(&destination_path, &id).unwrap();
        assert!(path.is_dir());
        assert_eq!(path, tmp.path().join(".backups-staging").join("cold").join("data"));
    }

    #[test]
    fn ensure_staging_root_creates_the_drive_directory_with_nothing_pushed_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new());
        let destination_path = tmp.path().join("backups");
        let path = reg.ensure_staging_root(&destination_path, "cold").unwrap();
        assert!(path.is_dir());
        assert_eq!(path, tmp.path().join(".backups-staging").join("cold"));
    }

    #[test]
    fn staging_root_sits_next_to_the_destination_folder_not_under_data_dir() {
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new());
        let destination_path = Path::new("/srv/backups/mydest");
        let path = reg.staging_root_for(destination_path, "cold");
        assert_eq!(path, PathBuf::from("/srv/backups/.mydest-staging/cold"));
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
        assert_eq!(reg.role("fast", &BTreeSet::new()), DriveRole::Replicate);
        assert!(!reg.is_served("fast"));
    }

    #[test]
    fn role_is_serve_on_the_serving_node_and_mount_elsewhere() {
        let serving = DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/srv/fast"))]),
        )
        .with_served_by(BTreeMap::from([("fast".to_string(), vec!["n1".to_string()])]));
        assert_eq!(serving.role("fast", &BTreeSet::new()), DriveRole::Serve);
        assert!(serving.is_served("fast"));

        let mounting = DriveRegistry::from_drives(
            "n2",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/mnt/fast"))]),
        )
        .with_served_by(BTreeMap::from([("fast".to_string(), vec!["n1".to_string()])]));
        assert_eq!(mounting.role("fast", &BTreeSet::new()), DriveRole::Mount);
        assert!(mounting.is_served("fast"));
    }

    #[test]
    fn standby_takes_over_when_the_active_server_is_unreachable() {
        let served_by = BTreeMap::from([("fast".to_string(), vec!["n1".to_string(), "n2".to_string()])]);

        let primary = DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/srv/fast"))]),
        )
        .with_served_by(served_by.clone());
        let backup = DriveRegistry::from_drives(
            "n2",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/srv/fast"))]),
        )
        .with_served_by(served_by);

        assert_eq!(primary.role("fast", &BTreeSet::new()), DriveRole::Serve);
        assert_eq!(backup.role("fast", &BTreeSet::new()), DriveRole::Standby);
        assert_eq!(primary.active_server("fast", &BTreeSet::new()), Some("n1"));

        let n1_down = BTreeSet::from(["n1".to_string()]);
        assert_eq!(primary.role("fast", &n1_down), DriveRole::Standby);
        assert_eq!(backup.role("fast", &n1_down), DriveRole::Serve);
        assert_eq!(primary.active_server("fast", &n1_down), Some("n2"));
    }

    #[test]
    fn active_server_is_none_when_every_candidate_is_unreachable() {
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new())
            .with_served_by(BTreeMap::from([("fast".to_string(), vec!["n1".to_string(), "n2".to_string()])]));
        let all_down = BTreeSet::from(["n1".to_string(), "n2".to_string()]);
        assert_eq!(reg.active_server("fast", &all_down), None);
        assert_eq!(reg.role("fast", &all_down), DriveRole::Standby);
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
    fn resolve_relative_of_bare_dot_is_the_drive_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_relative(tmp.path(), ".").unwrap(), tmp.path());
    }

    #[test]
    fn resolve_in_root_of_bare_dot_is_the_drive_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_in_root(tmp.path(), ".").unwrap(), tmp.path());
    }

    #[test]
    fn resolve_relative_still_rejects_a_dot_component_inside_a_longer_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_relative(tmp.path(), "a/./b").unwrap_err();
        assert!(matches!(err, DriveError::PathEscape { .. }));
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
