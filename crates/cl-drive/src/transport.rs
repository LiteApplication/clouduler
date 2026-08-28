//! Wire protocol for bulk volume transfer.
//!
//! Kept off the raft rpc channel so a multi-megabyte delta never sits behind
//! a heartbeat. Framed like `cl_raft::rpc` (length delimited over plain TCP),
//! but encoded with `bincode` rather than JSON: this channel carries mostly
//! raw file bytes and deltas, and a byte vector round-trips as its literal
//! bytes plus a length prefix instead of a comma-separated JSON array of
//! decimal integers. Nothing else talks this protocol, so there's no
//! cross-version compatibility to preserve.
//!
//! This module owns both directions of the protocol: `send`/`recv` and the
//! `Message` enum are shared, and `serve` runs the receive side (the sender
//! side lives in `delta::DeltaEngine`, which is the thing that decides what
//! to send).

use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fast_rsync::{apply, Signature, SignatureOptions};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, warn};

use cl_raft::VolumeId;

use crate::delta::off_runtime;
use crate::error::{DriveError, Result};
use crate::registry::{resolve_relative, DriveRegistry};
use crate::scan::{self, Manifest};

/// Deltas and whole files can be large; refuse anything absurd so a corrupt
/// length prefix cannot make us allocate wildly.
const MAX_FRAME: usize = 256 * 1024 * 1024;

/// Files at or below this size go over the wire as a single `WholeFile`
/// message. Above it, `send_whole` streams `WholeFileBegin`/`WholeFileChunk`/
/// `WholeFileEnd` instead, so one huge file can't force a single allocation
/// and frame the size of the whole thing and so progress can be observed
/// (and backpressure applied) chunk by chunk rather than after one giant
/// read. Chosen with headroom under `MAX_FRAME` for the message envelope.
pub const WHOLE_FILE_CHUNK: usize = 32 * 1024 * 1024;

/// How many peers may be pushing to this node at once. See `serve`.
const MAX_CONCURRENT_RECEIVES: usize = 2;

/// rpc_port + 1. Keeps bulk transfer off the raft socket without adding a
/// second port to the config file.
pub fn drive_port(rpc_port: u16) -> u16 {
    rpc_port.wrapping_add(1)
}

