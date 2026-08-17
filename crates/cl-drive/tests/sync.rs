//! Integration tests over a real loopback transport: two temp directories
//! standing in for two nodes' volume roots, synced over actual TCP.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener as StdListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cl_drive::{DeltaEngine, DriveRegistry, SyncEngine};
use cl_raft::VolumeId;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const DRIVE: &str = "fast";
const VOLUME: &str = "data";

fn volume_id() -> VolumeId {
    VolumeId::new(DRIVE, VOLUME)
}

/// Bind a receiver on an ephemeral loopback port, backed by `root/data`.
/// Every accepted connection runs one `transport::receive` call.
fn spawn_receiver(root: &Path) -> (SocketAddr, Arc<DriveRegistry>, JoinHandle<()>) {
    let registry = Arc::new(DriveRegistry::from_drives(
        "target",
        BTreeMap::from([(DRIVE.to_string(), root.to_path_buf())]),
    ));

    // Bind synchronously so the address is known before the caller races
    // ahead and tries to connect.
    let std_listener = StdListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let addr = std_listener.local_addr().expect("addr");
    let listener = TcpListener::from_std(std_listener).expect("tokio listener");

    let task_registry = registry.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let registry = task_registry.clone();
            tokio::spawn(async move {
                let _ = cl_drive::transport::receive(stream, &registry).await;
            });
        }
    });

    (addr, registry, handle)
}

/// The receiver's registry roots each drive at the tempdir directly, so the
/// volume's files land under `<root>/<VOLUME>`, same as the sender's
/// registry would resolve it. Tests that inspect the receiver's disk need
/// this, not the bare tempdir path.
fn dst_volume(dst: &tempfile::TempDir) -> PathBuf {
    dst.path().join(VOLUME)
}

/// Compares content, not metadata: sync does not (and should not) preserve
/// the source's mtime, only its data, mode and symlink target.
fn tree_equal(a: &Path, b: &Path) -> bool {
    let ma = cl_drive::scan_volume(a, true).expect("scan a");
    let mb = cl_drive::scan_volume(b, true).expect("scan b");
    if ma.files.len() != mb.files.len() {
        return false;
    }
    ma.files.iter().all(|(path, meta)| {
        mb.files.get(path).is_some_and(|other| {
            meta.is_symlink == other.is_symlink
                && meta.link_target == other.link_target
                && meta.mode == other.mode
                && meta.hash == other.hash
        })
    })
}

#[tokio::test]
async fn syncs_a_directory_tree_byte_for_byte() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    std::fs::write(src.path().join("a.txt"), b"hello world").unwrap();
    std::fs::create_dir(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("sub/b.txt"), b"nested content").unwrap();
    std::os::unix::fs::symlink("a.txt", src.path().join("link")).unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let engine = DeltaEngine::default();
    let stats = engine.sync(src.path(), addr, &volume_id(), 1).await.expect("sync");

    assert_eq!(stats.files_added, 3);
    assert!(tree_equal(src.path(), &dst_volume(&dst)));

    let link_meta = std::fs::symlink_metadata(dst_volume(&dst).join("link")).unwrap();
    assert!(link_meta.file_type().is_symlink());
}

#[tokio::test]
async fn a_small_edit_sends_far_fewer_bytes_than_the_whole_file() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    // Big enough to clear the whole-file threshold, so the second sync goes
    // through the signature/delta path instead of a flat resend.
    let mut content = vec![b'x'; 200_000];
    std::fs::write(src.path().join("big.bin"), &content).unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let engine = DeltaEngine::default();
    let first = engine.sync(src.path(), addr, &volume_id(), 1).await.expect("first sync");
    assert!(first.bytes_sent >= 200_000, "first sync ships the whole file");

    // Flip a handful of bytes in the middle; everything else is unchanged.
    for byte in content.iter_mut().skip(100_000).take(16) {
        *byte = b'y';
    }
    let big_path = src.path().join("big.bin");
    std::fs::write(&big_path, &content).unwrap();
    // Force the mtime forward: some filesystems round to whole seconds, and
    // this test must not depend on real time passing between the two writes.
    let file = std::fs::File::open(&big_path).unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2)).unwrap();

    let second = engine.sync(src.path(), addr, &volume_id(), 2).await.expect("second sync");
    assert_eq!(second.files_changed, 1);
    assert!(
        second.bytes_sent < 20_000,
        "delta should be a small fraction of the file, got {} bytes",
        second.bytes_sent
    );
    assert_eq!(std::fs::read(dst_volume(&dst).join("big.bin")).unwrap(), content);
}

