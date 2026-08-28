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

/// Compares content and metadata, mtime included for regular files: the
/// sender's mtime has to survive the trip, or the next `scan`+`diff` on
/// either side sees a fresh "now" timestamp that never matches the source's
/// and re-syncs the file forever (see `a_synced_file_is_not_re_diffed_as_changed`
/// below). Symlinks are exempt: `FileMeta::differs_from` never compares their
/// mtime (only `is_symlink`/`link_target`), so `write_whole` doesn't restamp
/// one either.
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
                && (meta.is_symlink || meta.mtime_nanos == other.mtime_nanos)
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

/// A file that already made it across must not be re-diffed as "changed" on
/// the very next pass. Before the receiver preserved the sender's mtime, the
/// write itself stamped a fresh "now" mtime that could never again match the
/// source's — so a re-scan on both sides always disagreed, and every synced
/// file was resent on every single pass forever, regardless of whether its
/// content had actually changed. This is what made a large volume (e.g. a
/// Nextcloud preview cache with hundreds of thousands of files) never settle:
/// each pass took longer than the sync interval, so the scheduler restarted
/// it back-to-back, pinning the daemon's CPU indefinitely.
#[tokio::test]
async fn a_synced_file_is_not_re_diffed_as_changed() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    std::fs::write(src.path().join("a.txt"), b"hello world").unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let engine = DeltaEngine::default();
    let first = engine.sync(src.path(), addr, &volume_id(), 1).await.expect("first sync");
    assert_eq!(first.files_added, 1);

    // Nothing on either side changed between passes; a second sync must ship
    // nothing at all.
    let second = engine.sync(src.path(), addr, &volume_id(), 2).await.expect("second sync");
    assert_eq!(second.files_added, 0);
    assert_eq!(second.files_changed, 0);
    assert_eq!(second.files_removed, 0);

    // The actual invariant: scanning both sides independently agrees they
    // hold the same file, mtime included, so `Manifest::diff` between them
    // is empty.
    let src_manifest = cl_drive::scan_volume(src.path(), false).expect("scan src");
    let dst_manifest = cl_drive::scan_volume(&dst_volume(&dst), false).expect("scan dst");
    assert_eq!(src_manifest.diff(&dst_manifest), cl_drive::Diff::default());
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

/// Rename detection matches on content hash, and content equality is not
/// identity. A postgres data directory holds hundreds of byte-identical
/// files — every freshly created `_fsm`/`_vm` fork is zero length — so one
/// hash legitimately maps to many paths on both sides.
///
/// Keyed to a single path per hash, every one of those additions claimed the
/// *same* source: the sender emitted N renames out of one file, the second
/// reached a receiver that no longer had `from`, `std::fs::rename` returned
/// `ENOENT`, and `receive` returned an error — which drops the connection
/// mid-stream. All the sender ever saw was `Broken pipe`. Deterministic, so
/// the volume then failed on every single pass, forever.
#[tokio::test]
async fn many_identical_files_replaced_at_once_still_sync() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    // A batch of empty files and a batch sharing one non-empty body: both
    // collapse to a single hash.
    for i in 0..20 {
        std::fs::write(src.path().join(format!("empty_{i}")), b"").unwrap();
        std::fs::write(src.path().join(format!("same_{i}")), b"identical body").unwrap();
    }

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let engine = DeltaEngine::default();
    engine.sync(src.path(), addr, &volume_id(), 1).await.expect("first sync");

    // Swap the whole lot for a differently named set with the same contents,
    // exactly as postgres does when it rewrites relations under new
    // relfilenode numbers.
    for i in 0..20 {
        std::fs::remove_file(src.path().join(format!("empty_{i}"))).unwrap();
        std::fs::remove_file(src.path().join(format!("same_{i}"))).unwrap();
        std::fs::write(src.path().join(format!("empty_new_{i}")), b"").unwrap();
        std::fs::write(src.path().join(format!("same_new_{i}")), b"identical body").unwrap();
    }

    let stats = engine.sync(src.path(), addr, &volume_id(), 2).await.expect("second sync must not die");
    assert!(tree_equal(src.path(), &dst_volume(&dst)));

    // Each source is claimed at most once, so renames never exceed the
    // number of files that actually went away.
    assert!(stats.files_renamed <= 20, "a source was reused: {} renames", stats.files_renamed);

    // And it settles: a third pass has nothing left to do.
    let third = engine.sync(src.path(), addr, &volume_id(), 3).await.expect("third sync");
    assert_eq!(third.files_added, 0);
    assert_eq!(third.files_changed, 0);
    assert_eq!(third.files_removed, 0);
}

/// A rename is only ever the sender's guess. If the source is gone by the
/// time it lands, the receiver must skip that one file rather than abort —
/// dropping the connection here is what turned a single bad guess into a
/// volume that never synced again.
#[tokio::test]
async fn a_rename_whose_source_is_missing_does_not_abort_the_sync() {
    use cl_drive::transport::{self, Message};

    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut conn = transport::frame(stream);

    transport::send(&mut conn, &Message::Hello { volume: volume_id(), generation: 1 })
        .await
        .expect("hello");
    assert!(matches!(transport::recv(&mut conn).await.expect("ready"), Some(Message::Ready(_))));

    transport::send(&mut conn, &Message::Rename { from: "not_there".into(), to: "wherever".into(), mode: 0o644, mtime_nanos: 1_000_000_000 })
        .await
        .expect("rename");
    // The connection has to still be alive and complete the sync normally.
    transport::send(&mut conn, &Message::WholeFile {
        path: "after.txt".into(),
        mode: 0o644,
        mtime_nanos: 1_000_000_000,
        is_symlink: false,
        link_target: None,
        data: b"still here".to_vec(),
    })
    .await
    .expect("whole file after a failed rename");

    transport::send(&mut conn, &Message::Done).await.expect("done");
    assert_eq!(transport::recv(&mut conn).await.expect("ack"), Some(Message::Ack(1)));
    assert_eq!(std::fs::read(dst_volume(&dst).join("after.txt")).unwrap(), b"still here");
    let _ = src;
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
            mtime_nanos: 0,
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
async fn a_file_bigger_than_one_chunk_arrives_intact() {
    // Regression test: a file whose single `WholeFile` frame would exceed
    // the wire's max frame length (a large postgres relation file being the
    // real-world case that surfaced this) used to abort the sync partway
    // through, leaving the target with only whatever came before it
    // alphabetically. `send_whole` now streams anything over
    // `WHOLE_FILE_CHUNK` as a begin/chunk/end sequence instead.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let size = cl_drive::transport::WHOLE_FILE_CHUNK + 5 * 1024 * 1024;
    let mut content = vec![0u8; size];
    for (i, byte) in content.iter_mut().enumerate() {
        *byte = (i % 251) as u8;
    }
    std::fs::write(src.path().join("big.bin"), &content).unwrap();
    // A small file sorting after "big.bin" would previously never arrive at
    // all once the oversized frame aborted the connection.
    std::fs::write(src.path().join("small.txt"), b"tail file").unwrap();

    let (addr, _registry, _rx) = spawn_receiver(dst.path());
    let engine = DeltaEngine::default();
    let stats = engine.sync(src.path(), addr, &volume_id(), 1).await.expect("sync");

    assert_eq!(stats.files_added, 2);
    assert_eq!(std::fs::read(dst_volume(&dst).join("big.bin")).unwrap(), content);
    assert_eq!(std::fs::read(dst_volume(&dst).join("small.txt")).unwrap(), b"tail file");
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
