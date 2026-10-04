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

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fast_rsync::{apply, Signature, SignatureOptions};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{debug, warn};

use cl_raft::{KeyWatch, VolumeId};
use cl_secret::stream_auth;

use crate::delta::off_runtime;
use crate::error::{DriveError, Result};
use crate::registry::{resolve_no_follow, DriveRegistry};
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

/// How long a connection may take to send its first frame. A real sender
/// writes `Hello` the instant it connects, so this only ever fires on a
/// connection that holds one of the `MAX_CONCURRENT_RECEIVES` slots while
/// doing nothing.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connection may go silent between frames once a sync is under
/// way. This is far longer than any network stall would warrant on purpose:
/// the sender legitimately says nothing while it scans its own volume after
/// `Ready`, hashes every rename candidate (whole files, possibly many
/// gigabytes) and computes a delta of a large file, all before the next frame
/// goes out. A tight timeout would turn those into failed syncs; this one
/// still frees the slot of a peer that has simply gone away.
const RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
    /// `staging`, when set, marks a `NodeLocal`-elsewhere backup's one-off
    /// push (see `backup_runner`'s module doc) and carries the backup
    /// destination's own configured path: the receiver stages the volume
    /// under `DriveRegistry::ensure_staging_volume`, in a hidden directory
    /// next to that path, instead of requiring it already implement the
    /// drive. Colocating with the destination (rather than staging under
    /// this node's data dir) keeps the staged copy on whatever volume the
    /// destination itself lives on. Nothing else in this protocol cares -
    /// added here rather than as a new `Message` variant since only the
    /// initial handshake needs to say it.
    Hello {
        volume: VolumeId,
        generation: u64,
        staging: Option<String>,
    },
    /// Receiver's current manifest, scanned without hashing.
    Ready(Manifest),
    /// A one-shot manifest request outside a sync, used for migration
    /// verification. Answered with `Ready` and the connection closed.
    ManifestRequest {
        volume: VolumeId,
        hash_content: bool,
    },
    SignatureRequest(String),
    Signature {
        path: String,
        data: Vec<u8>,
    },
    HashRequest(String),
    /// `None` when the receiver has no such file, e.g. a stale rename guess.
    Hash {
        path: String,
        hash: Option<[u8; 32]>,
    },
    WholeFile {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
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
    WholeFileBegin {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
        mtime_nanos: i64,
    },
    WholeFileChunk {
        data: Vec<u8>,
    },
    WholeFileEnd,
    Delta {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
        mtime_nanos: i64,
        data: Vec<u8>,
    },
    /// Move `from` to `to` on the receiver, then stamp the sender's `mode`,
    /// ownership and `mtime_nanos` on it.
    ///
    /// The metadata is not decoration. A rename moves the receiver's existing
    /// file, which carries whatever mode, ownership and mtime the *source*
    /// had when it was synced — not the ones the sender's `to` has now.
    /// Without restamping, `Manifest::diff` sees a mismatch on the very next
    /// pass and re-sends the file whole, which is exactly the work the
    /// rename existed to avoid. Same reasoning as `WholeFile::mtime_nanos`.
    Rename {
        from: String,
        to: String,
        mode: u32,
        uid: u32,
        gid: u32,
        mtime_nanos: i64,
    },
    Delete(String),
    /// Sender is done; receiver replies with `Ack`.
    Done,
    Ack(u64),
    Error(String),
    /// Create a directory (and any missing parents) with these permissions
    /// and ownership. Sent for directories the peer is missing, ahead of any
    /// file, so an *empty* directory survives replication — nothing else
    /// would create one.
    ///
    /// New variants go at the end: `Message` is encoded with bincode, which
    /// writes the variant's index and is not self-describing, so inserting
    /// anywhere else silently renumbers every variant after it.
    MakeDir {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    /// Remove a directory the sender no longer has. Best effort and
    /// non-recursive on the receiving side.
    RemoveDir(String),
    /// Restamp a directory the peer already has whose mode, uid or gid
    /// diverged — an existing directory whose ownership changed underneath
    /// it, not a new one.
    SetDirMeta {
        path: String,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    /// Restamp the volume root itself. The root is never "added" or
    /// "removed" like an ordinary directory, so its own permissions and
    /// ownership travel over their own message.
    SetRootMeta {
        mode: u32,
        uid: u32,
        gid: u32,
    },
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
    DriveError::Transfer {
        message: e.to_string(),
    }
}

/// Scopes the stream handshake to this protocol, so a proof captured on the
/// raft rpc channel cannot open this one.
pub const AUTH_LABEL: &str = "drive";

/// How many connections may be mid-handshake at once. The handshake itself is
/// bounded to `HANDSHAKE_TIMEOUT`, but without a cap a flood of connections
/// that never answer would each hold a task and a socket for that long. Real
/// peers handshake in milliseconds, so a small cap costs them nothing; when it
/// is full the excess connections are dropped rather than queued, since a
/// queued unauthenticated connection is exactly the resource being protected.
const MAX_PENDING_HANDSHAKES: usize = 64;

/// Accept connections and run the receive side of the sync protocol until
/// the task is cancelled.
///
/// The listener is reachable by anything that can route to the internal
/// address, and what it serves can read and overwrite any volume, so every
/// connection must prove it holds the cluster key before a single frame is
/// decoded. `keys` is read per connection so a rotated key takes effect
/// without restarting the listener; a node with no key refuses everything.
pub async fn serve(
    addr: SocketAddr,
    registry: Arc<DriveRegistry>,
    keys: KeyWatch,
) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    debug!(%addr, "drive transport listening");
    serve_listener(listener, registry, keys).await
}

/// `serve` on an already bound listener, so tests can learn the address
/// before connecting.
pub async fn serve_listener(
    listener: TcpListener,
    registry: Arc<DriveRegistry>,
    keys: KeyWatch,
) -> io::Result<()> {
    // Every node that replicates a drive here pushes on its own schedule, and
    // a transfer's working set is bounded per connection but not across them.
    // Unlimited, one sync pass per peer arriving at the same moment multiplies
    // that by the size of the cluster — which on a small node is an OOM kill,
    // reported to each sender as a bare "Connection reset by peer". Senders
    // that wait simply take longer; the sync interval is minutes.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_RECEIVES));
    let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES));

    loop {
        let (mut stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        let slots = slots.clone();
        let keys = keys.clone();
        let Ok(handshake_slot) = handshakes.clone().try_acquire_owned() else {
            warn!(%peer, "too many drive connections mid-handshake, dropping this one");
            continue;
        };
        tokio::spawn(async move {
            let key = keys.borrow().clone();
            let Some(key) = key else {
                debug!(%peer, "refusing drive connection, this node holds no cluster key");
                return;
            };
            // Authenticate before taking a transfer slot, so a peer that never
            // proves itself cannot starve the real senders of slots.
            if let Err(e) = stream_auth::accept(&mut stream, &key, AUTH_LABEL).await {
                warn!(%peer, error = %e, "drive peer failed authentication, dropping the connection");
                return;
            }
            drop(handshake_slot);

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

/// A fixed key watch loaded from the configured key file, for the one-shot
/// CLI senders (`folder-import`) that have no daemon channel to borrow. Mirrors
/// what `cl_raft::client::client_for` does for the rpc channel.
pub fn key_watch_from_config(cfg: &cl_config::ClusterConfig) -> Result<KeyWatch> {
    let key = cl_secret::ClusterKey::load(&cfg.secrets.key_file).map_err(|e| {
        DriveError::Transfer {
            message: format!(
                "the drive transport needs the cluster key, and {} cannot be read: {e}. Run this from a node that holds the key",
                cfg.secrets.key_file.display()
            ),
        }
    })?;
    Ok(cl_raft::fixed_key(Arc::new(key)))
}

/// Dial a peer's drive listener and prove we hold the cluster key before
/// framing, so no manifest or file is ever sent to something that is not a
/// member. Shared by every sender (sync, migration verify, the CLIs).
pub async fn connect_authenticated(peer: SocketAddr, keys: &KeyWatch) -> Result<Connection> {
    // No key means we cannot prove membership and the listener would refuse
    // us anyway; failing here says why instead of a bare reset.
    let key = keys.borrow().clone().ok_or_else(|| DriveError::Transfer {
        message: "this node holds no cluster key, so it cannot authenticate to the drive transport"
            .into(),
    })?;
    let mut stream = tokio::time::timeout(crate::delta::CONNECT_TIMEOUT, TcpStream::connect(peer))
        .await
        .map_err(|_| DriveError::Transfer {
            message: format!("connecting to {peer} timed out"),
        })?
        .map_err(io_err)?;
    stream_auth::connect(&mut stream, &key, AUTH_LABEL)
        .await
        .map_err(|e| DriveError::Transfer {
            message: format!("authenticating to {peer}: {e}"),
        })?;
    Ok(frame(stream))
}

/// `recv`, bounded: a peer that stops talking must not hold a receive slot
/// forever.
async fn recv_within(
    conn: &mut Connection,
    limit: Duration,
    what: &str,
) -> Result<Option<Message>> {
    match tokio::time::timeout(limit, recv(conn)).await {
        Ok(msg) => msg.map_err(io_err),
        Err(_) => Err(DriveError::Transfer {
            message: format!("timed out waiting for {what}"),
        }),
    }
}

/// A chunked whole-file transfer in progress: the final target, the temp file
/// being written, the metadata to stamp on it and the open handle chunks are
/// appended to.
///
/// Owns the temp file's lifetime: unless `armed` is cleared after the final
/// rename, dropping this removes it. Every way out of `receive` other than a
/// completed `WholeFileEnd` (a second `WholeFileBegin` replacing it, a
/// dropped connection, a failed write, a timeout) just drops the state, so
/// none of them can leave a half written `.tmp` file behind forever.
struct ChunkedWrite {
    target: PathBuf,
    tmp: PathBuf,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_nanos: i64,
    file: Option<std::fs::File>,
    armed: bool,
}

impl ChunkedWrite {
    /// The file is in place; there is nothing left to clean up.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChunkedWrite {
    fn drop(&mut self) {
        self.file = None;
        if self.armed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Run the receive side of one sync over an already accepted connection.
/// The stream must already have passed `stream_auth::accept`: `serve` does
/// that, and tests that call this directly have to do it themselves. Exposed
/// so tests can drive it over a loopback pair without a listener.
pub async fn receive(stream: TcpStream, registry: &DriveRegistry) -> Result<()> {
    let mut conn = frame(stream);

    let (volume, generation, staging) =
        match recv_within(&mut conn, FIRST_FRAME_TIMEOUT, "hello").await? {
            Some(Message::Hello {
                volume,
                generation,
                staging,
            }) => (volume, generation, staging),
            Some(Message::ManifestRequest {
                volume,
                hash_content,
            }) => {
                let root = registry.ensure_volume(&volume)?;
                let manifest = scan::scan_off_runtime(root, hash_content).await?;
                send(&mut conn, &Message::Ready(manifest))
                    .await
                    .map_err(io_err)?;
                return Ok(());
            }
            Some(other) => {
                return Err(DriveError::Transfer {
                    message: format!("expected hello, got {other:?}"),
                })
            }
            None => return Ok(()),
        };

    let root = match &staging {
        // `ensure_staging_volume` refuses a path that is not a backup
        // destination configured for this node.
        Some(destination_path) => {
            registry.ensure_staging_volume(Path::new(destination_path), &volume)?
        }
        None => registry.ensure_volume(&volume)?,
    };
    let manifest = scan::scan_off_runtime(root.clone(), false).await?;
    send(&mut conn, &Message::Ready(manifest))
        .await
        .map_err(io_err)?;

    let mut chunked: Option<ChunkedWrite> = None;

    loop {
        match recv_within(&mut conn, RECV_IDLE_TIMEOUT, "the next frame").await? {
            Some(Message::WholeFileBegin {
                path,
                mode,
                uid,
                gid,
                mtime_nanos,
            }) => {
                // Drop (and so delete the temp file of) a transfer that was
                // never finished before starting the next. Done first: for
                // the same target both would use the same temp name.
                drop(chunked.take());
                let target = resolve_no_follow(&root, &path)?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| DriveError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                let tmp = temp_path(&target);
                let file = create_temp(&tmp)?;
                chunked = Some(ChunkedWrite {
                    target,
                    tmp,
                    mode,
                    uid,
                    gid,
                    mtime_nanos,
                    file: Some(file),
                    armed: true,
                });
            }
            Some(Message::WholeFileChunk { data }) => {
                let mut state = chunked.take().ok_or_else(|| DriveError::Transfer {
                    message: "WholeFileChunk with no WholeFileBegin in progress".into(),
                })?;
                // Hand the open handle to the blocking pool and take it back,
                // so writing a chunk of a multi-gigabyte file never runs on a
                // runtime thread.
                chunked = Some(
                    off_runtime("writing a chunk", move || {
                        let file = state.file.as_mut().expect("file is open until the end");
                        file.write_all(&data).map_err(|source| DriveError::Io {
                            path: state.tmp.clone(),
                            source,
                        })?;
                        Ok(state)
                    })
                    .await?,
                );
            }
            Some(Message::WholeFileEnd) => {
                let mut state = chunked.take().ok_or_else(|| DriveError::Transfer {
                    message: "WholeFileEnd with no WholeFileBegin in progress".into(),
                })?;
                off_runtime("finishing a chunked file", move || {
                    let file = state.file.take().expect("file is open until the end");
                    stamp_open_file(
                        &file,
                        &state.tmp,
                        state.mode,
                        state.uid,
                        state.gid,
                        state.mtime_nanos,
                    )?;
                    drop(file);
                    std::fs::rename(&state.tmp, &state.target).map_err(|source| {
                        DriveError::Io {
                            path: state.target.clone(),
                            source,
                        }
                    })?;
                    state.disarm();
                    Ok(())
                })
                .await?;
            }
            Some(Message::SignatureRequest(path)) => {
                let target = resolve_no_follow(&root, &path)?;
                // Reading the file and building its rolling-hash signature is
                // whole-file IO plus CPU.
                let data = off_runtime("building a signature", move || {
                    let data = read_no_follow(&target).unwrap_or_default();
                    Ok(Signature::calculate(&data, signature_options()).into_serialized())
                })
                .await?;
                send(&mut conn, &Message::Signature { path, data })
                    .await
                    .map_err(io_err)?;
            }
            Some(Message::HashRequest(path)) => {
                let target = resolve_no_follow(&root, &path)?;
                // Streamed, not `fs::read`: a hash is requested for every
                // file the sender no longer has, whatever its size. A
                // symlink has no content of its own to hash and must not be
                // read through to whatever it points at.
                let hash = off_runtime("hashing a file", move || {
                    let is_link = std::fs::symlink_metadata(&target)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false);
                    Ok(if is_link {
                        None
                    } else {
                        scan::hash_file(&target).ok()
                    })
                })
                .await?;
                send(&mut conn, &Message::Hash { path, hash })
                    .await
                    .map_err(io_err)?;
            }
            Some(Message::WholeFile {
                path,
                mode,
                uid,
                gid,
                mtime_nanos,
                is_symlink,
                link_target,
                data,
            }) => {
                let root = root.clone();
                off_runtime("writing a whole file", move || {
                    write_whole(
                        &root,
                        &path,
                        mode,
                        uid,
                        gid,
                        mtime_nanos,
                        is_symlink,
                        link_target.as_deref(),
                        &data,
                    )
                })
                .await?;
            }
            Some(Message::Delta {
                path,
                mode,
                uid,
                gid,
                mtime_nanos,
                data,
            }) => {
                let target = resolve_no_follow(&root, &path)?;
                // Reading the base, applying the delta and writing the result
                // is whole-file IO on both ends plus the patch itself.
                off_runtime("applying a delta", move || {
                    let base = read_no_follow(&target).unwrap_or_default();
                    let mut out = Vec::new();
                    apply(&base, &data, &mut out).map_err(|e| DriveError::Transfer {
                        message: format!("apply delta for {path}: {e}"),
                    })?;
                    write_temp_then_rename(&target, &out, mode, uid, gid, mtime_nanos)
                })
                .await?;
            }
            Some(Message::Rename {
                from,
                to,
                mode,
                uid,
                gid,
                mtime_nanos,
            }) => {
                let src = resolve_no_follow(&root, &from)?;
                let dst = resolve_no_follow(&root, &to)?;
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).map_err(|source| DriveError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
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
                    stamp_path_no_follow(&dst, mode, uid, gid, mtime_nanos)?;
                }
            }
            Some(Message::MakeDir {
                path,
                mode,
                uid,
                gid,
            }) => {
                let target = resolve_no_follow(&root, &path)?;
                let root = root.clone();
                off_runtime("creating a directory", move || {
                    reject_symlink(&root, &target, &path)?;
                    std::fs::create_dir_all(&target).map_err(|source| DriveError::Io {
                        path: target.clone(),
                        source,
                    })?;
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                        .map_err(|source| DriveError::Io {
                            path: target.clone(),
                            source,
                        })?;
                    std::os::unix::fs::chown(&target, Some(uid), Some(gid)).map_err(|source| {
                        DriveError::Io {
                            path: target,
                            source,
                        }
                    })
                })
                .await?;
            }
            Some(Message::SetDirMeta {
                path,
                mode,
                uid,
                gid,
            }) => {
                let target = resolve_no_follow(&root, &path)?;
                let root = root.clone();
                off_runtime("restamping a directory", move || {
                    // `set_permissions` and `chown` follow a final symlink,
                    // which would restamp whatever it points at.
                    reject_symlink(&root, &target, &path)?;
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                        .map_err(|source| DriveError::Io {
                            path: target.clone(),
                            source,
                        })?;
                    std::os::unix::fs::chown(&target, Some(uid), Some(gid)).map_err(|source| {
                        DriveError::Io {
                            path: target,
                            source,
                        }
                    })
                })
                .await?;
            }
            Some(Message::SetRootMeta { mode, uid, gid }) => {
                let root = root.clone();
                off_runtime("restamping the volume root", move || {
                    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(mode))
                        .map_err(|source| DriveError::Io {
                            path: root.clone(),
                            source,
                        })?;
                    std::os::unix::fs::chown(&root, Some(uid), Some(gid))
                        .map_err(|source| DriveError::Io { path: root, source })
                })
                .await?;
            }
            Some(Message::RemoveDir(path)) => {
                let target = resolve_no_follow(&root, &path)?;
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
                let target = resolve_no_follow(&root, &path)?;
                match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(DriveError::Io {
                            path: target,
                            source,
                        })
                    }
                }
            }
            Some(Message::Done) => {
                send(&mut conn, &Message::Ack(generation))
                    .await
                    .map_err(io_err)?;
                break;
            }
            // Nothing a well behaved sender emits here, so whatever is on
            // the other end is confused or hostile: end the connection
            // rather than keep serving it.
            Some(other) => {
                return Err(DriveError::Transfer {
                    message: format!("unexpected message on drive transport: {other:?}"),
                })
            }
            None => break,
        }
    }

    Ok(())
}

/// Ask a peer for its manifest without starting a sync. Used by migration
/// verification to compare source and target after the final delta.
pub async fn request_manifest(
    peer: SocketAddr,
    volume: &VolumeId,
    hash_content: bool,
    keys: &KeyWatch,
) -> Result<Manifest> {
    let mut conn = connect_authenticated(peer, keys).await?;
    send(
        &mut conn,
        &Message::ManifestRequest {
            volume: volume.clone(),
            hash_content,
        },
    )
    .await
    .map_err(io_err)?;
    match recv(&mut conn).await.map_err(io_err)? {
        Some(Message::Ready(manifest)) => Ok(manifest),
        Some(other) => Err(DriveError::Transfer {
            message: format!("expected ready, got {other:?}"),
        }),
        None => Err(DriveError::Transfer {
            message: "peer closed before ready".into(),
        }),
    }
}

/// A temp path next to `target`, on the same directory so the final rename
/// is same-filesystem and therefore atomic.
fn temp_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.tmp-{}", std::process::id()))
}

