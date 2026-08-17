//! Wire protocol for a served drive: a small filesystem RPC that a FUSE
//! mount (`client.rs`) drives against the real directory on the serving
//! node (`server.rs`).
//!
//! Unlike the delta sync protocol in `transport.rs`, this connection is
//! long lived (one per mount, for as long as the mount exists) and each
//! request gets exactly one response, in order. A failed operation is
//! carried in the response as `ProtoError`, not by closing the connection:
//! a mount that hit `ENOENT` once should keep working, the way a real
//! filesystem does.
//!
//! Open files are addressed by `Handle` after `Open`/`Create`, so a `Read`
//! or `Write` doesn't have to re-resolve and re-open the path every call.
//! Everything else is addressed by path, relative to the drive root and
//! always `/`-separated, the same convention `transport.rs` uses.

use std::io;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

use crate::transport::Connection;

/// Largest single read or write this protocol will move in one request.
/// Callers moving a whole file (`read_file`/`write_file`) split into chunks
/// of this size, so a multi-gigabyte file never has to fit in one frame.
pub const MAX_CHUNK: u32 = 4 * 1024 * 1024;

pub type Handle = u64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attr {
    pub kind: FileKind,
    pub size: u64,
    pub mode: u32,
    pub mtime_nanos: i64,
    pub atime_nanos: i64,
    pub nlink: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub kind: FileKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct StatfsInfo {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
}

/// Errors that can come back for one operation, kept coarse enough that the
/// FUSE client can map each straight onto an errno.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProtoError {
    NotFound,
    NotDir,
    IsDir,
    NotEmpty,
    AlreadyExists,
    PermissionDenied,
    InvalidHandle,
    Escape,
    Io(String),
}

