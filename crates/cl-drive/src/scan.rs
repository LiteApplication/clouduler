//! Walking a volume into a manifest, and diffing two manifests.
//!
//! Pure and IO only at the edges: `scan` touches the filesystem, `Manifest`
//! and `diff` do not. Hashing every file on every scan does not scale, so
//! `scan` only hashes when asked; ordinary sync decisions run on size and
//! mtime alone, and content hashing is reserved for rename detection and
//! migration verification.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::error::{DriveError, Result};

/// What's known about one file (or symlink) in a volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub size: u64,
    /// Nanoseconds since the epoch. Second resolution is too coarse: a
    /// rewrite that lands in the same wall-clock second as the original
    /// would otherwise be invisible to a no-hash diff.
    pub mtime_nanos: i64,
    /// Permission bits, masked to the low 12 (rwx + setuid/gid/sticky).
    pub mode: u32,
    pub is_symlink: bool,
    /// Set only for symlinks.
    pub link_target: Option<String>,
    /// Content hash. Only present when the scan was asked to hash.
    pub hash: Option<[u8; 32]>,
}

impl FileMeta {
    /// Whether two entries for the same path represent different content.
    /// Falls back to cheap metadata unless both sides carry a hash, in which
    /// case the hash is authoritative.
    fn differs_from(&self, other: &FileMeta) -> bool {
        if self.is_symlink || other.is_symlink {
            return self.is_symlink != other.is_symlink || self.link_target != other.link_target;
        }
        if let (Some(a), Some(b)) = (self.hash, other.hash) {
            return a != b;
        }
        self.size != other.size || self.mtime_nanos != other.mtime_nanos || self.mode != other.mode
    }
}

/// A volume's contents, keyed by path relative to the volume root, always
/// using `/` separators so it's portable between nodes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub files: BTreeMap<String, FileMeta>,
    /// Directories, path to permission bits.
    ///
    /// Tracked in their own right rather than being implied by the files
    /// inside them, because an *empty* directory is implied by nothing. A
    /// receiver that only ever calls `create_dir_all` on the way to writing a
    /// file ends up with a faithful copy of every file and none of the empty
    /// directories — which is not a faithful copy of the volume. Postgres
    /// keeps a dozen normally-empty directories (`pg_notify`, `pg_replslot`,
    /// `pg_twophase`, ...) and refuses to start without them, so a replica
    /// built purely by sync was unstartable: `could not open directory
    /// "pg_notify"`. That only ever surfaces on failover, which is the worst
    /// possible moment to discover it.
    #[serde(default)]
    pub dirs: BTreeMap<String, u32>,
}

/// The difference needed to turn `other` into `self`. Sorted since the
/// backing map is a `BTreeMap`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diff {
    /// Present in `self` only: needs sending.
    pub added: Vec<String>,
    /// Present in both, content differs: needs sending.
    pub changed: Vec<String>,
    /// Present in `other` only: needs deleting from `other`.
    pub removed: Vec<String>,
    /// Directories to create on `other`, parents first (lexicographic order
    /// puts `a` before `a/b`).
    pub added_dirs: Vec<String>,
    /// Directories `other` has and `self` does not, children first so a tree
    /// can be taken down from the leaves.
    pub removed_dirs: Vec<String>,
}

impl Manifest {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// What's needed to bring `other` up to `self`.
    pub fn diff(&self, other: &Manifest) -> Diff {
        let mut diff = Diff::default();
        for (path, meta) in &self.files {
            match other.files.get(path) {
                None => diff.added.push(path.clone()),
                Some(other_meta) => {
                    if meta.differs_from(other_meta) {
                        diff.changed.push(path.clone());
                    }
                }
            }
        }
        for path in other.files.keys() {
            if !self.files.contains_key(path) {
                diff.removed.push(path.clone());
            }
        }
        for path in self.dirs.keys() {
            if !other.dirs.contains_key(path) {
                diff.added_dirs.push(path.clone());
            }
        }
        for path in other.dirs.keys() {
            if !self.dirs.contains_key(path) {
                diff.removed_dirs.push(path.clone());
            }
        }
        // Children before parents, so a removed tree comes down from the
        // leaves and each `remove_dir` sees an empty directory.
        diff.removed_dirs.reverse();
        diff
    }
}

