//! The transfer engine: decides what to send for a volume and drives the
//! sender side of the `transport` protocol.
//!
//! `DeltaEngine` is the default: rolling-hash delta sync over `fast_rsync`,
//! our own directory walk, rename/delete handling and resumable (temp file +
//! rename) receiver writes.
//!
//! `BtrfsEngine` and `ZfsEngine` are optional fast paths for matching
//! filesystems (detected via `statfs`). No maintained crate exists, so they
//! would shell out to `btrfs`/`zfs`. Same `SyncEngine` trait, so callers can
//! fall back to `DeltaEngine`.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;

use fast_rsync::{diff as rsync_diff, Signature};

use cl_raft::VolumeId;

use crate::error::{DriveError, Result};
use crate::scan::{self, FileMeta};
use crate::transport::{self, Message};

/// Below this size, sending the whole file beats a signature round trip.
/// Brand new files are always sent whole.
const WHOLE_FILE_THRESHOLD: u64 = 8 * 1024;

/// Above this size a changed file is streamed whole instead of diffed.
///
/// The delta path is O(filesize) resident memory on both ends: the receiver's
/// `Message::Delta` handler holds the base and the output at once
/// (`fast_rsync::apply` writes into a `Vec`), so `2 x filesize` plus the delta,
/// and the sender's `send_delta` holds another copy. `transport::serve` accepts
/// peers concurrently, so this multiplies. On an 8GB node a multi-gigabyte
/// file is an OOM kill, which looks from the sender like a bare "Connection
/// reset by peer".
///
/// Past this size the `WholeFileBegin`/`Chunk`/`End` path streams in
/// `WHOLE_FILE_CHUNK` pieces with bounded memory, at the cost of resending big
/// changed files in full.
///
/// Capped at 8 MiB so a delta frame (at most about the file's size) always fits
/// under `transport::MAX_FRAME`.
const MAX_DELTA_FILE: u64 = 8 * 1024 * 1024;

/// Against a destination holding at least `DROP_CHECK_MIN_FILES` files, a
/// source with under 1/`DROP_FRACTION` of that is lost data, not a clean-up.
const DROP_CHECK_MIN_FILES: usize = 200;
const DROP_FRACTION: usize = 20;

/// Refuses a sync that would delete a destination's data because the source
/// lost its own (empty or nearly empty tree over a populated one). A volume
/// deliberately emptied needs one file left, or its replicas cleared by hand;
/// a replicated wipe is not recoverable.
pub(crate) fn guard_against_data_loss(
    volume: &VolumeId,
    local: &scan::Manifest,
    remote: &scan::Manifest,
) -> Result<()> {
    let (have, held) = (local.files.len(), remote.files.len());
    let reason = if have == 0 && held > 0 {
        format!("the source holds no files but the destination holds {held}")
    } else if held >= DROP_CHECK_MIN_FILES && have * DROP_FRACTION < held {
        format!("the source holds {have} files but the destination holds {held}")
    } else {
        return Ok(());
    };
    Err(DriveError::DestructiveSync {
        volume: volume.clone(),
        reason: format!(
            "{reason}; the source copy looks lost, so the destination copy was kept. \
             Restore the source, or clear the destination's copy if the loss is intended"
        ),
    })
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SyncStats {
    pub files_added: u64,
    pub files_changed: u64,
    pub files_removed: u64,
    pub files_renamed: u64,
    pub bytes_sent: u64,
    pub engine: String,
}

/// Replicates one volume to a peer's drive transport listener, as sender;
/// `transport::receive` is the one receiver for every engine.
pub trait SyncEngine: Send + Sync {
    fn name(&self) -> &str;

    /// `staging`, when set, marks a `NodeLocal`-elsewhere backup's one-off push and
    /// carries the destination's configured path (see `Message::Hello` in
    /// `transport.rs`). Every other caller passes `None`.
    #[allow(clippy::too_many_arguments)]
    fn sync<'a>(
        &'a self,
        src: &'a Path,
        peer: SocketAddr,
        volume: &'a VolumeId,
        generation: u64,
        staging: Option<String>,
        progress: crate::progress::ProgressSink,
    ) -> BoxFuture<'a, Result<SyncStats>>;

    /// Whether `peer`'s copy of a served volume was written since it last agreed
    /// with its source. Engines that cannot ask refuse, so a served volume is
    /// never pushed over blind.
    fn copy_state<'a>(
        &'a self,
        _peer: SocketAddr,
        _volume: &'a VolumeId,
    ) -> BoxFuture<'a, Result<crate::transport::CopyState>> {
        Box::pin(async {
            Err(DriveError::Transfer {
                message: "this sync engine cannot report a peer's copy state".into(),
            })
        })
    }
}