impl ProtoError {
    /// Turn a filesystem `io::Error` into the coarse category the wire
    /// carries. Anything without a matching category becomes `Io` with the
    /// original message, so nothing is silently swallowed.
    pub fn from_io(e: &io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => ProtoError::NotFound,
            io::ErrorKind::PermissionDenied => ProtoError::PermissionDenied,
            io::ErrorKind::AlreadyExists => ProtoError::AlreadyExists,
            _ => match e.raw_os_error() {
                Some(libc::ENOTDIR) => ProtoError::NotDir,
                Some(libc::EISDIR) => ProtoError::IsDir,
                Some(libc::ENOTEMPTY) => ProtoError::NotEmpty,
                _ => ProtoError::Io(e.to_string()),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Request {
    Lookup(String),
    GetAttr(String),
    SetAttr {
        path: String,
        mode: Option<u32>,
        mtime_nanos: Option<i64>,
        atime_nanos: Option<i64>,
    },
    ReadDir(String),
    Open {
        path: String,
        write: bool,
    },
    Read {
        handle: Handle,
        offset: u64,
        len: u32,
    },
    Write {
        handle: Handle,
        offset: u64,
        data: Vec<u8>,
    },
    Create {
        path: String,
        mode: u32,
    },
    Mkdir {
        path: String,
        mode: u32,
    },
    Unlink(String),
    Rmdir(String),
    Rename {
        from: String,
        to: String,
    },
    Truncate {
        path: String,
        size: u64,
    },
    Fsync(Handle),
    /// Closes a handle opened by `Open` or `Create`. Not in the caller's
    /// requested list, but required so a handle doesn't leak for the
    /// lifetime of the mount; `fuser`'s `release` callback maps onto it.
    Release(Handle),
    Statfs,
    Symlink {
        path: String,
        target: String,
    },
    Readlink(String),
    Link {
        /// New path to create.
        path: String,
        /// Existing path it should point at.
        target: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Response {
    Attr(Attr),
    Entries(Vec<DirEntry>),
    Opened(Handle),
    Created { handle: Handle, attr: Attr },
    Data(Vec<u8>),
    Written(u32),
    Statfs(StatfsInfo),
    Link(String),
    Ok,
    Err(ProtoError),
}

pub async fn send_request(conn: &mut Connection, req: &Request) -> io::Result<()> {
    send_json(conn, req).await
}

pub async fn recv_request(conn: &mut Connection) -> io::Result<Option<Request>> {
    recv_json(conn).await
}

pub async fn send_response(conn: &mut Connection, resp: &Response) -> io::Result<()> {
    send_json(conn, resp).await
}

pub async fn recv_response(conn: &mut Connection) -> io::Result<Option<Response>> {
    recv_json(conn).await
}

async fn send_json<T: Serialize>(conn: &mut Connection, msg: &T) -> io::Result<()> {
    let raw = serde_json::to_vec(msg).map_err(io::Error::other)?;
    conn.send(bytes::Bytes::from(raw)).await
}

async fn recv_json<T: for<'de> Deserialize<'de>>(conn: &mut Connection) -> io::Result<Option<T>> {
    match conn.next().await {
        Some(Ok(raw)) => Ok(Some(serde_json::from_slice(&raw).map_err(io::Error::other)?)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_round_trip(req: Request) {
        let raw = serde_json::to_vec(&req).unwrap();
        let decoded: Request = serde_json::from_slice(&raw).unwrap();
        assert_eq!(decoded, req);
    }

    fn resp_round_trip(resp: Response) {
        let raw = serde_json::to_vec(&resp).unwrap();
        let decoded: Response = serde_json::from_slice(&raw).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn every_request_variant_round_trips() {
        req_round_trip(Request::Lookup("a/b.txt".into()));
        req_round_trip(Request::GetAttr("a/b.txt".into()));
        req_round_trip(Request::SetAttr {
            path: "a/b.txt".into(),
            mode: Some(0o644),
            mtime_nanos: Some(123),
            atime_nanos: None,
        });
        req_round_trip(Request::ReadDir("a".into()));
        req_round_trip(Request::Open { path: "a/b.txt".into(), write: true });
        req_round_trip(Request::Read { handle: 1, offset: 0, len: 4096 });
        req_round_trip(Request::Write { handle: 1, offset: 10, data: vec![1, 2, 3] });
        req_round_trip(Request::Create { path: "new.txt".into(), mode: 0o600 });
        req_round_trip(Request::Mkdir { path: "sub".into(), mode: 0o755 });
        req_round_trip(Request::Unlink("gone.txt".into()));
        req_round_trip(Request::Rmdir("sub".into()));
        req_round_trip(Request::Rename { from: "a.txt".into(), to: "b.txt".into() });
        req_round_trip(Request::Truncate { path: "a.txt".into(), size: 0 });
        req_round_trip(Request::Fsync(1));
        req_round_trip(Request::Release(1));
        req_round_trip(Request::Statfs);
        req_round_trip(Request::Symlink { path: "link".into(), target: "a.txt".into() });
        req_round_trip(Request::Readlink("link".into()));
        req_round_trip(Request::Link { path: "hard".into(), target: "a.txt".into() });
    }

    #[test]
    fn every_response_variant_round_trips() {
        let attr = Attr {
            kind: FileKind::File,
            size: 5,
            mode: 0o644,
            mtime_nanos: 1,
            atime_nanos: 2,
            nlink: 1,
        };
        resp_round_trip(Response::Attr(attr.clone()));
        resp_round_trip(Response::Entries(vec![DirEntry { name: "a".into(), kind: FileKind::File }]));
        resp_round_trip(Response::Opened(7));
        resp_round_trip(Response::Created { handle: 7, attr });
        resp_round_trip(Response::Data(vec![1, 2, 3]));
        resp_round_trip(Response::Written(3));
        resp_round_trip(Response::Statfs(StatfsInfo {
            blocks: 1,
            bfree: 1,
            bavail: 1,
            files: 1,
            ffree: 1,
            bsize: 4096,
            namelen: 255,
        }));
        resp_round_trip(Response::Link("target".into()));
        resp_round_trip(Response::Ok);
        resp_round_trip(Response::Err(ProtoError::NotFound));
        resp_round_trip(Response::Err(ProtoError::Io("boom".into())));
    }

    #[test]
    fn io_error_kinds_map_to_coarse_categories() {
        assert_eq!(ProtoError::from_io(&io::Error::from(io::ErrorKind::NotFound)), ProtoError::NotFound);
        assert_eq!(
            ProtoError::from_io(&io::Error::from(io::ErrorKind::PermissionDenied)),
            ProtoError::PermissionDenied
        );
        assert_eq!(
            ProtoError::from_io(&io::Error::from(io::ErrorKind::AlreadyExists)),
            ProtoError::AlreadyExists
        );
        let notdir = io::Error::from_raw_os_error(libc::ENOTDIR);
        assert_eq!(ProtoError::from_io(&notdir), ProtoError::NotDir);
        let other = io::Error::other("weird");
        assert!(matches!(ProtoError::from_io(&other), ProtoError::Io(_)));
    }
}