/// Rolling signature block size. Small enough to localize a match, large
/// enough that the signature itself stays a small fraction of the file.
pub fn signature_options() -> SignatureOptions {
    SignatureOptions {
        block_size: 2048,
        crypto_hash_size: 8,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Message {
    /// Sender announces the volume and the generation this sync will reach.
    Hello { volume: VolumeId, generation: u64 },
    /// Receiver's current manifest, scanned without hashing.
    Ready(Manifest),
    /// A one-shot manifest request outside a sync, used for migration
    /// verification. Answered with `Ready` and the connection closed.
    ManifestRequest { volume: VolumeId, hash_content: bool },
    SignatureRequest(String),
    Signature { path: String, data: Vec<u8> },
    HashRequest(String),
    /// `None` when the receiver has no such file, e.g. a stale rename guess.
    Hash { path: String, hash: Option<[u8; 32]> },
    WholeFile {
        path: String,
        mode: u32,
        /// Nanoseconds since the epoch, straight from the sender's
        /// `FileMeta::mtime_nanos`. Applied on write so the receiver's copy
        /// carries the source's mtime rather than the time it was written —
        /// without this, the next scan sees a fresh "now" mtime that never
        /// matches the sender's, and re-syncs the file forever.
        mtime_nanos: i64,
        is_symlink: bool,
        link_target: Option<String>,
        data: Vec<u8>,
    },
    /// Starts a chunked whole-file transfer for a regular file too large to
    /// send as one `WholeFile` frame. Followed by one or more
    /// `WholeFileChunk`s and a final `WholeFileEnd`.
    WholeFileBegin { path: String, mode: u32, mtime_nanos: i64 },
    WholeFileChunk { data: Vec<u8> },
    WholeFileEnd,
    Delta { path: String, mode: u32, mtime_nanos: i64, data: Vec<u8> },
    /// Move `from` to `to` on the receiver, then stamp the sender's `mode`
    /// and `mtime_nanos` on it.
    ///
    /// The metadata is not decoration. A rename moves the receiver's existing
    /// file, which carries whatever mode and mtime the *source* had when it
    /// was synced — not the ones the sender's `to` has now. Without
    /// restamping, `Manifest::diff` sees a mismatch on the very next pass and
    /// re-sends the file whole, which is exactly the work the rename existed
    /// to avoid. Same reasoning as `WholeFile::mtime_nanos`.
    Rename { from: String, to: String, mode: u32, mtime_nanos: i64 },
    Delete(String),
    /// Sender is done; receiver replies with `Ack`.
    Done,
    Ack(u64),
    Error(String),
    /// Create a directory (and any missing parents) with these permissions.
    /// Sent for directories the peer is missing, ahead of any file, so an
    /// *empty* directory survives replication — nothing else would create one.
    ///
    /// New variants go at the end: `Message` is encoded with bincode, which
    /// writes the variant's index and is not self-describing, so inserting
    /// anywhere else silently renumbers every variant after it.
    MakeDir { path: String, mode: u32 },
    /// Remove a directory the sender no longer has. Best effort and
    /// non-recursive on the receiving side.
    RemoveDir(String),
}

pub type Connection = Framed<TcpStream, LengthDelimitedCodec>;

pub fn frame(stream: TcpStream) -> Connection {
    let _ = stream.set_nodelay(true);
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_framed(stream)
}

pub async fn send(conn: &mut Connection, msg: &Message) -> io::Result<()> {
    let raw = bincode::serialize(msg).map_err(io::Error::other)?;
    conn.send(bytes::Bytes::from(raw)).await
}

/// `Ok(None)` means the peer closed the connection cleanly.
pub async fn recv(conn: &mut Connection) -> io::Result<Option<Message>> {
    match conn.next().await {
        Some(Ok(raw)) => Ok(Some(bincode::deserialize(&raw).map_err(io::Error::other)?)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

fn io_err(e: io::Error) -> DriveError {
    DriveError::Transfer { message: e.to_string() }
}

/// Accept connections and run the receive side of the sync protocol until
/// the task is cancelled.
pub async fn serve(addr: SocketAddr, registry: Arc<DriveRegistry>) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    debug!(%addr, "drive transport listening");

    // Every node that replicates a drive here pushes on its own schedule, and
    // a transfer's working set is bounded per connection but not across them.
    // Unlimited, one sync pass per peer arriving at the same moment multiplies
    // that by the size of the cluster — which on a small node is an OOM kill,
    // reported to each sender as a bare "Connection reset by peer". Senders
    // that wait simply take longer; the sync interval is minutes.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_RECEIVES));

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let _slot = match slots.acquire_owned().await {
                Ok(slot) => slot,
                // Only if the semaphore is closed, which never happens here.
                Err(_) => return,
            };
            if let Err(e) = receive(stream, &registry).await {
                // `warn`, not `debug`: this is the *only* place the real
                // reason a sync failed exists. All the sender ever learns is
                // that the socket closed, so it reports "Broken pipe" — which
                // says nothing about which file or which error. A sync that
                // keeps failing is invisible at the default level otherwise.
                warn!(%peer, error = %e, "drive transfer connection closed");
            }
        });
    }
}

/// Run the receive side of one sync over an already accepted connection.
/// Exposed directly so tests can drive it over a loopback pair without a
/// listener.
pub async fn receive(stream: TcpStream, registry: &DriveRegistry) -> Result<()> {
    let mut conn = frame(stream);

    let (volume, generation) = match recv(&mut conn).await.map_err(io_err)? {
        Some(Message::Hello { volume, generation }) => (volume, generation),
        Some(Message::ManifestRequest { volume, hash_content }) => {
            let root = registry.ensure_volume(&volume)?;
            let manifest = scan::scan_off_runtime(root, hash_content).await?;
            send(&mut conn, &Message::Ready(manifest)).await.map_err(io_err)?;
            return Ok(());
        }
        Some(other) => {
            return Err(DriveError::Transfer {
                message: format!("expected hello, got {other:?}"),
            })
        }
        None => return Ok(()),
    };

    let root = registry.ensure_volume(&volume)?;
    let manifest = scan::scan_off_runtime(root.clone(), false).await?;
    send(&mut conn, &Message::Ready(manifest)).await.map_err(io_err)?;

    // State for a `WholeFileBegin`/`WholeFileChunk`*/`WholeFileEnd` sequence
    // in progress: the final target, the temp file it's being written to,
    // its mode, mtime and the open handle chunks are appended to.
    let mut chunked: Option<(PathBuf, PathBuf, u32, i64, std::fs::File)> = None;

    loop {
        match recv(&mut conn).await.map_err(io_err)? {
            Some(Message::WholeFileBegin { path, mode, mtime_nanos }) => {
                let target = resolve_relative(&root, &path)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|source| DriveError::Io { path: parent.to_path_buf(), source })?;
                }
                let tmp = temp_path(&target);
                let file = std::fs::File::create(&tmp).map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
                chunked = Some((target, tmp, mode, mtime_nanos, file));
            }
            Some(Message::WholeFileChunk { data }) => {
                let state = chunked.take().ok_or_else(|| DriveError::Transfer {
                    message: "WholeFileChunk with no WholeFileBegin in progress".into(),
                })?;
                // Hand the open handle to the blocking pool and take it back,
                // so writing a chunk of a multi-gigabyte file never runs on a
                // runtime thread.
                chunked = Some(
                    off_runtime("writing a chunk", move || {
                        let (target, tmp, mode, mtime_nanos, mut file) = state;
                        match file.write_all(&data) {
                            Ok(()) => Ok((target, tmp, mode, mtime_nanos, file)),
                            Err(source) => Err(DriveError::Io { path: tmp, source }),
                        }
                    })
                    .await?,
                );
            }
            Some(Message::WholeFileEnd) => {
                let (target, tmp, mode, mtime_nanos, file) = chunked.take().ok_or_else(|| DriveError::Transfer {
                    message: "WholeFileEnd with no WholeFileBegin in progress".into(),
                })?;
                off_runtime("finishing a chunked file", move || {
                    drop(file);
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
                        .map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
                    let mtime = scan::nanos_to_filetime(mtime_nanos);
                    filetime::set_file_times(&tmp, mtime, mtime)
                        .map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
                    std::fs::rename(&tmp, &target).map_err(|source| DriveError::Io { path: target, source })
                })
                .await?;
            }
            Some(Message::SignatureRequest(path)) => {
                let target = resolve_relative(&root, &path)?;
                // Reading the file and building its rolling-hash signature is
                // whole-file IO plus CPU.
                let data = off_runtime("building a signature", move || {
                    let data = std::fs::read(&target).unwrap_or_default();
                    Ok(Signature::calculate(&data, signature_options()).into_serialized())
                })
                .await?;
                send(&mut conn, &Message::Signature { path, data }).await.map_err(io_err)?;
            }
            Some(Message::HashRequest(path)) => {
                let target = resolve_relative(&root, &path)?;
                // Streamed, not `fs::read`: a hash is requested for every
                // file the sender no longer has, whatever its size.
                let hash = off_runtime("hashing a file", move || Ok(scan::hash_file(&target).ok())).await?;
                send(&mut conn, &Message::Hash { path, hash }).await.map_err(io_err)?;
            }
            Some(Message::WholeFile { path, mode, mtime_nanos, is_symlink, link_target, data }) => {
                let root = root.clone();
                off_runtime("writing a whole file", move || {
                    write_whole(&root, &path, mode, mtime_nanos, is_symlink, link_target.as_deref(), &data)
                })
                .await?;
            }
            Some(Message::Delta { path, mode, mtime_nanos, data }) => {
                let target = resolve_relative(&root, &path)?;
                // Reading the base, applying the delta and writing the result
                // is whole-file IO on both ends plus the patch itself.
                off_runtime("applying a delta", move || {
                    let base = std::fs::read(&target).unwrap_or_default();
                    let mut out = Vec::new();
                    apply(&base, &data, &mut out)
                        .map_err(|e| DriveError::Transfer { message: format!("apply delta for {path}: {e}") })?;
                    write_temp_then_rename(&target, &out, mode, mtime_nanos)
                })
                .await?;
            }
            Some(Message::Rename { from, to, mode, mtime_nanos }) => {
                let src = resolve_relative(&root, &from)?;
                let dst = resolve_relative(&root, &to)?;
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|source| DriveError::Io { path: parent.to_path_buf(), source })?;
                }
                // A rename is only ever the sender's *guess* that two files
                // with equal content are the same file moved. A guess that
                // misses — the source was already consumed by an earlier
                // rename this pass, or deleted underneath us — must not take
                // the whole volume down with it: returning here drops the
                // connection mid-stream, the sender reports a bare "Broken
                // pipe", and the volume then fails every pass forever. Skip
                // it instead; the file is missing from the diff next pass and
                // gets sent whole.
                if let Err(source) = std::fs::rename(&src, &dst) {
                    if source.kind() == io::ErrorKind::NotFound {
                        warn!(%from, %to, "skipping a rename whose source is gone");
                    } else {
                        return Err(DriveError::Io { path: dst, source });
                    }
                } else {
                    std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(mode))
                        .map_err(|source| DriveError::Io { path: dst.clone(), source })?;
                    let mtime = scan::nanos_to_filetime(mtime_nanos);
                    filetime::set_file_times(&dst, mtime, mtime)
                        .map_err(|source| DriveError::Io { path: dst, source })?;
                }
            }
            Some(Message::MakeDir { path, mode }) => {
                let target = resolve_relative(&root, &path)?;
                off_runtime("creating a directory", move || {
                    std::fs::create_dir_all(&target)
                        .map_err(|source| DriveError::Io { path: target.clone(), source })?;
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                        .map_err(|source| DriveError::Io { path: target, source })
                })
                .await?;
            }
            Some(Message::RemoveDir(path)) => {
                let target = resolve_relative(&root, &path)?;
                // Non-recursive and best effort on purpose: `remove_dir` only
                // succeeds on an empty directory, so this can never take out a
                // tree the sender still has files in, and a directory that is
                // busy (a mount point, say) is left alone rather than failing
                // the whole sync.
                off_runtime("removing a directory", move || {
                    if let Err(e) = std::fs::remove_dir(&target) {
                        debug!(path = %target.display(), error = %e, "could not remove a directory");
                    }
                    Ok(())
                })
                .await?;
            }
            Some(Message::Delete(path)) => {
                let target = resolve_relative(&root, &path)?;
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => return Err(DriveError::Io { path: target, source }),
                }
            }
            Some(Message::Done) => {
                send(&mut conn, &Message::Ack(generation)).await.map_err(io_err)?;
                break;
            }
            Some(other) => warn!(?other, "unexpected message on drive transport"),
            None => break,
        }
    }

    Ok(())
}