#[derive(Debug, Clone)]
pub struct DeltaEngine {
    whole_file_threshold: u64,
    /// The cluster key, as a watch so a long-lived scheduler follows rotation.
    keys: cl_raft::KeyWatch,
}

impl DeltaEngine {
    pub fn new(keys: cl_raft::KeyWatch) -> Self {
        Self {
            whole_file_threshold: WHOLE_FILE_THRESHOLD,
            keys,
        }
    }
}

impl SyncEngine for DeltaEngine {
    fn name(&self) -> &str {
        "delta"
    }

    fn sync<'a>(
        &'a self,
        src: &'a Path,
        peer: SocketAddr,
        volume: &'a VolumeId,
        generation: u64,
        staging: Option<String>,
        progress: crate::progress::ProgressSink,
    ) -> BoxFuture<'a, Result<SyncStats>> {
        Box::pin(async move {
            self.sync_impl(src, peer, volume, generation, staging, progress)
                .await
        })
    }

    fn copy_state<'a>(
        &'a self,
        peer: SocketAddr,
        volume: &'a VolumeId,
    ) -> BoxFuture<'a, Result<crate::transport::CopyState>> {
        Box::pin(async move { transport::request_copy_state(peer, volume, &self.keys).await })
    }
}

fn transfer_err(e: std::io::Error) -> DriveError {
    DriveError::Transfer {
        message: e.to_string(),
    }
}

/// Run filesystem or hashing work on the blocking pool. Inline in these
/// `async fn`s it would hold a tokio worker and starve raft heartbeats; see
/// `scan::scan_off_runtime`.
pub(crate) async fn off_runtime<T, F>(what: &'static str, f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| DriveError::Transfer {
            message: format!("{what} did not run: {e}"),
        })?
}

