//! Integration tests for the served drive protocol over a real loopback
//! connection: a server task backed by a temp directory, driven with the
//! client protocol calls directly. No FUSE mount involved; that path is
//! exercised only by the `#[ignore]`d test at the bottom, since mounting
//! needs root or `user_allow_other` and CI has neither.

use std::net::SocketAddr;
use std::time::Duration;

use cl_drive::served::protocol::{self, DirEntry, FileKind, ProtoError, Request, Response};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Bind a server on an ephemeral loopback port, backed by `root`. Every
/// connection is handled inline on the one returned task rather than
/// spawned off separately, so aborting that single handle simulates the
/// whole server process dying, connections included, not just its listener.
async fn spawn_server(root: std::path::PathBuf) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let _ = cl_drive::served::server::handle_connection(stream, root.clone()).await;
        }
    });
    (addr, handle)
}

async fn connect(addr: SocketAddr) -> cl_drive::transport::Connection {
    let stream = TcpStream::connect(addr).await.expect("connect");
    cl_drive::transport::frame(stream)
}

async fn call(conn: &mut cl_drive::transport::Connection, req: Request) -> Response {
    protocol::send_request(conn, &req).await.expect("send");
    protocol::recv_response(conn).await.expect("recv").expect("server closed early")
}

#[tokio::test]
async fn create_write_and_read_back_a_file() {
    let root = tempfile::tempdir().unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::Create { path: "a.txt".into(), mode: 0o644 }).await;
    let handle = match resp {
        Response::Created { handle, .. } => handle,
        other => panic!("expected Created, got {other:?}"),
    };

    let resp = call(&mut conn, Request::Write { handle, offset: 0, data: b"hello world".to_vec() }).await;
    assert_eq!(resp, Response::Written(11));

    let resp = call(&mut conn, Request::Read { handle, offset: 0, len: 100 }).await;
    assert_eq!(resp, Response::Data(b"hello world".to_vec()));

    call(&mut conn, Request::Release(handle)).await;

    assert_eq!(std::fs::read(root.path().join("a.txt")).unwrap(), b"hello world");
}

#[tokio::test]
async fn write_at_an_offset_only_touches_that_range() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("b.txt"), b"0123456789").unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::Open { path: "b.txt".into(), write: true }).await;
    let handle = match resp {
        Response::Opened(h) => h,
        other => panic!("expected Opened, got {other:?}"),
    };

    let resp = call(&mut conn, Request::Write { handle, offset: 3, data: b"XYZ".to_vec() }).await;
    assert_eq!(resp, Response::Written(3));
    call(&mut conn, Request::Release(handle)).await;

    assert_eq!(std::fs::read(root.path().join("b.txt")).unwrap(), b"012XYZ6789");
}

#[tokio::test]
async fn readdir_lists_directory_contents() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("one.txt"), b"1").unwrap();
    std::fs::create_dir(root.path().join("sub")).unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::ReadDir(".".into())).await;
    let mut entries = match resp {
        Response::Entries(e) => e,
        other => panic!("expected Entries, got {other:?}"),
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        entries,
        vec![
            DirEntry { name: "one.txt".into(), kind: FileKind::File },
            DirEntry { name: "sub".into(), kind: FileKind::Dir },
        ]
    );
}

#[tokio::test]
async fn rename_moves_the_file_on_disk() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("old.txt"), b"content").unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::Rename { from: "old.txt".into(), to: "new.txt".into() }).await;
    assert_eq!(resp, Response::Ok);

    assert!(!root.path().join("old.txt").exists());
    assert_eq!(std::fs::read(root.path().join("new.txt")).unwrap(), b"content");
}

#[tokio::test]
async fn unlink_removes_the_file() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("gone.txt"), b"bye").unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::Unlink("gone.txt".into())).await;
    assert_eq!(resp, Response::Ok);
    assert!(!root.path().join("gone.txt").exists());
}

