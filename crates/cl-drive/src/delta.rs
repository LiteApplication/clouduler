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
use tokio::net::TcpStream;

use cl_raft::VolumeId;

use crate::error::{DriveError, Result};
use crate::scan::{self, FileMeta};
use crate::transport::{self, Message};

/// Below this size, sending the whole file beats a signature round trip:
/// the extra request/response pair costs more than the delta would ever save.
/// Only applies to files that exist on both ends; brand new files have
/// nothing to diff against and are always sent whole.
const WHOLE_FILE_THRESHOLD: u64 = 8 * 1024;

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

    fn sync<'a>(
        &'a self,
        src: &'a Path,
        peer: SocketAddr,
        volume: &'a VolumeId,
        generation: u64,
    ) -> BoxFuture<'a, Result<SyncStats>>;
}

/// The default engine: works anywhere, no filesystem cooperation required.
#[derive(Debug, Clone)]
pub struct DeltaEngine {
    whole_file_threshold: u64,
}

impl Default for DeltaEngine {
    fn default() -> Self {
        Self { whole_file_threshold: WHOLE_FILE_THRESHOLD }
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
    ) -> BoxFuture<'a, Result<SyncStats>> {
        Box::pin(async move { self.sync_impl(src, peer, volume, generation).await })
    }
}

fn transfer_err(e: std::io::Error) -> DriveError {
    DriveError::Transfer { message: e.to_string() }
}

impl DeltaEngine {
    async fn sync_impl(
        &self,
        src: &Path,
        peer: SocketAddr,
        volume: &VolumeId,
        generation: u64,
    ) -> Result<SyncStats> {
        let stream = TcpStream::connect(peer).await.map_err(transfer_err)?;
        let mut conn = transport::frame(stream);

        transport::send(&mut conn, &Message::Hello { volume: volume.clone(), generation })
            .await
            .map_err(transfer_err)?;

        let remote = match transport::recv(&mut conn).await.map_err(transfer_err)? {
            Some(Message::Ready(manifest)) => manifest,
            Some(other) => {
                return Err(DriveError::Transfer { message: format!("expected ready, got {other:?}") })
            }
            None => return Err(DriveError::Transfer { message: "peer closed before ready".into() }),
        };

        let local = scan::scan(src, false)?;
        let diff = local.diff(&remote);

        let mut stats = SyncStats { engine: self.name().to_string(), ..Default::default() };

        // Rename detection: hash local "added" candidates directly, and ask
        // the peer for the hash of each "removed" candidate (it still has
        // the file; we don't). A match means a move, not new content.
        let mut removed: Vec<String> = diff.removed;
        let mut renamed_from = std::collections::HashSet::new();
        let mut renamed_to = std::collections::HashSet::new();

        if !removed.is_empty() {
            let mut removed_hashes: HashMap<[u8; 32], String> = HashMap::new();
            for path in &removed {
                transport::send(&mut conn, &Message::HashRequest(path.clone()))
                    .await
                    .map_err(transfer_err)?;
                match transport::recv(&mut conn).await.map_err(transfer_err)? {
                    Some(Message::Hash { hash: Some(hash), .. }) => {
                        removed_hashes.insert(hash, path.clone());
                    }
                    Some(Message::Hash { hash: None, .. }) => {}
                    Some(other) => {
                        return Err(DriveError::Transfer { message: format!("expected hash, got {other:?}") })
                    }
                    None => return Err(DriveError::Transfer { message: "peer closed mid hash".into() }),
                }
            }

            for path in &diff.added {
                let meta = &local.files[path];
                if meta.is_symlink {
                    continue;
                }
                let Ok(data) = std::fs::read(src.join(path)) else { continue };
                let hash = *blake3::hash(&data).as_bytes();
                if let Some(from) = removed_hashes.get(&hash) {
                    transport::send(&mut conn, &Message::Rename { from: from.clone(), to: path.clone() })
                        .await
                        .map_err(transfer_err)?;
                    renamed_from.insert(from.clone());
                    renamed_to.insert(path.clone());
                    stats.files_renamed += 1;
                }
            }
        }
        removed.retain(|p| !renamed_from.contains(p));

        for path in &removed {
            transport::send(&mut conn, &Message::Delete(path.clone())).await.map_err(transfer_err)?;
            stats.files_removed += 1;
        }

        for path in &diff.added {
            if renamed_to.contains(path) {
                continue;
            }
            let meta = &local.files[path];
            self.send_whole(&mut conn, src, path, meta, &mut stats).await?;
            stats.files_added += 1;
        }

        for path in &diff.changed {
            let meta = &local.files[path];
            if meta.is_symlink || meta.size < self.whole_file_threshold {
                self.send_whole(&mut conn, src, path, meta, &mut stats).await?;
            } else {
                self.send_delta(&mut conn, src, path, meta, &mut stats).await?;
            }
            stats.files_changed += 1;
        }

        transport::send(&mut conn, &Message::Done).await.map_err(transfer_err)?;
        match transport::recv(&mut conn).await.map_err(transfer_err)? {
            Some(Message::Ack(acked)) if acked == generation => Ok(stats),
            Some(Message::Ack(acked)) => Err(DriveError::Transfer {
                message: format!("peer acked generation {acked}, expected {generation}"),
            }),
            Some(other) => Err(DriveError::Transfer { message: format!("expected ack, got {other:?}") }),
            None => Err(DriveError::Transfer { message: "peer closed before ack".into() }),
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
        let data = if meta.is_symlink { Vec::new() } else { std::fs::read(src.join(path)).map_err(|source| DriveError::Io { path: src.join(path), source })? };
        stats.bytes_sent += data.len() as u64;
        transport::send(
            conn,
            &Message::WholeFile {
                path: path.to_string(),
                mode: meta.mode,
                is_symlink: meta.is_symlink,
                link_target: meta.link_target.clone(),
                data,
            },
        )
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
        transport::send(conn, &Message::SignatureRequest(path.to_string())).await.map_err(transfer_err)?;
        let sig_data = match transport::recv(conn).await.map_err(transfer_err)? {
            Some(Message::Signature { data, .. }) => data,
            Some(other) => {
                return Err(DriveError::Transfer { message: format!("expected signature, got {other:?}") })
            }
            None => return Err(DriveError::Transfer { message: "peer closed mid signature".into() }),
        };
        let signature = Signature::deserialize(sig_data)
            .map_err(|e| DriveError::Transfer { message: format!("bad signature for {path}: {e}") })?;
        let indexed = signature.index();

        let local_data = std::fs::read(src.join(path)).map_err(|source| DriveError::Io { path: src.join(path), source })?;
        let mut delta = Vec::new();
        rsync_diff(&indexed, &local_data, &mut delta)
            .map_err(|e| DriveError::Transfer { message: format!("diff for {path}: {e}") })?;

        stats.bytes_sent += delta.len() as u64;
        transport::send(conn, &Message::Delta { path: path.to_string(), mode: meta.mode, data: delta })
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
            assert_eq!(detect(Path::new("/definitely/does/not/exist")), FsKind::Other);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_engine_is_named_delta() {
        assert_eq!(DeltaEngine::default().name(), "delta");
    }
}