/// A down node gives no RST, so a plain `TcpStream::connect` can sit for the OS
/// timeout (minutes). Sync loops over several targets must move on and record
/// the failure.
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl DeltaEngine {
    #[allow(clippy::too_many_arguments)]
    async fn sync_impl(
        &self,
        src: &Path,
        peer: SocketAddr,
        volume: &VolumeId,
        generation: u64,
        staging: Option<String>,
        progress: crate::progress::ProgressSink,
    ) -> Result<SyncStats> {
        // A missing source scans as an empty manifest with a zeroed root, which
        // would delete the peer's files and chmod its root to 000.
        if !src.is_dir() {
            return Err(DriveError::DestructiveSync {
                volume: volume.clone(),
                reason: format!("the source directory {} does not exist", src.display()),
            });
        }
        let mut conn = transport::connect_authenticated(peer, &self.keys).await?;

        transport::send(
            &mut conn,
            &Message::Hello {
                volume: volume.clone(),
                generation,
                staging,
            },
        )
        .await
        .map_err(transfer_err)?;

        let remote = match transport::recv(&mut conn).await.map_err(transfer_err)? {
            Some(Message::Ready(manifest)) => manifest,
            Some(other) => {
                return Err(DriveError::Transfer {
                    message: format!("expected ready, got {other:?}"),
                })
            }
            None => {
                return Err(DriveError::Transfer {
                    message: "peer closed before ready".into(),
                })
            }
        };

        let local = scan::scan_off_runtime(src.to_path_buf(), false).await?;
        guard_against_data_loss(volume, &local, &remote)?;
        let diff = local.diff(&remote);
        let total_files = (diff.added.len() + diff.changed.len()) as u64;
        let mut files_done: u64 = 0;
        progress.report(0, total_files);

        let mut stats = SyncStats {
            engine: self.name().to_string(),
            ..Default::default()
        };

        // Parents first, before files go in: empty directories exist nowhere else
        // in this protocol.
        for path in &diff.added_dirs {
            let meta = local.dirs[path];
            transport::send(
                &mut conn,
                &Message::MakeDir {
                    path: path.clone(),
                    mode: meta.mode,
                    uid: meta.uid,
                    gid: meta.gid,
                },
            )
            .await
            .map_err(transfer_err)?;
        }

        // Directories both sides have whose mode/owner drifted.
        for path in &diff.changed_dirs {
            let meta = local.dirs[path];
            transport::send(
                &mut conn,
                &Message::SetDirMeta {
                    path: path.clone(),
                    mode: meta.mode,
                    uid: meta.uid,
                    gid: meta.gid,
                },
            )
            .await
            .map_err(transfer_err)?;
        }

        if let Some(meta) = diff.root {
            transport::send(
                &mut conn,
                &Message::SetRootMeta {
                    mode: meta.mode,
                    uid: meta.uid,
                    gid: meta.gid,
                },
            )
            .await
            .map_err(transfer_err)?;
        }

        // Rename detection: hash local "added" candidates, ask the peer for the
        // hash of each "removed" one. A match means a move.
        let mut removed: Vec<String> = diff.removed;
        let mut renamed_from = std::collections::HashSet::new();
        let mut renamed_to = std::collections::HashSet::new();

        if !removed.is_empty() {
            // One hash can belong to many paths, so map to a *list* and pop one per
            // match. A database directory holds hundreds of byte-identical files
            // (empty postgres `_fsm`/`_vm` forks); keyed to one path, N additions
            // claim the same source, the second `Rename` hits a receiver that no
            // longer has `from`, and the whole sync dies. See the empty-file guard.
            let mut removed_hashes: HashMap<[u8; 32], Vec<String>> = HashMap::new();
            for path in &removed {
                transport::send(&mut conn, &Message::HashRequest(path.clone()))
                    .await
                    .map_err(transfer_err)?;
                match transport::recv(&mut conn).await.map_err(transfer_err)? {
                    Some(Message::Hash {
                        hash: Some(hash), ..
                    }) => {
                        removed_hashes.entry(hash).or_default().push(path.clone());
                    }
                    Some(Message::Hash { hash: None, .. }) => {}
                    Some(other) => {
                        return Err(DriveError::Transfer {
                            message: format!("expected hash, got {other:?}"),
                        })
                    }
                    None => {
                        return Err(DriveError::Transfer {
                            message: "peer closed mid hash".into(),
                        })
                    }
                }
            }

            for path in &diff.added {
                let meta = &local.files[path];
                if meta.is_symlink {
                    continue;
                }
                // Empty files are never renames: they all hash alike, and `Rename`
                // moves the receiver's file with the *source's* mode and mtime, so
                // it differs from the sender's copy and is re-sent anyway.
                if meta.size == 0 {
                    continue;
                }
                let full = src.join(path);
                let hashed = off_runtime("hashing a rename candidate", move || {
                    Ok(scan::hash_file(&full).ok())
                })
                .await?;
                let Some(hash) = hashed else { continue };
                // `pop` so a source is claimed at most once; leftovers are sent whole.
                let Some(from) = removed_hashes.get_mut(&hash).and_then(Vec::pop) else {
                    continue;
                };
                transport::send(
                    &mut conn,
                    &Message::Rename {
                        from: from.clone(),
                        to: path.clone(),
                        mode: meta.mode,
                        uid: meta.uid,
                        gid: meta.gid,
                        mtime_nanos: meta.mtime_nanos,
                    },
                )
                .await
                .map_err(transfer_err)?;
                renamed_from.insert(from);
                renamed_to.insert(path.clone());
                stats.files_renamed += 1;
            }
        }
        removed.retain(|p| !renamed_from.contains(p));

        for path in &removed {
            transport::send(&mut conn, &Message::Delete(path.clone()))
                .await
                .map_err(transfer_err)?;
            stats.files_removed += 1;
        }

        for path in &diff.added {
            if renamed_to.contains(path) {
                continue;
            }
            let meta = &local.files[path];
            self.send_whole(&mut conn, src, path, meta, &mut stats)
                .await?;
            stats.files_added += 1;
            files_done += 1;
            progress.report(files_done, total_files);
        }

        for path in &diff.changed {
            let meta = &local.files[path];
            // Too small for a signature round trip, or too big to diff (`MAX_DELTA_FILE`).
            if meta.is_symlink
                || meta.size < self.whole_file_threshold
                || meta.size > MAX_DELTA_FILE
            {
                self.send_whole(&mut conn, src, path, meta, &mut stats)
                    .await?;
            } else {
                self.send_delta(&mut conn, src, path, meta, &mut stats)
                    .await?;
            }
            stats.files_changed += 1;
            files_done += 1;
            progress.report(files_done, total_files);
        }

        // Last, once their files are gone (`removed_dirs` is children-first).
        for path in &diff.removed_dirs {
            // Now under a file or symlink here (a type change): the receiver already
            // cleared the old directory, and the path no longer resolves as one.
            if under_a_file(&local, path) {
                continue;
            }
            transport::send(&mut conn, &Message::RemoveDir(path.clone()))
                .await
                .map_err(transfer_err)?;
        }

        transport::send(&mut conn, &Message::Done)
            .await
            .map_err(transfer_err)?;
        match transport::recv(&mut conn).await.map_err(transfer_err)? {
            Some(Message::Ack(acked)) if acked == generation => Ok(stats),
            Some(Message::Ack(acked)) => Err(DriveError::Transfer {
                message: format!("peer acked generation {acked}, expected {generation}"),
            }),
            Some(other) => Err(DriveError::Transfer {
                message: format!("expected ack, got {other:?}"),
            }),
            None => Err(DriveError::Transfer {
                message: "peer closed before ack".into(),
            }),
        }
    }

    async fn send_whole(
        &self,
        conn: &mut transport::Connection,
        src: &Path,
        path: &str,
        meta: &FileMeta,
        stats: &mut SyncStats,
    ) -> Result<()> {
        if meta.is_symlink {
            stats.bytes_sent += meta.size;
            return transport::send(
                conn,
                &Message::WholeFile {
                    path: path.to_string(),
                    mode: meta.mode,
                    uid: meta.uid,
                    gid: meta.gid,
                    mtime_nanos: meta.mtime_nanos,
                    is_symlink: true,
                    link_target: meta.link_target.clone(),
                    data: Vec::new(),
                },
            )
            .await
            .map_err(transfer_err);
        }

        stats.bytes_sent += meta.size;
        if meta.size <= transport::WHOLE_FILE_CHUNK as u64 {
            let full = src.join(path);
            let data = off_runtime("reading a file to send", move || {
                // Replaced by a directory or symlink since the scan: skip, the next
                // pass sees the new type.
                if !is_regular_file(&full) {
                    return Ok(None);
                }
                match std::fs::read(&full) {
                    Ok(data) => Ok(Some(data)),
                    Err(e) if scan::vanished(Some(&e)) || !is_regular_file(&full) => Ok(None),
                    Err(source) => Err(DriveError::Io { path: full, source }),
                }
            })
            .await?;
            // Deleted since the scan (e.g. database scratch file): next pass sees it gone.
            let Some(data) = data else {
                return Ok(());
            };
            return transport::send(
                conn,
                &Message::WholeFile {
                    path: path.to_string(),
                    mode: meta.mode,
                    uid: meta.uid,
                    gid: meta.gid,
                    mtime_nanos: meta.mtime_nanos,
                    is_symlink: false,
                    link_target: None,
                    data,
                },
            )
            .await
            .map_err(transfer_err);
        }

        // Large file: stream as `WholeFileBegin`/`Chunk`*/`End`; see
        // `WHOLE_FILE_CHUNK` for why one frame can't hold it.
        let full_path = src.join(path);
        let opened = full_path.clone();
        let file = off_runtime(
            "opening a file to send",
            move || match std::fs::File::open(&opened) {
                Ok(file) if file.metadata().map(|m| m.is_file()).unwrap_or(false) => Ok(Some(file)),
                Ok(_) => Ok(None),
                Err(e) if scan::vanished(Some(&e)) => Ok(None),
                Err(source) => Err(DriveError::Io {
                    path: opened,
                    source,
                }),
            },
        )
        .await?;
        // Deleted since the scan; checked before `Begin` so the receiver never sees
        // a half-announced file.
        let Some(mut file) = file else {
            return Ok(());
        };
        transport::send(
            conn,
            &Message::WholeFileBegin {
                path: path.to_string(),
                mode: meta.mode,
                uid: meta.uid,
                gid: meta.gid,
                mtime_nanos: meta.mtime_nanos,
            },
        )
        .await
        .map_err(transfer_err)?;
        // Handle and buffer go to the blocking pool and back per chunk, keeping IO
        // and allocation off runtime threads; the buffer is reused.
        let mut buf = vec![0u8; transport::WHOLE_FILE_CHUNK];
        loop {
            let at = full_path.clone();
            let (returned_file, returned_buf, n) =
                off_runtime("reading a chunk to send", move || {
                    let mut file = file;
                    let mut buf = buf;
                    match std::io::Read::read(&mut file, &mut buf) {
                        Ok(n) => Ok((file, buf, n)),
                        Err(source) => Err(DriveError::Io { path: at, source }),
                    }
                })
                .await?;
            file = returned_file;
            buf = returned_buf;
            if n == 0 {
                break;
            }
            transport::send(
                conn,
                &Message::WholeFileChunk {
                    data: buf[..n].to_vec(),
                },
            )
            .await
            .map_err(transfer_err)?;
        }
        transport::send(conn, &Message::WholeFileEnd)
            .await
            .map_err(transfer_err)
    }

    async fn send_delta(
        &self,
        conn: &mut transport::Connection,
        src: &Path,
        path: &str,
        meta: &FileMeta,
        stats: &mut SyncStats,
    ) -> Result<()> {
        // Deleted since the scan: skip before asking the peer for a signature.
        if !is_regular_file(&src.join(path)) {
            return Ok(());
        }
        transport::send(conn, &Message::SignatureRequest(path.to_string()))
            .await
            .map_err(transfer_err)?;
        let sig_data = match transport::recv(conn).await.map_err(transfer_err)? {
            Some(Message::Signature { data, .. }) => data,
            Some(other) => {
                return Err(DriveError::Transfer {
                    message: format!("expected signature, got {other:?}"),
                })
            }
            None => {
                return Err(DriveError::Transfer {
                    message: "peer closed mid signature".into(),
                })
            }
        };
        let full = src.join(path);
        let named = path.to_string();
        let delta = off_runtime("computing a delta", move || {
            let signature = Signature::deserialize(sig_data).map_err(|e| DriveError::Transfer {
                message: format!("bad signature for {named}: {e}"),
            })?;
            let indexed = signature.index();

            let local_data = match std::fs::read(&full) {
                Ok(data) => data,
                // Replaced since the check above: skip, the next pass resends it.
                Err(e) if scan::vanished(Some(&e)) || !is_regular_file(&full) => return Ok(None),
                Err(source) => return Err(DriveError::Io { path: full, source }),
            };
            let mut delta = Vec::new();
            rsync_diff(&indexed, &local_data, &mut delta).map_err(|e| DriveError::Transfer {
                message: format!("diff for {named}: {e}"),
            })?;
            Ok(Some(delta))
        })
        .await?;
        let Some(delta) = delta else {
            // The receiver is waiting for a Delta it will not get; its temp state is
            // untouched, so the next frame just proceeds.
            return Ok(());
        };

        stats.bytes_sent += delta.len() as u64;
        transport::send(
            conn,
            &Message::Delta {
                path: path.to_string(),
                mode: meta.mode,
                uid: meta.uid,
                gid: meta.gid,
                mtime_nanos: meta.mtime_nanos,
                data: delta,
            },
        )
        .await
        .map_err(transfer_err)
    }
}