/// Read size for [`hash_file`]. Big enough that the syscall overhead is
/// irrelevant, small enough to be noise against the process's footprint.
const HASH_CHUNK: usize = 1024 * 1024;

/// blake3 a file's contents without holding the file in memory.
///
/// `std::fs::read` followed by `blake3::hash` needs the entire file resident,
/// and these hashes are taken over arbitrary volume contents — a
/// multi-gigabyte database relation, an ONNX model, a stray core dump. On a
/// node with 8GB that is an OOM kill for a single file. Streaming keeps the
/// cost at one `HASH_CHUNK` buffer regardless of size, for the same result.
pub fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn mtime_nanos(meta: &std::fs::Metadata) -> i64 {
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

/// Converts a `FileMeta`-style nanosecond timestamp to a `filetime::FileTime`,
/// shared by every writer (`transport`, `served::server`) that needs to stamp
/// a file with a timestamp carried over the wire rather than "now".
pub fn nanos_to_filetime(nanos: i64) -> filetime::FileTime {
    filetime::FileTime::from_unix_time(nanos / 1_000_000_000, (nanos % 1_000_000_000) as u32)
}

/// `scan`, moved off the async runtime.
///
/// Every caller of `scan` in this crate is inside an `async fn`, and a scan is
/// not a quick syscall: it stats every file in the volume and, with
/// `hash_content`, reads and blake3 hashes all of it. On a volume the size of
/// a database that is seconds of solid syscalls and CPU on one thread.
///
/// Run inline, that blocks a tokio worker for the whole pass — and this
/// process is also a raft node. openraft's heartbeat deadline is a fraction of
/// `heartbeat_interval` (375ms at the default 500ms), so a sync pass is long
/// enough to make the node miss heartbeats, be marked unreachable by peers
/// that can reach it perfectly well, lose quorum and fence itself. The
/// giveaway is `Unreachable node: deadline has elapsed` on a link that pings
/// with 0% loss.
pub async fn scan_off_runtime(root: PathBuf, hash_content: bool) -> Result<Manifest> {
    let name = root.display().to_string();
    tokio::task::spawn_blocking(move || scan(&root, hash_content))
        .await
        .map_err(|e| DriveError::Transfer {
            message: format!("scan of {name} did not run: {e}"),
        })?
}

/// Walk `root` into a manifest. Symlinks are recorded, not followed. When
/// `hash_content` is set every regular file is read and blake3 hashed, which
/// is only worth paying for during verification or rename detection.
///
/// Blocking. Inside an `async fn` use `scan_off_runtime`.
pub fn scan(root: &Path, hash_content: bool) -> Result<Manifest> {
    let mut files = BTreeMap::new();
    let mut dirs = BTreeMap::new();

    if !root.exists() {
        return Ok(Manifest::default());
    }

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|e| DriveError::Io {
            path: e.path().unwrap_or(root).to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        let path = entry.path();
        if path == root {
            continue;
        }

        let file_type = entry.file_type();

        let rel = path
            .strip_prefix(root)
            .map_err(|_| DriveError::PathEscape { path: path.display().to_string() })?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");

        let meta = std::fs::symlink_metadata(path).map_err(|source| DriveError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        if file_type.is_dir() {
            dirs.insert(rel, meta.mode() & 0o7777);
            continue;
        }

        if file_type.is_symlink() {
            let target = std::fs::read_link(path)
                .map_err(|source| DriveError::Io { path: path.to_path_buf(), source })?;
            files.insert(
                rel,
                FileMeta {
                    size: meta.size(),
                    mtime_nanos: mtime_nanos(&meta),
                    mode: meta.mode() & 0o7777,
                    is_symlink: true,
                    link_target: Some(target.to_string_lossy().to_string()),
                    hash: None,
                },
            );
            continue;
        }

        if !file_type.is_file() {
            // Sockets, fifos and friends: not something a bind-mounted volume
            // should carry, and not something we know how to replicate.
            continue;
        }

        let hash = if hash_content {
            Some(hash_file(path).map_err(|source| DriveError::Io {
                path: path.to_path_buf(),
                source,
            })?)
        } else {
            None
        };

        files.insert(
            rel,
            FileMeta {
                size: meta.size(),
                mtime_nanos: mtime_nanos(&meta),
                mode: meta.mode() & 0o7777,
                is_symlink: false,
                link_target: None,
                hash,
            },
        );
    }

    Ok(Manifest { files, dirs })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn meta(size: u64, mtime: i64) -> FileMeta {
        FileMeta {
            size,
            mtime_nanos: mtime,
            mode: 0o644,
            is_symlink: false,
            link_target: None,
            hash: None,
        }
    }

    #[test]
    fn scan_finds_files_and_records_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/b.txt"), b"world").unwrap();

        let manifest = scan(tmp.path(), false).unwrap();
        assert_eq!(manifest.files.len(), 2);
        assert!(manifest.files.contains_key("a.txt"));
        assert!(manifest.files.contains_key("sub/b.txt"));
        assert_eq!(manifest.files["a.txt"].size, 5);
        assert!(manifest.files["a.txt"].hash.is_none());
    }

    #[test]
    fn scan_with_hashing_fills_in_the_hash() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let manifest = scan(tmp.path(), true).unwrap();
        assert_eq!(manifest.files["a.txt"].hash, Some(*blake3::hash(b"hello").as_bytes()));
    }

    #[test]
    fn scan_of_missing_root_is_an_empty_manifest() {
        let manifest = scan(Path::new("/does/not/exist"), false).unwrap();
        assert!(manifest.is_empty());
    }

    #[test]
    fn scan_records_symlinks_without_following_them() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("real.txt"), b"data").unwrap();
        std::os::unix::fs::symlink("real.txt", tmp.path().join("link")).unwrap();

        let manifest = scan(tmp.path(), false).unwrap();
        let link = &manifest.files["link"];
        assert!(link.is_symlink);
        assert_eq!(link.link_target.as_deref(), Some("real.txt"));
    }

    #[test]
    fn diff_finds_added_changed_and_removed() {
        let mut old = Manifest::default();
        old.files.insert("keep.txt".into(), meta(10, 100));
        old.files.insert("gone.txt".into(), meta(5, 100));
        old.files.insert("edit.txt".into(), meta(20, 100));

        let mut new = Manifest::default();
        new.files.insert("keep.txt".into(), meta(10, 100));
        new.files.insert("edit.txt".into(), meta(21, 200));
        new.files.insert("fresh.txt".into(), meta(1, 300));

        let diff = new.diff(&old);
        assert_eq!(diff.added, vec!["fresh.txt".to_string()]);
        assert_eq!(diff.changed, vec!["edit.txt".to_string()]);
        assert_eq!(diff.removed, vec!["gone.txt".to_string()]);
    }

    #[test]
    fn diff_is_empty_for_identical_manifests() {
        let mut m = Manifest::default();
        m.files.insert("a.txt".into(), meta(1, 1));
        let diff = m.diff(&m.clone());
        assert!(diff.added.is_empty() && diff.changed.is_empty() && diff.removed.is_empty());
    }

    #[test]
    fn diff_trusts_hash_over_stale_metadata_when_both_present() {
        let mut old = Manifest::default();
        let mut new = Manifest::default();
        let mut a = meta(10, 100);
        a.hash = Some([1u8; 32]);
        let mut b = meta(10, 100);
        b.hash = Some([2u8; 32]);
        old.files.insert("f".into(), a);
        new.files.insert("f".into(), b);

        // Same size and mtime, but hashes disagree: still a change.
        let diff = new.diff(&old);
        assert_eq!(diff.changed, vec!["f".to_string()]);
    }

    #[test]
    fn diff_mode_change_alone_counts_as_changed() {
        let mut old = Manifest::default();
        let mut new = Manifest::default();
        old.files.insert("f".into(), meta(10, 100));
        let mut changed_mode = meta(10, 100);
        changed_mode.mode = 0o600;
        new.files.insert("f".into(), changed_mode);

        let diff = new.diff(&old);
        assert_eq!(diff.changed, vec!["f".to_string()]);
    }
}