#[tokio::test]
async fn deleting_a_file_removes_it_on_the_peer() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    std::fs::write(src.path().join("keep.txt"), b"stays").unwrap();
    std::fs::write(src.path().join("gone.txt"), b"leaves").unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let engine = DeltaEngine::default();
    engine.sync(src.path(), addr, &volume_id(), 1).await.expect("first sync");
    assert!(dst_volume(&dst).join("gone.txt").exists());

    std::fs::remove_file(src.path().join("gone.txt")).unwrap();
    let stats = engine.sync(src.path(), addr, &volume_id(), 2).await.expect("second sync");

    assert_eq!(stats.files_removed, 1);
    assert!(!dst_volume(&dst).join("gone.txt").exists());
    assert!(dst_volume(&dst).join("keep.txt").exists());
}

#[tokio::test]
async fn renaming_a_file_moves_it_instead_of_retransferring() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let content = vec![b'z'; 50_000];
    std::fs::write(src.path().join("old_name.bin"), &content).unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let engine = DeltaEngine::default();
    engine.sync(src.path(), addr, &volume_id(), 1).await.expect("first sync");

    std::fs::rename(src.path().join("old_name.bin"), src.path().join("new_name.bin")).unwrap();
    let stats = engine.sync(src.path(), addr, &volume_id(), 2).await.expect("second sync");

    assert_eq!(stats.files_renamed, 1);
    assert_eq!(stats.files_added, 0, "a rename should not count as a fresh add");
    assert!(!dst_volume(&dst).join("old_name.bin").exists());
    assert_eq!(std::fs::read(dst_volume(&dst).join("new_name.bin")).unwrap(), content);
}

#[tokio::test]
async fn an_interrupted_transfer_never_leaves_a_partial_file_visible() {
    let dst = tempfile::tempdir().unwrap();
    let (addr, _registry, _rx) = spawn_receiver(dst.path());

    // Drive the wire protocol by hand so the connection can be dropped
    // mid-sync, after a whole file has been fully framed and sent but
    // before `Done`/`Ack` ever happens.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut conn = cl_drive::transport::frame(stream);
    let volume = volume_id();

    cl_drive::transport::send(&mut conn, &cl_drive::Message::Hello { volume: volume.clone(), generation: 1 })
        .await
        .unwrap();
    match cl_drive::transport::recv(&mut conn).await.unwrap() {
        Some(cl_drive::Message::Ready(_)) => {}
        other => panic!("expected ready, got {other:?}"),
    }

    let payload = vec![b'p'; 32 * 1024];
    cl_drive::transport::send(
        &mut conn,
        &cl_drive::Message::WholeFile {
            path: "committed.bin".into(),
            mode: 0o644,
            is_symlink: false,
            link_target: None,
            data: payload.clone(),
        },
    )
    .await
    .unwrap();

    // Give the receiver a moment to apply the message, then vanish without
    // sending `Done`. A real interruption looks exactly like this from the
    // receiver's point of view: a socket that stops producing frames.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(conn);

    let target = dst_volume(&dst).join("committed.bin");
    assert_eq!(std::fs::read(&target).unwrap(), payload, "a fully framed message is applied whole");

    // No stray temp file left dangling under the volume root.
    for entry in std::fs::read_dir(dst_volume(&dst)).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(!name.to_string_lossy().starts_with('.'), "leaked temp file: {name:?}");
    }
}

#[tokio::test]
async fn migrated_manifest_matches_after_verification_scan() {
    // Sanity check that scan+diff sees a synced tree as identical, the
    // property migration verification relies on.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.txt"), b"content").unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    DeltaEngine::default().sync(src.path(), addr, &volume_id(), 1).await.unwrap();

    let source_manifest = cl_drive::scan_volume(src.path(), true).unwrap();
    let target_manifest =
        cl_drive::transport::request_manifest(addr, &volume_id(), true).await.unwrap();
    let diff = source_manifest.diff(&target_manifest);
    assert!(diff.added.is_empty() && diff.changed.is_empty() && diff.removed.is_empty());
}