/// Ask a peer for its manifest without starting a sync. Used by migration
/// verification to compare source and target after the final delta.
pub async fn request_manifest(peer: SocketAddr, volume: &VolumeId, hash_content: bool) -> Result<Manifest> {
    let stream = tokio::time::timeout(crate::delta::CONNECT_TIMEOUT, TcpStream::connect(peer))
        .await
        .map_err(|_| DriveError::Transfer { message: format!("connecting to {peer} timed out") })?
        .map_err(io_err)?;
    let mut conn = frame(stream);
    send(&mut conn, &Message::ManifestRequest { volume: volume.clone(), hash_content })
        .await
        .map_err(io_err)?;
    match recv(&mut conn).await.map_err(io_err)? {
        Some(Message::Ready(manifest)) => Ok(manifest),
        Some(other) => Err(DriveError::Transfer { message: format!("expected ready, got {other:?}") }),
        None => Err(DriveError::Transfer { message: "peer closed before ready".into() }),
    }
}

/// A temp path next to `target`, on the same directory so the final rename
/// is same-filesystem and therefore atomic.
fn temp_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.tmp-{}", std::process::id()))
}

/// Write `data` to a temp file next to `target`, then rename into place.
/// The rename means an interrupted transfer never leaves a half written
/// file visible under the real name.
///
/// `mtime_nanos` is the sender's original mtime, applied to the temp file
/// before the rename: leaving this at "now" (the write time) would make the
/// receiver's next scan see a fresh mtime that never matches the sender's,
/// so `Manifest::diff` would keep reporting the file changed and re-sync it
/// every pass forever.
fn write_temp_then_rename(target: &Path, data: &[u8], mode: u32, mtime_nanos: i64) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DriveError::Io { path: parent.to_path_buf(), source })?;
    }
    let tmp = temp_path(target);
    std::fs::write(&tmp, data).map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
    let mtime = scan::nanos_to_filetime(mtime_nanos);
    filetime::set_file_times(&tmp, mtime, mtime).map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
    std::fs::rename(&tmp, target).map_err(|source| DriveError::Io { path: target.to_path_buf(), source })?;
    Ok(())
}