/// Create the temp file fresh. A leftover from a crashed run is removed
/// first (`remove_file` unlinks a symlink rather than following it), and the
/// open itself refuses to follow a link or reuse an existing file, so a
/// symlink planted at the temp name between the two cannot redirect the write
/// elsewhere. Mode is tightened to 0600 until the real mode is stamped.
fn create_temp(tmp: &Path) -> Result<std::fs::File> {
    let _ = std::fs::remove_file(tmp);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(tmp)
        .map_err(|source| DriveError::Io {
            path: tmp.to_path_buf(),
            source,
        })
}

/// Read a file without following a symlink in its final component, which
/// `std::fs::read` would. Intermediate components are the caller's concern
/// (`resolve_no_follow`).
fn read_no_follow(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

/// Refuse to act on `target` if it is itself a symlink, for operations whose
/// underlying syscalls would follow it. `target == root` is exempt: the drive
/// root is trusted, and `"."` resolves to it.
fn reject_symlink(root: &Path, target: &Path, rel: &str) -> Result<()> {
    if target == root {
        return Ok(());
    }
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => Err(DriveError::PathEscape {
            path: rel.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Stamp mode, ownership and mtime through an open handle, so nothing here
/// resolves a path again and a swapped-in symlink cannot redirect it.
fn stamp_open_file(
    file: &std::fs::File,
    tmp: &Path,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_nanos: i64,
) -> Result<()> {
    let io = |source| DriveError::Io {
        path: tmp.to_path_buf(),
        source,
    };
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io)?;
    std::os::unix::fs::fchown(file, Some(uid), Some(gid)).map_err(io)?;
    let mtime = scan::nanos_to_filetime(mtime_nanos);
    filetime::set_file_handle_times(file, Some(mtime), Some(mtime)).map_err(io)
}

/// Stamp metadata on a path that may be a symlink (a renamed link), without
/// ever following it: ownership via `lchown`, times via the symlink variant,
/// and no chmod at all since a link's own mode is meaningless on Linux.
fn stamp_path_no_follow(
    path: &Path,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_nanos: i64,
) -> Result<()> {
    let io = |source| DriveError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mtime = scan::nanos_to_filetime(mtime_nanos);
    let is_link = std::fs::symlink_metadata(path)
        .map_err(io)?
        .file_type()
        .is_symlink();
    if is_link {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid)).map_err(io)?;
        filetime::set_symlink_file_times(path, mtime, mtime).map_err(io)
    } else {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(io)?;
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(io)?;
        filetime::set_file_times(path, mtime, mtime).map_err(io)
    }
}

/// Write `data` to a temp file next to `target`, then rename into place.
/// The rename means an interrupted transfer never leaves a half written
/// file visible under the real name, and it replaces a symlink at `target`
/// rather than writing through it.
///
/// `mtime_nanos` is the sender's original mtime, applied to the temp file
/// before the rename: leaving this at "now" (the write time) would make the
/// receiver's next scan see a fresh mtime that never matches the sender's,
/// so `Manifest::diff` would keep reporting the file changed and re-sync it
/// every pass forever.
fn write_temp_then_rename(
    target: &Path,
    data: &[u8],
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_nanos: i64,
) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DriveError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = temp_path(target);
    let mut file = create_temp(&tmp)?;
    let result = file
        .write_all(data)
        .map_err(|source| DriveError::Io {
            path: tmp.clone(),
            source,
        })
        .and_then(|()| stamp_open_file(&file, &tmp, mode, uid, gid, mtime_nanos))
        .and_then(|()| {
            std::fs::rename(&tmp, target).map_err(|source| DriveError::Io {
                path: target.to_path_buf(),
                source,
            })
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn write_whole(
    root: &Path,
    rel: &str,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_nanos: i64,
    is_symlink: bool,
    link_target: Option<&str>,
    data: &[u8],
) -> Result<()> {
    let target = resolve_no_follow(root, rel)?;

    if !is_symlink {
        return write_temp_then_rename(&target, data, mode, uid, gid, mtime_nanos);
    }

    let link_target = link_target.ok_or_else(|| DriveError::Transfer {
        message: format!("symlink {rel} sent with no target"),
    })?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|source| DriveError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = temp_path(&target);
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(link_target, &tmp).map_err(|source| DriveError::Io {
        path: tmp.clone(),
        source,
    })?;
    // A symlink's own mode is meaningless on Linux (always 0o777) and
    // `set_permissions` follows the link rather than the symlink itself, so
    // only ownership is stamped here — with `lchown`, which does not follow.
    std::os::unix::fs::lchown(&tmp, Some(uid), Some(gid)).map_err(|source| DriveError::Io {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &target).map_err(|source| DriveError::Io {
        path: target,
        source,
    })?;
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
        msg_round_trip(Message::Hello {
            volume: VolumeId::new("fast", "data"),
            generation: 3,
            staging: None,
        });
        msg_round_trip(Message::Ready(Manifest::default()));
        msg_round_trip(Message::ManifestRequest {
            volume: VolumeId::new("fast", "data"),
            hash_content: true,
        });
        msg_round_trip(Message::SignatureRequest("a.txt".into()));
        msg_round_trip(Message::Signature {
            path: "a.txt".into(),
            data: vec![1, 2, 3],
        });
        msg_round_trip(Message::HashRequest("a.txt".into()));
        msg_round_trip(Message::Hash {
            path: "a.txt".into(),
            hash: Some([9u8; 32]),
        });
        msg_round_trip(Message::Hash {
            path: "missing.txt".into(),
            hash: None,
        });
        msg_round_trip(Message::WholeFile {
            path: "a.txt".into(),
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            mtime_nanos: 123,
            is_symlink: false,
            link_target: None,
            data: b"hello".to_vec(),
        });
        msg_round_trip(Message::WholeFileBegin {
            path: "a.txt".into(),
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            mtime_nanos: 123,
        });
        msg_round_trip(Message::WholeFileChunk {
            data: vec![1, 2, 3],
        });
        msg_round_trip(Message::WholeFileEnd);
        msg_round_trip(Message::Delta {
            path: "a.txt".into(),
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            mtime_nanos: 123,
            data: vec![4, 5],
        });
        msg_round_trip(Message::Rename {
            from: "a.txt".into(),
            to: "b.txt".into(),
            mode: 0o644,
            uid: 1000,
            gid: 1000,
            mtime_nanos: 123,
        });
        msg_round_trip(Message::Delete("a.txt".into()));
        msg_round_trip(Message::Done);
        msg_round_trip(Message::Ack(7));
        msg_round_trip(Message::Error("boom".into()));
        msg_round_trip(Message::MakeDir {
            path: "sub".into(),
            mode: 0o755,
            uid: 1000,
            gid: 1000,
        });
        msg_round_trip(Message::RemoveDir("sub".into()));
        msg_round_trip(Message::SetDirMeta {
            path: "sub".into(),
            mode: 0o750,
            uid: 1000,
            gid: 1000,
        });
        msg_round_trip(Message::SetRootMeta {
            mode: 0o700,
            uid: 1000,
            gid: 1000,
        });
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
mod receive_tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::MetadataExt;

    use tokio::task::JoinHandle;

    use super::*;

    struct Session {
        conn: Connection,
        done: JoinHandle<Result<()>>,
    }

    /// A loopback sync session against a registry whose only drive is
    /// `drive_root`, past the `Hello`/`Ready` handshake. Volume "v" is the
    /// returned root.
    async fn session(drive_root: &Path) -> (Session, PathBuf) {
        let registry = DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), drive_root.to_path_buf())]),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let key = Arc::new(cl_secret::ClusterKey::generate());
        let server_key = key.clone();
        let done = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // `receive` expects an authenticated stream, as `serve` gives it.
            stream_auth::accept(&mut stream, &server_key, AUTH_LABEL)
                .await
                .unwrap();
            receive(stream, &registry).await
        });
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream_auth::connect(&mut stream, &key, AUTH_LABEL)
            .await
            .unwrap();
        let mut conn = frame(stream);
        send(
            &mut conn,
            &Message::Hello {
                volume: VolumeId::new("fast", "v"),
                generation: 1,
                staging: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            recv(&mut conn).await.unwrap(),
            Some(Message::Ready(_))
        ));
        (Session { conn, done }, drive_root.join("v"))
    }

    fn me() -> (u32, u32) {
        // SAFETY: getuid/getgid have no preconditions and cannot fail.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    #[tokio::test]
    async fn a_symlinked_intermediate_directory_is_never_followed() {
        let (uid, gid) = me();
        let msgs: Vec<(&str, Message)> = vec![
            (
                "whole file",
                Message::WholeFile {
                    path: "a/new.txt".into(),
                    mode: 0o644,
                    uid,
                    gid,
                    mtime_nanos: 0,
                    is_symlink: false,
                    link_target: None,
                    data: b"pwn".to_vec(),
                },
            ),
            (
                "chunked begin",
                Message::WholeFileBegin {
                    path: "a/new.txt".into(),
                    mode: 0o644,
                    uid,
                    gid,
                    mtime_nanos: 0,
                },
            ),
            (
                "delta",
                Message::Delta {
                    path: "a/victim".into(),
                    mode: 0o644,
                    uid,
                    gid,
                    mtime_nanos: 0,
                    data: vec![],
                },
            ),
            ("delete", Message::Delete("a/victim".into())),
            (
                "rename from",
                Message::Rename {
                    from: "a/victim".into(),
                    to: "moved".into(),
                    mode: 0o644,
                    uid,
                    gid,
                    mtime_nanos: 0,
                },
            ),
            (
                "rename to",
                Message::Rename {
                    from: "plain".into(),
                    to: "a/moved".into(),
                    mode: 0o644,
                    uid,
                    gid,
                    mtime_nanos: 0,
                },
            ),
            (
                "make dir",
                Message::MakeDir {
                    path: "a/sub".into(),
                    mode: 0o755,
                    uid,
                    gid,
                },
            ),
            (
                "make dir through the link itself",
                Message::MakeDir {
                    path: "a".into(),
                    mode: 0o777,
                    uid,
                    gid,
                },
            ),
            (
                "dir meta through a link",
                Message::SetDirMeta {
                    path: "a/inner".into(),
                    mode: 0o777,
                    uid,
                    gid,
                },
            ),
            (
                "dir meta on the link itself",
                Message::SetDirMeta {
                    path: "a".into(),
                    mode: 0o777,
                    uid,
                    gid,
                },
            ),
            ("remove dir", Message::RemoveDir("a/inner".into())),
            ("signature", Message::SignatureRequest("a/victim".into())),
            ("hash", Message::HashRequest("a/victim".into())),
        ];

        for (name, msg) in msgs {
            let tmp = tempfile::tempdir().unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir_all(outside.join("inner")).unwrap();
            std::fs::write(outside.join("victim"), b"keep").unwrap();
            std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
            let before = std::fs::metadata(&outside).unwrap().mode();

            let (mut s, root) = session(tmp.path()).await;
            std::fs::write(root.join("plain"), b"p").unwrap();
            std::os::unix::fs::symlink(&outside, root.join("a")).unwrap();

            send(&mut s.conn, &msg).await.unwrap();
            let result = s.done.await.unwrap();
            assert!(
                matches!(result, Err(DriveError::PathEscape { .. })),
                "{name}: expected PathEscape, got {result:?}"
            );

            assert_eq!(
                std::fs::read(outside.join("victim")).unwrap(),
                b"keep",
                "{name}"
            );
            assert!(outside.join("inner").is_dir(), "{name}");
            let names: Vec<_> = std::fs::read_dir(&outside)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names.len(), 2, "{name}: outside gained {names:?}");
            assert_eq!(
                std::fs::metadata(&outside).unwrap().mode(),
                before,
                "{name}"
            );
            assert!(root.join("plain").exists(), "{name}");
        }
    }

    #[tokio::test]
    async fn the_escape_from_the_report_is_closed() {
        // Plant a link to a directory via the protocol, then write through it.
        let (uid, gid) = me();
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let (mut s, _root) = session(tmp.path()).await;
        send(
            &mut s.conn,
            &Message::WholeFile {
                path: "a".into(),
                mode: 0o777,
                uid,
                gid,
                mtime_nanos: 0,
                is_symlink: true,
                link_target: Some(outside.to_string_lossy().into_owned()),
                data: vec![],
            },
        )
        .await
        .unwrap();
        send(
            &mut s.conn,
            &Message::WholeFile {
                path: "a/x".into(),
                mode: 0o644,
                uid,
                gid,
                mtime_nanos: 0,
                is_symlink: false,
                link_target: None,
                data: b"pwn".to_vec(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            s.done.await.unwrap(),
            Err(DriveError::PathEscape { .. })
        ));
        assert!(!outside.join("x").exists());
    }

    #[test]
    fn a_preplanted_symlink_at_the_temp_name_is_not_written_through() {
        let (uid, gid) = me();
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let target = tmp.path().join("f");
        std::os::unix::fs::symlink(&victim, temp_path(&target)).unwrap();

        write_temp_then_rename(&target, b"new", 0o644, uid, gid, 0).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!std::fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!temp_path(&target).exists());
    }

    #[test]
    fn create_temp_refuses_to_follow_a_symlink_that_appears_late() {
        // The open itself must not follow, independent of the stale removal
        // that normally runs just before it.
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let link = tmp.path().join("t");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let err = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&link);
        assert!(err.is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    }

    fn leftover_temps(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect()
    }

    #[tokio::test]
    async fn the_temp_file_is_removed_when_the_connection_drops_mid_transfer() {
        let (uid, gid) = me();
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, root) = session(tmp.path()).await;
        send(
            &mut s.conn,
            &Message::WholeFileBegin {
                path: "big".into(),
                mode: 0o644,
                uid,
                gid,
                mtime_nanos: 0,
            },
        )
        .await
        .unwrap();
        send(&mut s.conn, &Message::WholeFileChunk { data: vec![1; 64] })
            .await
            .unwrap();
        drop(s.conn);
        s.done.await.unwrap().unwrap();
        assert!(
            leftover_temps(&root).is_empty(),
            "{:?}",
            leftover_temps(&root)
        );
        assert!(!root.join("big").exists());
    }

    #[tokio::test]
    async fn a_second_begin_discards_the_unfinished_transfer() {
        let (uid, gid) = me();
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, root) = session(tmp.path()).await;
        for path in ["first", "second"] {
            send(
                &mut s.conn,
                &Message::WholeFileBegin {
                    path: path.into(),
                    mode: 0o644,
                    uid,
                    gid,
                    mtime_nanos: 0,
                },
            )
            .await
            .unwrap();
            send(
                &mut s.conn,
                &Message::WholeFileChunk {
                    data: b"x".to_vec(),
                },
            )
            .await
            .unwrap();
        }
        send(&mut s.conn, &Message::WholeFileEnd).await.unwrap();
        send(&mut s.conn, &Message::Done).await.unwrap();
        assert!(matches!(
            recv(&mut s.conn).await.unwrap(),
            Some(Message::Ack(1))
        ));
        s.done.await.unwrap().unwrap();
        assert!(
            leftover_temps(&root).is_empty(),
            "{:?}",
            leftover_temps(&root)
        );
        assert!(root.join("second").exists());
        assert!(!root.join("first").exists());
    }

    #[tokio::test]
    async fn an_unexpected_message_ends_the_connection_with_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, _root) = session(tmp.path()).await;
        send(&mut s.conn, &Message::Ack(1)).await.unwrap();
        assert!(matches!(
            s.done.await.unwrap(),
            Err(DriveError::Transfer { .. })
        ));
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

