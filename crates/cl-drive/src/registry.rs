//! Which drives this node implements, and safe path resolution within them.
//!
//! Volume names come from stack files, so validation is purely component-wise
//! (reject `..`, empty and absolute components) rather than canonicalizing,
//! since the target path may not exist yet.

use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cl_config::ClusterConfig;
use cl_raft::VolumeId;

use crate::error::{DriveError, Result};

/// How this node relates to a drive: serves it, mounts it, or keeps a replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveRole {
    /// A replicated drive: this node's copy is kept current by delta sync.
    Replicate,
    /// Served drive, and this node is the active server (holds the authoritative copy).
    Serve,
    /// Served drive, and this node is another `served_by` candidate: keeps a synced
    /// backup and takes over if the active server drops out.
    Standby,
    /// Served drive held by another node; this node mounts it over NFS.
    Mount,
}

/// Drives one node implements, mapped to their local backing directories.
pub struct DriveRegistry {
    node: String,
    drives: BTreeMap<String, PathBuf>,
    /// Candidate serving nodes per served drive, in priority order. A drive
    /// absent here is replicated.
    served_by: BTreeMap<String, Vec<String>>,
    /// `NodeLocal` backup destination paths on this node: the only places a peer
    /// may ask it to stage a volume. Behind a lock because they live in raft
    /// state and the daemon refreshes them; empty until then, which refuses every
    /// staging push.
    staging_destinations: RwLock<Vec<PathBuf>>,
    /// Per-volume sync records (see `record_synced`). `None` records nothing.
    state_dir: Option<PathBuf>,
}