fn write_whole(
    root: &Path,
    rel: &str,
    mode: u32,
    mtime_nanos: i64,
    is_symlink: bool,
    link_target: Option<&str>,
    data: &[u8],
) -> Result<()> {
    let target = resolve_relative(root, rel)?;

    if !is_symlink {
        return write_temp_then_rename(&target, data, mode, mtime_nanos);
    }

    let link_target = link_target.ok_or_else(|| DriveError::Transfer {
        message: format!("symlink {rel} sent with no target"),
    })?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DriveError::Io { path: parent.to_path_buf(), source })?;
    }
    let tmp = temp_path(&target);
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(link_target, &tmp).map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
    std::fs::rename(&tmp, &target).map_err(|source| DriveError::Io { path: target, source })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_round_trip(msg: Message) {
        let raw = bincode::serialize(&msg).unwrap();
        let decoded: Message = bincode::deserialize(&raw).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn every_message_variant_round_trips_through_bincode() {
        msg_round_trip(Message::Hello { volume: VolumeId::new("fast", "data"), generation: 3 });
        msg_round_trip(Message::Ready(Manifest::default()));
        msg_round_trip(Message::ManifestRequest { volume: VolumeId::new("fast", "data"), hash_content: true });
        msg_round_trip(Message::SignatureRequest("a.txt".into()));
        msg_round_trip(Message::Signature { path: "a.txt".into(), data: vec![1, 2, 3] });
        msg_round_trip(Message::HashRequest("a.txt".into()));
        msg_round_trip(Message::Hash { path: "a.txt".into(), hash: Some([9u8; 32]) });
        msg_round_trip(Message::Hash { path: "missing.txt".into(), hash: None });
        msg_round_trip(Message::WholeFile {
            path: "a.txt".into(),
            mode: 0o644,
            mtime_nanos: 123,
            is_symlink: false,
            link_target: None,
            data: b"hello".to_vec(),
        });
        msg_round_trip(Message::WholeFileBegin { path: "a.txt".into(), mode: 0o644, mtime_nanos: 123 });
        msg_round_trip(Message::WholeFileChunk { data: vec![1, 2, 3] });
        msg_round_trip(Message::WholeFileEnd);
        msg_round_trip(Message::Delta { path: "a.txt".into(), mode: 0o644, mtime_nanos: 123, data: vec![4, 5] });
        msg_round_trip(Message::Rename {
            from: "a.txt".into(),
            to: "b.txt".into(),
            mode: 0o644,
            mtime_nanos: 123,
        });
        msg_round_trip(Message::Delete("a.txt".into()));
        msg_round_trip(Message::Done);
        msg_round_trip(Message::Ack(7));
        msg_round_trip(Message::Error("boom".into()));
    }

    #[test]
    fn drive_port_is_offset_from_rpc_port() {
        assert_eq!(drive_port(7373), 7374);
    }

    #[test]
    fn temp_path_stays_alongside_the_target_and_is_hidden() {
        let target = Path::new("/vol/data/file.txt");
        let tmp = temp_path(target);
        assert_eq!(tmp.parent(), target.parent());
        assert!(tmp.file_name().unwrap().to_str().unwrap().starts_with('.'));
    }
}

#[cfg(test)]
mod port_tests {
    use super::*;

    #[test]
    fn the_bulk_transfer_port_sits_just_above_the_rpc_port() {
        assert_eq!(drive_port(7373), 7374);
    }
}
