//! End-to-end restore tests against a real `restic` binary on a local
//! repository. Skipped (with a note) when `restic` is not on `PATH`, so the
//! suite still passes on machines without it - install restic to exercise
//! the restore path semantics for real.

use std::fs;
use std::path::Path;

use cl_restic::dependency::restic_available;
use cl_restic::destination::local_repo;
use cl_restic::restic::ResticRepo;
use cl_restic::restore::include_path_for_volume;

fn skip_without_restic() -> bool {
    if restic_available() {
        return false;
    }
    eprintln!("skipping: restic is not installed");
    true
}

fn repo_in(dir: &Path) -> ResticRepo {
    let password = dir.join("password");
    fs::write(&password, "pw").unwrap();
    let repo_dir = dir.join("repo");
    let repo = ResticRepo::new(local_repo(&repo_dir), Vec::new(), &password, &[]);
    repo.init().expect("init");
    repo
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// Builds `<root>/{vol1/sub/f, vol2/g, a/b/h, odd*name/i}`, backs `root` up
/// and returns (repo, snapshot id).
fn backed_up(dir: &Path, root: &Path) -> (ResticRepo, String) {
    write(&root.join("vol1/sub/f"), "one");
    write(&root.join("vol2/g"), "two");
    write(&root.join("a/b/h"), "nested");
    write(&root.join("odd*name/i"), "odd");
    let repo = repo_in(dir);
    let outcome = repo.backup(&[root], &["fast"], |_| {}).expect("backup");
    (repo, outcome.snapshot_id)
}

fn root_str(root: &Path) -> String {
    root.to_str().unwrap().to_string()
}

#[test]
fn a_whole_drive_restore_lands_directly_in_the_target() {
    if skip_without_restic() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("drive");
    let (repo, id) = backed_up(tmp.path(), &root);

    // Simulate the live drive having lost its data.
    fs::remove_dir_all(&root).unwrap();
    fs::create_dir_all(&root).unwrap();

    repo.restore(&id, &root_str(&root), &root, None)
        .expect("restore");

    assert_eq!(fs::read_to_string(root.join("vol1/sub/f")).unwrap(), "one");
    assert_eq!(fs::read_to_string(root.join("vol2/g")).unwrap(), "two");
    // Not recreated under the absolute path.
    assert!(!root.join(root.strip_prefix("/").unwrap()).exists());
}

#[test]
fn a_volume_restore_touches_only_that_volume() {
    if skip_without_restic() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("drive");
    let (repo, id) = backed_up(tmp.path(), &root);

    // Corrupt both volumes after the backup.
    write(&root.join("vol1/sub/f"), "CORRUPT");
    write(&root.join("vol2/g"), "CORRUPT");

    let include = include_path_for_volume("vol1");
    repo.restore(&id, &root_str(&root), &root, include.as_deref())
        .expect("restore");

    assert_eq!(fs::read_to_string(root.join("vol1/sub/f")).unwrap(), "one");
    assert_eq!(
        fs::read_to_string(root.join("vol2/g")).unwrap(),
        "CORRUPT",
        "a volume restore must leave the other volumes alone"
    );
}

#[test]
fn nested_and_glob_named_volumes_restore_by_their_literal_name() {
    if skip_without_restic() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("drive");
    let (repo, id) = backed_up(tmp.path(), &root);

    write(&root.join("a/b/h"), "CORRUPT");
    write(&root.join("odd*name/i"), "CORRUPT");
    write(&root.join("vol2/g"), "CORRUPT");

    for volume in ["a/b", "odd*name"] {
        let include = include_path_for_volume(volume);
        repo.restore(&id, &root_str(&root), &root, include.as_deref())
            .expect("restore");
    }

    assert_eq!(fs::read_to_string(root.join("a/b/h")).unwrap(), "nested");
    assert_eq!(fs::read_to_string(root.join("odd*name/i")).unwrap(), "odd");
    assert_eq!(fs::read_to_string(root.join("vol2/g")).unwrap(), "CORRUPT");
}

#[test]
fn the_snapshot_reports_the_source_path_it_was_taken_of() {
    if skip_without_restic() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("drive");
    let (repo, id) = backed_up(tmp.path(), &root);

    let snapshots = repo.snapshots().expect("snapshots");
    let snapshot = snapshots.iter().find(|s| s.id == id).expect("our snapshot");
    assert_eq!(snapshot.paths, vec![root_str(&root)]);
    assert_eq!(snapshot.tags, vec!["fast".to_string()]);
}
