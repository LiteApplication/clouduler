//! Served drives: one node holds the data and serves it to the rest of the
//! cluster over the internal network, the way NFS would, but implemented by
//! the daemon itself so it doesn't depend on a system NFS server being
//! installed and configured on every node.
//!
//! `protocol` is the filesystem RPC shared by both sides. `server` answers
//! it against the real directory, on the node named as a drive's
//! `served_by`. `client` is a `fuser` filesystem that turns kernel calls
//! into protocol requests, mounted on every other node that implements the
//! drive. `mount` owns the lifecycle of those mounts: mounting at daemon
//! start, unmounting on shutdown, and detecting when mounting isn't
//! possible at all.

pub mod client;
pub mod mount;
pub mod protocol;
pub mod server;

pub use client::{ClientConfig, ClientFs};
pub use mount::{MountManager, MountPermission, MountSpec};
pub use protocol::{Attr, DirEntry, FileKind, Handle, ProtoError, Request, Response, StatfsInfo};
pub use server::{handle_connection, serve, HandleTable};
