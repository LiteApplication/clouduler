//! The transfer engine: decides what to send for a volume and drives the
//! sender side of the `transport` protocol.
//!
//! `DeltaEngine` is the default and always available: rolling-hash delta
//! sync over `fast_rsync`, our own directory walk, rename/delete handling
//! and resumable (temp file + rename) writes on the receiver.
//!
//! `BtrfsEngine` and `ZfsEngine` are optional fast paths for when both ends
//! back the drive with a matching filesystem. Neither crates.io crate for
//! this (`btrfsutil`, `libzetta`) is maintained enough to depend on as of
//! this writing, so both shell out to the `btrfs`/`zfs` binaries and detect
//! availability by probing the filesystem type of the drive path with
//! `statfs`. They are behind the same `SyncEngine` trait so a caller that
//! picks the fast path can always fall back to `DeltaEngine`.

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

/// Below this size, sending the whole file beats a signature round trip:
/// the extra request/response pair costs more than the delta would ever save.
/// Only applies to files that exist on both ends; brand new files have
/// nothing to diff against and are always sent whole.
const WHOLE_FILE_THRESHOLD: u64 = 8 * 1024;

/// Above this size a changed file is streamed whole instead of diffed.
///
/// The delta path is O(filesize) in *resident memory* on both ends, and
/// nothing else in this protocol is. The receiver's `Message::Delta` handler
/// holds the base file and the reconstructed output at the same time —
/// `fast_rsync::apply` writes into a `Vec`, it cannot stream — so applying a
/// delta costs `2 x filesize` plus the delta itself; the sender's
/// `send_delta` holds another full copy to diff against. With multi-gigabyte
/// files in a volume (a database relation, an ONNX model, a stray core dump)
/// that is several gigabytes for one file, and `transport::serve` accepts
/// every peer concurrently, so it multiplies by the number of nodes pushing.
/// On an 8GB node that is an OOM kill, which looks from the sender like a
/// bare "Connection reset by peer".
///
/// Past this size the `WholeFileBegin`/`WholeFileChunk`/`WholeFileEnd` path
/// is used instead: it streams in `WHOLE_FILE_CHUNK` pieces and its peak
/// memory is that chunk, whatever the file's size. The cost is bandwidth —
/// a big file that changed is resent in full rather than diffed — which is
/// the right trade against an unbounded allocation that takes the node down.
const MAX_DELTA_FILE: u64 = 256 * 1024 * 1024;

/// Against a destination holding at least `DROP_CHECK_MIN_FILES` files, a
/// source with under 1/`DROP_FRACTION` of that is lost data, not a clean-up.
const DROP_CHECK_MIN_FILES: usize = 200;
const DROP_FRACTION: usize = 20;

/// Refuses a sync that would delete a destination's data because the source
/// has lost its own: an empty (or nearly empty) tree pushed over a populated
/// one. A volume deliberately emptied needs one file left in it, or its
/// replicas cleared by hand; a replicated wipe is not recoverable.
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

/// What one sync did, for reporting and tests.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SyncStats {
    pub files_added: u64,
    pub files_changed: u64,
    pub files_removed: u64,
    pub files_renamed: u64,
    pub bytes_sent: u64,
    pub engine: String,
}

/// Replicates one volume from a local directory to a peer's drive transport
/// listener. Implementations run as the sender; `transport::receive` is the
/// one receiver used by every engine that talks this protocol.
pub trait SyncEngine: Send + Sync {
    fn name(&self) -> &str;

    /// `staging`, when set, marks a `NodeLocal`-elsewhere backup's one-off
    /// push and carries that destination's configured path - see
    /// `Message::Hello`'s doc comment in `transport.rs` for what it changes
    /// on the receiving end. Every other caller passes `None`.
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
}

/// The default engine: works anywhere, no filesystem cooperation required.
#[derive(Debug, Clone)]
pub struct DeltaEngine {
    whole_file_threshold: u64,
    /// The cluster key every connection authenticates with. A watch, not a
    /// snapshot, so a long-lived scheduler follows a key that arrives or
    /// rotates after it was built.
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
}

