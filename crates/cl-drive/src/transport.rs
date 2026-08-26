//! Wire protocol for bulk volume transfer.
//!
//! Kept off the raft rpc channel so a multi-megabyte delta never sits behind
//! a heartbeat, but framed the same way as `cl_raft::rpc` (length delimited
//! json over plain TCP) for consistency with the rest of the internal
//! network. A binary encoding would pack file bytes tighter; on a trusted,
//! unencrypted LAN that isn't worth the extra dependency.
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

use crate::error::{DriveError, Result};
use crate::registry::{resolve_relative, DriveRegistry};
use crate::scan::{self, Manifest};

/// Deltas and whole files can be large; refuse anything absurd so a corrupt
/// length prefix cannot make us allocate wildly.
const MAX_FRAME: usize = 256 * 1024 * 1024;

/// Files at or below this size go over the wire as a single `WholeFile`
/// message. Above it, `send_whole` streams `WholeFileBegin`/`WholeFileChunk`/
/// `WholeFileEnd` instead: `serde_json` encodes a `Vec<u8>` as a JSON array of
/// integers, not base64, so a single frame's payload can bloat to several
/// times the raw byte count. A postgres relation file comfortably exceeds
/// `MAX_FRAME` once encoded that way, which used to abort the whole sync
/// partway through and leave the receiver with a half-written volume (e.g. a
/// PGDATA directory missing everything alphabetically after the oversized
/// file). Chosen with enough headroom under `MAX_FRAME` that the JSON
/// encoding of one chunk, plus its message envelope, never gets close.
pub const WHOLE_FILE_CHUNK: usize = 32 * 1024 * 1024;

/// rpc_port + 1. Keeps bulk transfer off the raft socket without adding a
/// second port to the config file.
pub fn drive_port(rpc_port: u16) -> u16 {
    rpc_port.wrapping_add(1)
}

/// Port a served drive's server listens on, `rpc_port + 2 + index`.
///
/// The served protocol carries no drive name, so one listener serves exactly
/// one drive and each needs its own port. `index` is the drive's position in
/// the cluster config's drive map, which is ordered by name and replicated, so
/// every node works out the same port for the same drive without another
/// config field to keep in step.
pub fn served_port(rpc_port: u16, index: usize) -> u16 {
    rpc_port.wrapping_add(2).wrapping_add(index as u16)
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
        is_symlink: bool,
        link_target: Option<String>,
        data: Vec<u8>,
    },
    /// Starts a chunked whole-file transfer for a regular file too large to
    /// send as one `WholeFile` frame. Followed by one or more
    /// `WholeFileChunk`s and a final `WholeFileEnd`.
    WholeFileBegin { path: String, mode: u32 },
    WholeFileChunk { data: Vec<u8> },
    WholeFileEnd,
    Delta { path: String, mode: u32, data: Vec<u8> },
    Rename { from: String, to: String },
    Delete(String),
    /// Sender is done; receiver replies with `Ack`.
    Done,
    Ack(u64),
    Error(String),
}

pub type Connection = Framed<TcpStream, LengthDelimitedCodec>;

pub fn frame(stream: TcpStream) -> Connection {
    let _ = stream.set_nodelay(true);
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_framed(stream)
}

pub async fn send(conn: &mut Connection, msg: &Message) -> io::Result<()> {
    let raw = serde_json::to_vec(msg).map_err(io::Error::other)?;
    conn.send(bytes::Bytes::from(raw)).await
}