/// `true` if a strict ancestor of `path` is a file or symlink in `manifest`.
fn under_a_file(manifest: &scan::Manifest, path: &str) -> bool {
    let mut end = 0;
    while let Some(slash) = path[end..].find('/') {
        end += slash;
        if manifest.files.contains_key(&path[..end]) {
            return true;
        }
        end += 1;
    }
    false
}

/// `true` if `path` is currently a regular file (not a directory, symlink, or gone).
fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Detects btrfs/zfs and would shell out to their send/receive tools. Not
/// implemented; both fall through to `DeltaEngine`. A documented seam, since
/// testing needs a real btrfs/zfs filesystem.
pub mod fast_path {
    use std::path::Path;

    use nix::sys::statfs::{FsType, BTRFS_SUPER_MAGIC};

    /// Filesystem magic numbers from `statfs(2)`.
    const ZFS_SUPER_MAGIC: FsType = FsType(0x2fc12fc1);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FsKind {
        Btrfs,
        Zfs,
        Other,
    }

    /// Probe the filesystem backing `path`; `Other` for anything unrecognized or
    /// unreadable.
    pub fn detect(path: &Path) -> FsKind {
        let Ok(stat) = nix::sys::statfs::statfs(path) else {
            return FsKind::Other;
        };
        match stat.filesystem_type() {
            BTRFS_SUPER_MAGIC => FsKind::Btrfs,
            ZFS_SUPER_MAGIC => FsKind::Zfs,
            _ => FsKind::Other,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn detect_does_not_panic_on_a_normal_directory() {
            let tmp = tempfile::tempdir().unwrap();
            // CI is very unlikely to be btrfs/zfs; this just checks the probe doesn't crash.
            let _ = detect(tmp.path());
        }

        #[test]
        fn detect_handles_a_missing_path_without_erroring() {
            assert_eq!(
                detect(Path::new("/definitely/does/not/exist")),
                FsKind::Other
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_of(files: usize) -> scan::Manifest {
        let meta = FileMeta {
            size: 1,
            mtime_nanos: 0,
            mode: 0o644,
            uid: 0,
            gid: 0,
            is_symlink: false,
            link_target: None,
            hash: None,
        };
        scan::Manifest {
            files: (0..files)
                .map(|i| (format!("f{i}"), meta.clone()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_source_is_refused_over_a_populated_destination() {
        let id = VolumeId::new("fast", "db");
        let err = guard_against_data_loss(&id, &manifest_of(0), &manifest_of(3)).unwrap_err();
        assert!(matches!(err, DriveError::DestructiveSync { .. }), "{err}");
        assert!(err.to_string().contains("holds 3"), "{err}");
    }

    #[test]
    fn empty_over_empty_and_ordinary_changes_are_allowed() {
        let id = VolumeId::new("fast", "db");
        assert!(guard_against_data_loss(&id, &manifest_of(0), &manifest_of(0)).is_ok());
        assert!(guard_against_data_loss(&id, &manifest_of(5), &manifest_of(0)).is_ok());
        assert!(guard_against_data_loss(&id, &manifest_of(2), &manifest_of(3)).is_ok());
    }

    #[test]
    fn a_large_collapse_is_refused_but_only_against_a_big_destination() {
        let id = VolumeId::new("fast", "db");
        assert!(guard_against_data_loss(&id, &manifest_of(5), &manifest_of(400)).is_err());
        assert!(guard_against_data_loss(&id, &manifest_of(30), &manifest_of(400)).is_ok());
        assert!(guard_against_data_loss(&id, &manifest_of(1), &manifest_of(50)).is_ok());
    }

    #[test]
    fn default_engine_is_named_delta() {
        assert_eq!(DeltaEngine::new(cl_raft::throwaway_key()).name(), "delta");
    }

    /// The delta path costs `2 x filesize` resident, so it needs an upper bound
    /// too. The window must stay ordered and leave the chunked whole-file path
    /// to handle the rest.
    #[test]
    fn the_delta_window_is_bounded_at_both_ends() {
        const {
            assert!(WHOLE_FILE_THRESHOLD < MAX_DELTA_FILE);
            assert!(MAX_DELTA_FILE > transport::WHOLE_FILE_CHUNK as u64);
        }
    }
}