/// The listener must refuse anything that has not proven it holds the cluster
/// key, before it will read a manifest or write a byte.
#[cfg(test)]
mod auth_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A real `serve_listener` over a temp drive, and the key it holds.
    async fn listener(root: &Path) -> (SocketAddr, KeyWatch) {
        let registry = Arc::new(DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), root.to_path_buf())]),
        ));
        let keys = cl_raft::throwaway_key();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_listener(listener, registry, keys.clone()));
        (addr, keys)
    }

    fn manifest_request() -> Message {
        Message::ManifestRequest {
            volume: VolumeId::new("fast", "v"),
            hash_content: true,
        }
    }

    #[tokio::test]
    async fn an_authenticated_peer_gets_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, keys) = listener(dir.path()).await;
        request_manifest(addr, &VolumeId::new("fast", "v"), false, &keys)
            .await
            .expect("a peer with the key is served");
    }

    #[tokio::test]
    async fn a_peer_with_the_wrong_key_gets_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _keys) = listener(dir.path()).await;
        let wrong = cl_raft::throwaway_key();
        let err = request_manifest(addr, &VolumeId::new("fast", "v"), false, &wrong)
            .await
            .expect_err("a peer with another key must be refused");
        assert!(matches!(err, DriveError::Transfer { .. }), "{err}");
        // Refusal must not have created the volume either.
        assert!(!dir.path().join("v").exists());
    }

    #[tokio::test]
    async fn a_peer_that_skips_the_handshake_gets_no_ready_and_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _keys) = listener(dir.path()).await;

        // Speak the framed protocol straight away, as the old unauthenticated
        // listener allowed. The server's handshake hello arrives as unframed
        // bytes, which is not a valid frame, so the connection must end
        // without a `Ready` ever being produced.
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let mut conn_bytes = Vec::new();
        let raw = bincode::serialize(&manifest_request()).unwrap();
        conn_bytes.extend_from_slice(&(raw.len() as u32).to_be_bytes());
        conn_bytes.extend_from_slice(&raw);
        let raw = bincode::serialize(&Message::WholeFile {
            path: "pwned.txt".into(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime_nanos: 0,
            is_symlink: false,
            link_target: None,
            data: b"x".to_vec(),
        })
        .unwrap();
        conn_bytes.extend_from_slice(&(raw.len() as u32).to_be_bytes());
        conn_bytes.extend_from_slice(&raw);
        let _ = stream.write_all(&conn_bytes).await;

        // Anything the server says must be the handshake nonce, never a
        // framed `Ready`; after the garbage reply it closes.
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut got)).await;
        assert!(
            got.len() <= 4 + 32,
            "server sent more than its hello: {got:?}"
        );
        assert!(!dir.path().join("v").exists());
        assert!(!dir.path().join("v").join("pwned.txt").exists());
    }

    #[tokio::test]
    async fn a_node_with_no_key_refuses_every_connection() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(DriveRegistry::from_drives(
            "n1",
            BTreeMap::from([("fast".to_string(), dir.path().to_path_buf())]),
        ));
        let (_tx, keys) = {
            let (tx, rx) = tokio::sync::watch::channel(None);
            (tx, rx)
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_listener(listener, registry, keys));

        let client_keys = cl_raft::throwaway_key();
        assert!(
            request_manifest(addr, &VolumeId::new("fast", "v"), false, &client_keys)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_sender_with_no_key_fails_clearly_before_connecting() {
        let (_tx, keys) = tokio::sync::watch::channel(None);
        let err = request_manifest(
            "127.0.0.1:9".parse().unwrap(),
            &VolumeId::new("fast", "v"),
            false,
            &keys,
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("no cluster key"), "{err}");
    }
}