/// `Ok(None)` means the peer closed the connection cleanly.
pub async fn recv(conn: &mut Connection) -> io::Result<Option<Message>> {
    match conn.next().await {
        Some(Ok(raw)) => Ok(Some(serde_json::from_slice(&raw).map_err(io::Error::other)?)),
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

    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = receive(stream, &registry).await {
                debug!(%peer, error = %e, "drive transfer connection closed");
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
            let manifest = scan::scan(&root, hash_content)?;
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
    let manifest = scan::scan(&root, false)?;
    send(&mut conn, &Message::Ready(manifest)).await.map_err(io_err)?;

    // State for a `WholeFileBegin`/`WholeFileChunk`*/`WholeFileEnd` sequence
    // in progress: the final target, the temp file it's being written to,
    // its mode, and the open handle chunks are appended to.
    let mut chunked: Option<(PathBuf, PathBuf, u32, std::fs::File)> = None;

    loop {
        match recv(&mut conn).await.map_err(io_err)? {
            Some(Message::WholeFileBegin { path, mode }) => {
                let target = resolve_relative(&root, &path)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|source| DriveError::Io { path: parent.to_path_buf(), source })?;
                }
                let tmp = temp_path(&target);
                let file = std::fs::File::create(&tmp).map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
                chunked = Some((target, tmp, mode, file));
            }
            Some(Message::WholeFileChunk { data }) => {
                let (_, tmp, _, file) = chunked.as_mut().ok_or_else(|| DriveError::Transfer {
                    message: "WholeFileChunk with no WholeFileBegin in progress".into(),
                })?;
                file.write_all(&data).map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
            }
            Some(Message::WholeFileEnd) => {
                let (target, tmp, mode, file) = chunked.take().ok_or_else(|| DriveError::Transfer {
                    message: "WholeFileEnd with no WholeFileBegin in progress".into(),
                })?;
                drop(file);
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
                    .map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
                std::fs::rename(&tmp, &target).map_err(|source| DriveError::Io { path: target, source })?;
            }
            Some(Message::SignatureRequest(path)) => {
                let target = resolve_relative(&root, &path)?;
                let data = std::fs::read(&target).unwrap_or_default();
                let sig = Signature::calculate(&data, signature_options());
                send(&mut conn, &Message::Signature { path, data: sig.into_serialized() })
                    .await
                    .map_err(io_err)?;
            }
            Some(Message::HashRequest(path)) => {
                let target = resolve_relative(&root, &path)?;
                let hash = std::fs::read(&target).ok().map(|data| *blake3::hash(&data).as_bytes());
                send(&mut conn, &Message::Hash { path, hash }).await.map_err(io_err)?;
            }
            Some(Message::WholeFile { path, mode, is_symlink, link_target, data }) => {
                write_whole(&root, &path, mode, is_symlink, link_target.as_deref(), &data)?;
            }
            Some(Message::Delta { path, mode, data }) => {
                let target = resolve_relative(&root, &path)?;
                let base = std::fs::read(&target).unwrap_or_default();
                let mut out = Vec::new();
                apply(&base, &data, &mut out)
                    .map_err(|e| DriveError::Transfer { message: format!("apply delta for {path}: {e}") })?;
                write_temp_then_rename(&target, &out, mode)?;
            }
            Some(Message::Rename { from, to }) => {
                let src = resolve_relative(&root, &from)?;
                let dst = resolve_relative(&root, &to)?;
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|source| DriveError::Io { path: parent.to_path_buf(), source })?;
                }
                std::fs::rename(&src, &dst).map_err(|source| DriveError::Io { path: dst, source })?;
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
fn write_temp_then_rename(target: &Path, data: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DriveError::Io { path: parent.to_path_buf(), source })?;
    }
    let tmp = temp_path(target);
    std::fs::write(&tmp, data).map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|source| DriveError::Io { path: tmp.clone(), source })?;
    std::fs::rename(&tmp, target).map_err(|source| DriveError::Io { path: target.to_path_buf(), source })?;
    Ok(())
}

fn write_whole(
    root: &Path,
    rel: &str,
    mode: u32,
    is_symlink: bool,
    link_target: Option<&str>,
    data: &[u8],
) -> Result<()> {
    let target = resolve_relative(root, rel)?;

    if !is_symlink {
        return write_temp_then_rename(&target, data, mode);
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
        let raw = serde_json::to_vec(&msg).unwrap();
        let decoded: Message = serde_json::from_slice(&raw).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn every_message_variant_round_trips_through_json() {
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
            is_symlink: false,
            link_target: None,
            data: b"hello".to_vec(),
        });
        msg_round_trip(Message::WholeFileBegin { path: "a.txt".into(), mode: 0o644 });
        msg_round_trip(Message::WholeFileChunk { data: vec![1, 2, 3] });
        msg_round_trip(Message::WholeFileEnd);
        msg_round_trip(Message::Delta { path: "a.txt".into(), mode: 0o644, data: vec![4, 5] });
        msg_round_trip(Message::Rename { from: "a.txt".into(), to: "b.txt".into() });
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

    #[test]
    fn each_served_drive_gets_its_own_port_above_that() {
        assert_eq!(served_port(7373, 0), 7375);
        assert_eq!(served_port(7373, 1), 7376);
        // Never collides with the raft or bulk transfer ports.
        assert!(served_port(7373, 0) > drive_port(7373));
    }
}