impl DriveRegistry {
    /// Build from the cluster config and this node's name.
    ///
    /// A served drive is placeable on any node (see
    /// `cl_sched::place::rejection` and `ClusterState::check_placement`), so a
    /// node with no static entry gets a synthetic one. Without it, `implements`/
    /// `root` said `NotImplemented` for a drive placement had approved, failing
    /// served-drive migrations. The path mirrors
    /// `cl_sched::run::Scheduler::drive_roots` (`data_dir/volume-mounts/<name>`).
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
            staging_destinations: RwLock::default(),
            state_dir: Some(data_dir.join("served-sync")),
        })
    }

    /// Build from a name to path map, for tests and tools. All drives replicated.
    pub fn from_drives(node: impl Into<String>, drives: BTreeMap<String, PathBuf>) -> Self {
        Self {
            node: node.into(),
            drives,
            served_by: BTreeMap::new(),
            staging_destinations: RwLock::default(),
            state_dir: None,
        }
    }

    /// Keep sync records under `dir`. For tests and tools.
    pub fn with_state_dir(mut self, dir: PathBuf) -> Self {
        self.state_dir = Some(dir);
        self
    }

    fn record_path(&self, id: &VolumeId) -> Option<PathBuf> {
        let dir = self.state_dir.as_ref()?;
        let key = blake3::hash(id.to_string().as_bytes()).to_hex();
        Some(dir.join(format!("{}-{}", id.drive, &key.as_str()[..16])))
    }

    /// Remember that this copy held exactly the tree `digest` (see
    /// `Manifest::digest`) when it last agreed with its peer; a differing tree
    /// has been written since.
    pub fn record_synced(&self, id: &VolumeId, digest: &str) {
        let Some(path) = self.record_path(id) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, digest).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    pub fn recorded_sync(&self, id: &VolumeId) -> Option<String> {
        std::fs::read_to_string(self.record_path(id)?)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Mark drives as served, with candidate serving nodes in priority order.
    /// For tests and tools.
    pub fn with_served_by(mut self, served_by: BTreeMap<String, Vec<String>>) -> Self {
        self.served_by = served_by;
        self
    }

    /// Replace the allowed backup staging destinations.
    pub fn set_staging_destinations(&self, paths: Vec<PathBuf>) {
        *self.staging_destinations.write() = paths;
    }

    /// Accept a peer-supplied staging path only if it is exactly a configured
    /// backup destination. The path arrives in `Hello` and picks a directory to
    /// write into, so an unchecked one would let any peer write anywhere.
    /// Non-normalized or relative paths are refused before the allowlist check.
    pub fn check_staging_destination(&self, destination_path: &Path) -> Result<()> {
        // Raw string check: `Path::components` drops interior `.` and trailing slashes.
        let raw = destination_path.to_string_lossy();
        let normalized = destination_path.is_absolute()
            && !raw.contains('\0')
            && raw
                .trim_end_matches('/')
                .split('/')
                .skip(1)
                .all(|part| !part.is_empty() && part != "." && part != "..");
        let allowed = normalized
            && self
                .staging_destinations
                .read()
                .iter()
                .any(|p| p == destination_path);
        if allowed {
            Ok(())
        } else {
            Err(DriveError::Transfer {
                message: format!(
                    "staging path {} is not a backup destination configured for node {}",
                    destination_path.display(),
                    self.node
                ),
            })
        }
    }

    /// Candidate serving nodes in priority order; empty for non-served drives.
    pub fn served_candidates(&self, drive: &str) -> &[String] {
        self.served_by.get(drive).map_or(&[], Vec::as_slice)
    }

    /// The active server: the raft-recorded holder while reachable, else the first
    /// reachable candidate (`cl_raft::served::choose_active_server`). `None` if
    /// none reachable, a handover is in progress, or the drive isn't served.
    pub fn active_server<'a>(
        &'a self,
        drive: &str,
        state: &cl_raft::ClusterState,
    ) -> Option<&'a str> {
        cl_raft::served::choose_active_server(
            self.served_candidates(drive),
            &state.unreachable,
            state.served.get(drive),
        )
    }

    /// Serve, mount, back up, or replicate. Unknown drives come back as
    /// `Replicate`; check `implements` first.
    pub fn role(&self, drive: &str, state: &cl_raft::ClusterState) -> DriveRole {
        let candidates = self.served_candidates(drive);
        if candidates.is_empty() {
            return DriveRole::Replicate;
        }
        if !candidates.iter().any(|n| n == &self.node) {
            return DriveRole::Mount;
        }
        match self.active_server(drive, state) {
            Some(active) if active == self.node => DriveRole::Serve,
            _ => DriveRole::Standby,
        }
    }

    /// Whether the drive is served (by anyone) rather than replicated.
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
        self.drives
            .iter()
            .map(|(name, path)| (name.as_str(), path.as_path()))
    }

    pub fn root(&self, drive: &str) -> Result<&Path> {
        self.drives
            .get(drive)
            .map(PathBuf::as_path)
            .ok_or_else(|| DriveError::NotImplemented {
                drive: drive.to_string(),
                node: self.node.clone(),
            })
    }

    pub fn volume_path(&self, id: &VolumeId) -> Result<PathBuf> {
        let root = self.root(&id.drive)?;
        resolve_relative(root, &id.volume)
    }

    pub fn ensure_volume(&self, id: &VolumeId) -> Result<PathBuf> {
        let path = self.volume_path(id)?;
        std::fs::create_dir_all(&path).map_err(|source| DriveError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Where a `NodeLocal` backup destination stages `drive` when this node does
    /// not implement it: a hidden directory next to the destination's folder
    /// (`destination_path`), so a whole-drive copy lands on the destination's
    /// volume rather than filling the root filesystem.
    pub fn staging_root_for(&self, destination_path: &Path, drive: &str) -> PathBuf {
        let hidden = destination_path
            .file_name()
            .map(|name| format!(".{}-staging", name.to_string_lossy()))
            .unwrap_or_else(|| ".backup-staging".to_string());
        destination_path.with_file_name(hidden).join(drive)
    }

    /// `staging_root_for`, created if missing (restic needs a target before any
    /// volume lands).
    pub fn ensure_staging_root(&self, destination_path: &Path, drive: &str) -> Result<PathBuf> {
        let path = self.staging_root_for(destination_path, drive);
        std::fs::create_dir_all(&path).map_err(|source| DriveError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Staging path for one volume of a drive this node does not implement,
    /// created if missing (see `transport::receive`'s `staging` handling).
    pub fn ensure_staging_volume(&self, destination_path: &Path, id: &VolumeId) -> Result<PathBuf> {
        self.check_staging_destination(destination_path)?;
        let root = self.staging_root_for(destination_path, &id.drive);
        let path = resolve_relative(&root, &id.volume)?;
        std::fs::create_dir_all(&path).map_err(|source| DriveError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }
}

/// Join `rel` onto `root`, rejecting empty components, `.`/`..` and leading
/// slashes. `rel == "."` is the exception: it names the drive root itself
/// (whole-drive mount).
pub fn resolve_relative(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.is_empty() {
        return Err(DriveError::PathEscape {
            path: rel.to_string(),
        });
    }
    if rel == "." {
        return Ok(root.to_path_buf());
    }
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\0') {
            return Err(DriveError::PathEscape {
                path: rel.to_string(),
            });
        }
        path.push(part);
    }
    Ok(path)
}

/// Like `resolve_relative`, but also refuses to pass through a symlink on the
/// way to the final component.
///
/// Replication carries symlinks with arbitrary targets and containers can plant
/// them, so `a/etc/cron.d/x` must not walk through a link `a` pointing at `/`.
/// Every existing ancestor below `root` must be a real directory (non-dirs fail
/// later with ENOTDIR); the first missing one ends the walk. The final
/// component is not checked: it may be a link being replaced or removed, and
/// callers writing *through* it must handle that. `root` itself is trusted (it
/// may be a symlink to another disk).
pub fn resolve_no_follow(root: &Path, rel: &str) -> Result<PathBuf> {
    let resolved = resolve_relative(root, rel)?;
    let Ok(below_root) = resolved.strip_prefix(root) else {
        return Ok(resolved);
    };
    let parts: Vec<_> = below_root.components().collect();
    let mut walked = root.to_path_buf();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        walked.push(part);
        match std::fs::symlink_metadata(&walked) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(DriveError::PathEscape {
                    path: rel.to_string(),
                })
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(DriveError::Io {
                    path: walked,
                    source,
                })
            }
        }
    }
    Ok(resolved)
}