#[tokio::test]
async fn mkdir_and_rmdir_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::Mkdir { path: "sub".into(), mode: 0o755 }).await;
    assert_eq!(resp, Response::Ok);
    assert!(root.path().join("sub").is_dir());

    let resp = call(&mut conn, Request::Rmdir("sub".into())).await;
    assert_eq!(resp, Response::Ok);
    assert!(!root.path().join("sub").exists());
}

#[tokio::test]
async fn getattr_reports_size_and_kind() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("f.txt"), b"12345").unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::GetAttr("f.txt".into())).await;
    match resp {
        Response::Attr(attr) => {
            assert_eq!(attr.size, 5);
            assert_eq!(attr.kind, FileKind::File);
        }
        other => panic!("expected Attr, got {other:?}"),
    }
}

#[tokio::test]
async fn a_path_that_escapes_the_root_is_rejected_not_served() {
    let root = tempfile::tempdir().unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::GetAttr("../../etc/passwd".into())).await;
    assert_eq!(resp, Response::Err(ProtoError::Escape));
}

#[tokio::test]
async fn symlink_create_and_readlink_round_trip() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("real.txt"), b"data").unwrap();
    let (addr, _server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    let resp = call(&mut conn, Request::Symlink { path: "link".into(), target: "real.txt".into() }).await;
    assert_eq!(resp, Response::Ok);

    let resp = call(&mut conn, Request::Readlink("link".into())).await;
    assert_eq!(resp, Response::Link("real.txt".into()));
}

/// The server task is aborted mid-connection, the way a killed process
/// would drop every open socket. A client waiting on a response must see
/// that as an error promptly, not hang forever.
#[tokio::test]
async fn a_killed_server_surfaces_as_an_error_not_a_hang() {
    let root = tempfile::tempdir().unwrap();
    let (addr, server) = spawn_server(root.path().to_path_buf()).await;
    let mut conn = connect(addr).await;

    // One request to confirm the connection is live before killing the server.
    let resp = call(&mut conn, Request::Statfs).await;
    assert!(matches!(resp, Response::Statfs(_)));

    server.abort();
    // Give the abort a moment to actually close the listening socket and
    // any accepted connections before the next call.
    tokio::time::sleep(Duration::from_millis(50)).await;

    protocol::send_request(&mut conn, &Request::Statfs).await.ok();
    let result = tokio::time::timeout(Duration::from_secs(5), protocol::recv_response(&mut conn)).await;

    match result {
        Ok(Ok(None)) => {} // connection closed cleanly: also a valid "went away"
        Ok(Err(_)) => {}   // io error reading the closed socket
        Ok(Ok(Some(other))) => panic!("expected the connection to be gone, got {other:?}"),
        Err(_) => panic!("recv hung instead of erroring out when the server died"),
    }
}

/// Actually mounting requires root or `user_allow_other` in `/etc/fuse.conf`;
/// CI has neither. Run manually with `cargo test -p cl-drive --test served
/// -- --ignored mount_a_served_drive_and_read_a_file_through_the_kernel`.
#[tokio::test]
#[ignore = "requires a real FUSE mount, needs root or user_allow_other"]
async fn mount_a_served_drive_and_read_a_file_through_the_kernel() {
    use cl_drive::served::mount::MountManager;
    use cl_drive::served::MountSpec;

    let server_root = tempfile::tempdir().unwrap();
    std::fs::write(server_root.path().join("hello.txt"), b"served over fuse").unwrap();
    let (addr, _server) = spawn_server(server_root.path().to_path_buf()).await;

    let mountpoint = tempfile::tempdir().unwrap();
    let manager = MountManager::new(tokio::runtime::Handle::current());
    manager
        .mount_one(MountSpec { drive: "fast".into(), mountpoint: mountpoint.path().to_path_buf(), server: addr })
        .expect("mount");

    let content = tokio::task::spawn_blocking({
        let path = mountpoint.path().join("hello.txt");
        move || std::fs::read(path)
    })
    .await
    .unwrap()
    .expect("read through the mount");
    assert_eq!(content, b"served over fuse");

    manager.unmount_all().expect("unmount");
}