fn transfer_err(e: std::io::Error) -> DriveError {
    DriveError::Transfer {
        message: e.to_string(),
    }
}

/// Run one piece of filesystem or hashing work on the blocking pool.
///
/// The protocol chatter here is genuinely async, but the work between the
/// messages is not: reading a multi-gigabyte database file, blake3 hashing it,
/// building an rsync signature index, computing a delta. Left inline in these
/// `async fn`s it holds a tokio worker for the duration, which starves
/// everything else this process runs — including its raft heartbeats. See
/// `scan::scan_off_runtime` for what that looks like from the outside.
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

/// A node that is down rather than merely refusing gives no RST, so a plain
/// `TcpStream::connect` can sit for the OS's own connect timeout — minutes,
/// not seconds. A sync loop over several targets (the background scheduler,
/// or `folder-import`'s immediate push) has to move on and record the
/// failure instead of hanging on one unreachable peer.
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

        // Directories first, parents before children, before any file is
        // written into them. Empty directories exist nowhere else in this
        // protocol — `create_dir_all` on the way to a file only ever produces
        // the ones that happen to contain something.
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

        // Directories both sides already have, but whose mode/owner drifted —
        // a permission fix applied on the source after the directory was
        // first replicated, say.
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

        // Rename detection: hash local "added" candidates directly, and ask
        // the peer for the hash of each "removed" candidate (it still has
        // the file; we don't). A match means a move, not new content.
        let mut removed: Vec<String> = diff.removed;
        let mut renamed_from = std::collections::HashSet::new();
        let mut renamed_to = std::collections::HashSet::new();

        if !removed.is_empty() {
            // One hash can belong to many paths, so this maps to a *list* and
            // each match pops one. Content equality is not identity: a
            // database directory routinely holds hundreds of byte-identical
            // files (every freshly created postgres `_fsm`/`_vm` fork is
            // zero length), and they all hash the same. Keyed to a single
            // path, every one of those additions would claim the same source
            // and the sender would emit N renames out of one file — the
            // second arrives at a receiver that no longer has `from`, and the
            // whole sync dies there. See the empty-file guard below too.
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
                // Empty files are never renames. Every empty file hashes
                // alike, so a match here is evidence of nothing, and pairing
                // two arbitrary ones costs more than it saves: `Rename` moves
                // the receiver's existing file, which carries the *source's*
                // mode and mtime, so the result differs from what the sender
                // has and gets re-sent on the next pass anyway. Sending zero
                // bytes whole is cheaper than that round trip.
                if meta.size == 0 {
                    continue;
                }
                let full = src.join(path);
                // An unreadable candidate is simply not a rename, same as before.
                let hashed = off_runtime("hashing a rename candidate", move || {
                    Ok(scan::hash_file(&full).ok())
                })
                .await?;
                let Some(hash) = hashed else { continue };
                // `pop` so a source is claimed at most once: N identical
                // removals can pair with N identical additions, and any
                // addition left over is simply sent whole.
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
            // Too small to be worth a signature round trip, or too big to
            // diff without an unbounded allocation on both ends. See
            // `MAX_DELTA_FILE`.
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

        // Last, once every file inside them is gone: `diff.removed_dirs` is
        // already ordered children-first.
        for path in &diff.removed_dirs {
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
                match std::fs::read(&full) {
                    Ok(data) => Ok(Some(data)),
                    Err(e) if scan::vanished(Some(&e)) => Ok(None),
                    Err(source) => Err(DriveError::Io { path: full, source }),
                }
            })
            .await?;
            // Deleted since the scan (a database's scratch file): nothing to
            // send, and the next pass sees it gone.
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

        // Large regular file: stream it as a `WholeFileBegin`/`WholeFileChunk`*/
        // `WholeFileEnd` sequence instead of one giant frame. See
        // `WHOLE_FILE_CHUNK`'s doc comment for why a single frame can't be
        // trusted to hold an entire file past a few tens of megabytes.
        let full_path = src.join(path);
        let opened = full_path.clone();
        let file = off_runtime(
            "opening a file to send",
            move || match std::fs::File::open(&opened) {
                Ok(file) => Ok(Some(file)),
                Err(e) if scan::vanished(Some(&e)) => Ok(None),
                Err(source) => Err(DriveError::Io {
                    path: opened,
                    source,
                }),
            },
        )
        .await?;
        // Deleted since the scan: nothing to send. Checked before `Begin` so
        // the receiver is never left with a half-announced file.
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
        // The handle and the buffer are handed to the blocking pool and back
        // once per chunk, so neither the read nor the allocation lands on a
        // runtime thread, and the buffer is reused instead of reallocated.
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
        // Deleted since the scan: skip it before asking the peer for a
        // signature it would then wait on a delta for.
        if !src.join(path).exists() {
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
        // Indexing the signature, reading the local file and running the
        // rolling-hash diff are all CPU and IO over the whole file.
        let full = src.join(path);
        let named = path.to_string();
        let delta = off_runtime("computing a delta", move || {
            let signature = Signature::deserialize(sig_data).map_err(|e| DriveError::Transfer {
                message: format!("bad signature for {named}: {e}"),
            })?;
            let indexed = signature.index();

            let local_data = match std::fs::read(&full) {
                Ok(data) => data,
                Err(source) => return Err(DriveError::Io { path: full, source }),
            };
            let mut delta = Vec::new();
            rsync_diff(&indexed, &local_data, &mut delta).map_err(|e| DriveError::Transfer {
                message: format!("diff for {named}: {e}"),
            })?;
            Ok(delta)
        })
        .await?;

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

/// Detects btrfs/zfs and shells out to their send/receive tools. Neither is
/// implemented yet; both fall through to `DeltaEngine` today. Kept as a
/// documented seam rather than built in full, since exercising it needs a
/// real btrfs or zfs filesystem the test environment does not have.
pub mod fast_path {
    use std::path::Path;

    /// Filesystem magic numbers from `statfs(2)`, used to detect a fast path
    /// without shelling out just to find out we can't use it.
    const BTRFS_SUPER_MAGIC: i64 = 0x9123683e;
    const ZFS_SUPER_MAGIC: i64 = 0x2fc12fc1;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FsKind {
        Btrfs,
        Zfs,
        Other,
    }

    /// Probe the filesystem backing `path` with `statfs`. Returns `Other`
    /// (rather than an error) for anything unrecognized or unreadable, since
    /// callers only use this to decide whether to try a fast path.
    pub fn detect(path: &Path) -> FsKind {
        let c_path = match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
            Ok(p) => p,
            Err(_) => return FsKind::Other,
        };
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat) };
        if rc != 0 {
            return FsKind::Other;
        }
        match stat.f_type as i64 {
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
            // Whatever the test filesystem is, it's very unlikely to be
            // btrfs or zfs in CI; this just checks the probe doesn't crash.
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
        // Small volumes can legitimately shrink a lot.
        assert!(guard_against_data_loss(&id, &manifest_of(1), &manifest_of(50)).is_ok());
    }

    #[test]
    fn default_engine_is_named_delta() {
        assert_eq!(DeltaEngine::new(cl_raft::throwaway_key()).name(), "delta");
    }

    /// The delta path costs `2 x filesize` resident on the receiver and
    /// another copy on the sender, so it has to have an upper bound as well
    /// as a lower one. Both ends of the window must stay ordered, and the cap
    /// has to leave the chunked whole-file path (which is bounded by
    /// `WHOLE_FILE_CHUNK`, not by size) as the thing that handles the rest.
    #[test]
    fn the_delta_window_is_bounded_at_both_ends() {
        const {
            assert!(WHOLE_FILE_THRESHOLD < MAX_DELTA_FILE);
            // A capped file must still stream in more than one chunk.
            assert!(MAX_DELTA_FILE > transport::WHOLE_FILE_CHUNK as u64);
        }
    }
}