/// Like `resolve_relative`, then guards against a symlink inside the drive
/// pointing out of it. Component checks can't see that for paths that exist
/// (served drives), so this canonicalizes the deepest existing ancestor and
/// checks it is still under the root, allowing a non-existent final component
/// (`create`, `mkdir`, rename target).
pub fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let candidate = resolve_relative(root, rel)?;

    let canonical_root = root.canonicalize().map_err(|source| DriveError::Io {
        path: root.to_path_buf(),
        source,
    })?;

    let mut existing = candidate.as_path();
    while !existing.exists() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => break,
        }
    }
    let canonical_existing = existing.canonicalize().map_err(|source| DriveError::Io {
        path: existing.to_path_buf(),
        source,
    })?;

    if !canonical_existing.starts_with(&canonical_root) {
        return Err(DriveError::PathEscape {
            path: rel.to_string(),
        });
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::ClusterState;

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
        assert_eq!(
            reg.volume_path(&id).unwrap(),
            PathBuf::from("/srv/fast/gitlab-data")
        );
    }

    #[test]
    fn nested_volume_names_are_allowed() {
        let reg = registry();
        let id = VolumeId::new("fast", "a/b");
        assert_eq!(
            reg.volume_path(&id).unwrap(),
            PathBuf::from("/srv/fast/a/b")
        );
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
                assert!(
                    matches!(result, Err(DriveError::PathEscape { .. })),
                    "{name} should be rejected"
                );
            }
        }
    }

    #[test]
    fn absolute_and_empty_components_are_rejected() {
        let reg = registry();
        for name in ["/etc/passwd", "a//b", ""] {
            let id = VolumeId::new("fast", name);
            assert!(
                matches!(reg.volume_path(&id), Err(DriveError::PathEscape { .. })),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn staging_root_is_available_for_a_drive_this_node_does_not_implement() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new());
        assert!(!reg.implements("cold"));

        let destination_path = tmp.path().join("backups");
        reg.set_staging_destinations(vec![destination_path.clone()]);
        let id = VolumeId::new("cold", "data");
        let path = reg.ensure_staging_volume(&destination_path, &id).unwrap();
        assert!(path.is_dir());
        assert_eq!(
            path,
            tmp.path()
                .join(".backups-staging")
                .join("cold")
                .join("data")
        );
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

    fn unreachable(nodes: &[&str]) -> ClusterState {
        ClusterState {
            unreachable: nodes.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_recorded_holder_stays_active_when_the_first_candidate_returns() {
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new()).with_served_by(BTreeMap::from(
            [("fast".to_string(), vec!["n1".to_string(), "n2".to_string()])],
        ));
        let mut state = ClusterState::default();
        assert_eq!(reg.active_server("fast", &state), Some("n1"));
        state.served.insert(
            "fast".into(),
            cl_raft::served::ServedAuthority {
                holder: "n2".into(),
                epoch: 2,
                ..Default::default()
            },
        );
        assert_eq!(reg.active_server("fast", &state), Some("n2"));
        assert_eq!(reg.role("fast", &state), DriveRole::Standby);
    }

    #[test]
    fn role_is_replicate_for_a_drive_with_no_serving_node() {
        let reg = registry();
        assert_eq!(
            reg.role("fast", &ClusterState::default()),
            DriveRole::Replicate
        );
        assert!(!reg.is_served("fast"));
    }

    #[test]
    fn role_is_serve_on_the_serving_node_and_mount_elsewhere() {
        let serving = DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/srv/fast"))]),
        )
        .with_served_by(BTreeMap::from([(
            "fast".to_string(),
            vec!["n1".to_string()],
        )]));
        assert_eq!(
            serving.role("fast", &ClusterState::default()),
            DriveRole::Serve
        );
        assert!(serving.is_served("fast"));

        let mounting = DriveRegistry::from_drives(
            "n2",
            BTreeMap::from([("fast".to_string(), PathBuf::from("/mnt/fast"))]),
        )
        .with_served_by(BTreeMap::from([(
            "fast".to_string(),
            vec!["n1".to_string()],
        )]));
        assert_eq!(
            mounting.role("fast", &ClusterState::default()),
            DriveRole::Mount
        );
        assert!(mounting.is_served("fast"));
    }

    #[test]
    fn standby_takes_over_when_the_active_server_is_unreachable() {
        let served_by =
            BTreeMap::from([("fast".to_string(), vec!["n1".to_string(), "n2".to_string()])]);

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

        assert_eq!(
            primary.role("fast", &ClusterState::default()),
            DriveRole::Serve
        );
        assert_eq!(
            backup.role("fast", &ClusterState::default()),
            DriveRole::Standby
        );
        assert_eq!(
            primary.active_server("fast", &ClusterState::default()),
            Some("n1")
        );

        let n1_down = unreachable(&["n1"]);
        assert_eq!(primary.role("fast", &n1_down), DriveRole::Standby);
        assert_eq!(backup.role("fast", &n1_down), DriveRole::Serve);
        assert_eq!(primary.active_server("fast", &n1_down), Some("n2"));
    }

    #[test]
    fn active_server_is_none_when_every_candidate_is_unreachable() {
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new()).with_served_by(BTreeMap::from(
            [("fast".to_string(), vec!["n1".to_string(), "n2".to_string()])],
        ));
        let all_down = unreachable(&["n1", "n2"]);
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

    #[test]
    fn staging_is_refused_unless_the_path_is_a_configured_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = DriveRegistry::from_drives("n1", BTreeMap::new());
        let id = VolumeId::new("cold", "data");
        let dest = tmp.path().join("backups");

        // Nothing configured yet: everything is refused.
        assert!(reg.ensure_staging_volume(&dest, &id).is_err());

        reg.set_staging_destinations(vec![dest.clone()]);
        assert!(reg.ensure_staging_volume(&dest, &id).is_ok());

        // Not on the list, relative, or spelled with `..`/`.`.
        for bad in [
            PathBuf::from("/etc/cron.d"),
            PathBuf::from("backups"),
            tmp.path().join("other"),
            tmp.path().join("backups/../backups"),
            tmp.path().join("./backups"),
        ] {
            let err = reg.ensure_staging_volume(&bad, &id).unwrap_err();
            assert!(
                matches!(err, DriveError::Transfer { .. }),
                "{} should be refused",
                bad.display()
            );
        }
        assert!(!tmp.path().join("other").exists());
    }

    #[test]
    fn resolve_no_follow_rejects_a_symlinked_intermediate_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("real/deeper")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("real/deeper/b")).unwrap();

        for rel in ["a/x", "a/y/z", "real/deeper/b/x"] {
            assert!(
                matches!(
                    resolve_no_follow(&root, rel),
                    Err(DriveError::PathEscape { .. })
                ),
                "{rel} should be rejected"
            );
        }
        // The final component may be a link: callers replace or remove it.
        assert!(resolve_no_follow(&root, "a").is_ok());
        assert!(resolve_no_follow(&root, "real/deeper/b").is_ok());
        // Missing ancestors are fine; nothing there can be a link.
        assert!(resolve_no_follow(&root, "new/dir/file").is_ok());
    }

    fn fs_write(path: impl AsRef<Path>, data: &[u8]) {
        std::fs::write(path, data).unwrap();
    }
}
