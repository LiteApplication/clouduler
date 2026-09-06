//! The git2 IO layer: a local working copy that gets fully rewritten each
//! tick, then staged, committed, and pushed with the cluster's ssh deploy
//! key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use git2::cert::Cert;
use git2::{CertificateCheckStatus, Cred, PushOptions, RemoteCallbacks, Repository, RepositoryInitOptions, Signature};

use crate::error::{BackupError, Result};
use crate::tree::TreeFile;

pub struct GitBackup {
    repo: Repository,
    dir: PathBuf,
    repo_url: String,
    branch: String,
}

impl GitBackup {
    /// Opens the local working repo at `dir`, initializing it (and wiring up
    /// `origin`) on first use.
    pub fn open_or_init(dir: &Path, repo_url: &str, branch: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| BackupError::Io { path: dir.to_path_buf(), source })?;

        let repo = match Repository::open(dir) {
            Ok(repo) => repo,
            Err(_) => {
                let mut opts = RepositoryInitOptions::new();
                opts.initial_head(branch);
                Repository::init_opts(dir, &opts)?
            }
        };

        match repo.find_remote("origin") {
            Ok(remote) if remote.url() == Some(repo_url) => {}
            Ok(_) => {
                repo.remote_delete("origin")?;
                repo.remote("origin", repo_url)?;
            }
            Err(_) => {
                repo.remote("origin", repo_url)?;
            }
        }

        Ok(Self {
            repo,
            dir: dir.to_path_buf(),
            repo_url: repo_url.to_string(),
            branch: branch.to_string(),
        })
    }

    pub fn repo_url(&self) -> &str {
        &self.repo_url
    }

    /// Replaces the whole working tree with `files`, and if that produced
    /// any change, commits and pushes it. Returns `None` when nothing
    /// changed, so an unrelated raft-version bump never produces an empty
    /// commit.
    pub fn sync(&self, files: &[TreeFile], message: &str, private_key_pem: &str) -> Result<Option<git2::Oid>> {
        self.rewrite_working_tree(files)?;

        let mut index = self.repo.index()?;
        index.clear()?;
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;

        let tree_oid = index.write_tree()?;
        let tree = self.repo.find_tree(tree_oid)?;

        if let Ok(head_tree) = self.repo.head().and_then(|h| h.peel_to_tree()) {
            if head_tree.id() == tree.id() {
                return Ok(None);
            }
        }

        let signature = Signature::now("clouduler", "clouduler@localhost")?;
        let parents = match self.repo.head().and_then(|h| h.peel_to_commit()) {
            Ok(commit) => vec![commit],
            Err(_) => vec![],
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let oid = self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )?;

        self.push(private_key_pem)?;

        Ok(Some(oid))
    }

    fn rewrite_working_tree(&self, files: &[TreeFile]) -> Result<()> {
        for entry in walk_tracked(&self.dir)? {
            let _ = std::fs::remove_file(&entry);
        }

        for file in files {
            let path = self.dir.join(&file.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| BackupError::Io { path: parent.to_path_buf(), source })?;
            }
            std::fs::write(&path, &file.contents).map_err(|source| BackupError::Io { path: path.clone(), source })?;
        }
        Ok(())
    }

    fn push(&self, private_key_pem: &str) -> Result<()> {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}", branch = self.branch);
        self.push_refspecs(&[refspec], private_key_pem)
    }

    fn push_refspecs(&self, refspecs: &[String], private_key_pem: &str) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let known_hosts_path = self.known_hosts_path();

        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, _allowed| {
            Cred::ssh_key_from_memory(
                username_from_url.unwrap_or("git"),
                None,
                private_key_pem,
                None,
            )
        });
        callbacks.certificate_check(move |cert, host| {
            check_host_key(&known_hosts_path, cert, host)
                .map_err(|e| git2::Error::from_str(&e.to_string()))
        });

        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);

        let refs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
        remote.push(&refs, Some(&mut push_options))?;
        Ok(())
    }

    /// Fetches `origin`'s tracked branch and returns its tip, or `None` if the
    /// branch does not exist on the remote yet. Does not touch the local
    /// working tree or the local branch ref - see `checkout_tree_to_disk` and
    /// `advance_local_ref` for that.
    pub fn fetch_remote_tip(&self, private_key_pem: &str) -> Result<Option<git2::Oid>> {
        let mut remote = self.repo.find_remote("origin")?;
        let known_hosts_path = self.known_hosts_path();

        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, _allowed| {
            Cred::ssh_key_from_memory(
                username_from_url.unwrap_or("git"),
                None,
                private_key_pem,
                None,
            )
        });
        callbacks.certificate_check(move |cert, host| {
            check_host_key(&known_hosts_path, cert, host)
                .map_err(|e| git2::Error::from_str(&e.to_string()))
        });

        let mut fetch_options = git2::FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);
        let tracking_ref = format!("refs/remotes/origin/{}", self.branch);
        let refspec = format!("refs/heads/{branch}:{tracking_ref}", branch = self.branch);
        remote.fetch(&[refspec.as_str()], Some(&mut fetch_options), None)?;

        match self.repo.find_reference(&tracking_ref) {
            Ok(reference) => Ok(reference.target()),
            Err(_) => Ok(None),
        }
    }

    /// The local tracked branch's current tip, i.e. the commit last accepted
    /// (either pushed by an export or accepted by a webhook-triggered pull).
    /// `None` before the very first one.
    pub fn local_branch_oid(&self) -> Result<Option<git2::Oid>> {
        match self.repo.find_reference(&format!("refs/heads/{}", self.branch)) {
            Ok(reference) => Ok(reference.target()),
            Err(_) => Ok(None),
        }
    }

    /// Moves the local branch ref to `oid` with no new commit - used once a
    /// fetched commit has passed validation and been applied to the cluster,
    /// so the next fetch's "known good" baseline is up to date. The remote
    /// already has `oid` (it is where it was fetched from), so this needs no
    /// push.
    pub fn advance_local_ref(&self, oid: git2::Oid) -> Result<()> {
        self.repo
            .reference(&format!("refs/heads/{}", self.branch), oid, true, "accept reviewed remote commit")?;
        Ok(())
    }

    /// Same as `advance_local_ref`, taking the oid as the hex string form a
    /// completed export or pull already has on hand (e.g. `run_git_export`'s
    /// return value), so a caller outside this crate never needs to depend on
    /// `git2` itself just to move a bookmark forward.
    pub fn advance_local_ref_str(&self, oid: &str) -> Result<()> {
        let oid = git2::Oid::from_str(oid)?;
        self.advance_local_ref(oid)
    }

    /// Whether the `secrets/` subtree differs between `good` and `candidate`,
    /// i.e. whether the pulled commit actually rotates a secret, not just
    /// whether the tree happens to contain any (every commit's tree does,
    /// since `sync` rewrites the whole tree every time it commits at all).
    /// `good` being `None` (no known-good history yet) counts as a change
    /// whenever `candidate` has any secrets at all.
    pub fn secrets_subtree_changed(&self, good: Option<git2::Oid>, candidate: git2::Oid) -> Result<bool> {
        let candidate_secrets = self.subtree_oid(candidate, "secrets")?;
        let good_secrets = match good {
            Some(oid) => self.subtree_oid(oid, "secrets")?,
            None => None,
        };
        Ok(candidate_secrets != good_secrets)
    }

    fn subtree_oid(&self, commit_oid: git2::Oid, path: &str) -> Result<Option<git2::Oid>> {
        let commit = self.repo.find_commit(commit_oid)?;
        let tree = commit.tree()?;
        Ok(tree.get_path(Path::new(path)).ok().map(|entry| entry.id()))
    }

    /// Full rewrite of the working directory to match `oid`'s tree, the pull
    /// counterpart to `rewrite_working_tree`, so `cl_backup::tree::parse_dir`
    /// can read it back off disk exactly like a plain checkout.
    pub fn checkout_tree_to_disk(&self, oid: git2::Oid) -> Result<()> {
        for entry in walk_tracked(&self.dir)? {
            let _ = std::fs::remove_file(&entry);
        }

        let commit = self.repo.find_commit(oid)?;
        let tree = commit.tree()?;
        let mut io_error = None;
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }
            let Some(name) = entry.name() else {
                return git2::TreeWalkResult::Ok;
            };
            let result = entry
                .to_object(&self.repo)
                .ok()
                .and_then(|obj| obj.into_blob().ok())
                .ok_or(())
                .and_then(|blob| write_blob(&self.dir, root, name, blob.content()).map_err(|_| ()));
            match result {
                Ok(()) => git2::TreeWalkResult::Ok,
                Err(()) => {
                    io_error = Some(BackupError::Malformed {
                        what: "backup tree".to_string(),
                        reason: format!("could not write {root}{name} to disk"),
                    });
                    git2::TreeWalkResult::Abort
                }
            }
        })?;
        if let Some(e) = io_error {
            return Err(e);
        }
        Ok(())
    }

    /// Reacts to a fetched commit that failed validation: preserves it under
    /// its own branch (never lost), then force-pushes the tracked branch back
    /// to `good`'s tree with an empty commit on top explaining `reason` - or,
    /// if there is no known-good history at all yet, back to an empty root
    /// commit.
    pub fn reject_and_restore(
        &self,
        bad: git2::Oid,
        good: Option<git2::Oid>,
        reason: &str,
        private_key_pem: &str,
    ) -> Result<RejectOutcome> {
        let bad_commit = self.repo.find_commit(bad)?;
        let preserved_branch = format!("rejected/{}-{}", self.branch, &bad.to_string()[..12]);
        self.repo.branch(&preserved_branch, &bad_commit, true)?;

        let (parents, tree) = match good {
            Some(good_oid) => {
                let commit = self.repo.find_commit(good_oid)?;
                let tree = commit.tree()?;
                (vec![commit], tree)
            }
            None => {
                let empty_tree_oid = self.repo.treebuilder(None)?.write()?;
                (vec![], self.repo.find_tree(empty_tree_oid)?)
            }
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let signature = Signature::now("clouduler", "clouduler@localhost")?;
        // Not `Some(&branch_ref)`: libgit2 requires an update-ref's current
        // tip to be a parent of the new commit, which a deliberate force
        // revert past `bad` never is. Force-set the ref by hand instead.
        let revert_commit = self.repo.commit(
            None,
            &signature,
            &signature,
            reason,
            &tree,
            &parent_refs,
        )?;
        let branch_ref = format!("refs/heads/{}", self.branch);
        self.repo.reference(&branch_ref, revert_commit, true, "force revert after a rejected pull")?;

        // Keep the working directory in step with the branch that was just
        // force-pushed, or the next export tick's tree-hash comparison in
        // `sync` would see a phantom change against a tree nobody asked for.
        self.checkout_tree_to_disk(good.unwrap_or(revert_commit))?;

        let force_refspec = format!("+refs/heads/{branch}:refs/heads/{branch}", branch = self.branch);
        let preserve_refspec = format!("refs/heads/{preserved_branch}:refs/heads/{preserved_branch}");
        self.push_refspecs(&[force_refspec, preserve_refspec], private_key_pem)?;

        Ok(RejectOutcome {
            preserved_branch,
            revert_commit,
        })
    }

    /// Force-pushes whatever the local tracked branch currently points at to
    /// `origin`, overwriting the remote branch unconditionally - for
    /// `clouduler git-backup force-push`, the operator's manual recovery once
    /// the ordinary fast-forward `push` is stuck failing (the remote branch
    /// was rewritten by hand, or left mid-way through a botched rejection)
    /// and the admin has confirmed the cluster's own exported state, not
    /// whatever is on the remote, is the one to keep.
    pub fn force_push(&self, private_key_pem: &str) -> Result<()> {
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}", branch = self.branch);
        self.push_refspecs(&[refspec], private_key_pem)
    }

    /// Where trust-on-first-use host key pins for this repo's remote are
    /// kept - inside `.git` so a working-tree rewrite never touches it.
    fn known_hosts_path(&self) -> PathBuf {
        self.repo.path().join("clouduler_known_hosts")
    }
}

/// What `reject_and_restore` did with a rejected commit.
pub struct RejectOutcome {
    /// Name of the branch the rejected commit was preserved on, for the
    /// operator to go inspect and fix.
    pub preserved_branch: String,
    /// The empty commit added on top of `good`, whose message explains why
    /// the original commit was rejected.
    pub revert_commit: git2::Oid,
}

fn write_blob(dir: &Path, root: &str, name: &str, content: &[u8]) -> Result<()> {
    let path = dir.join(format!("{root}{name}"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| BackupError::Io { path: parent.to_path_buf(), source })?;
    }
    std::fs::write(&path, content).map_err(|source| BackupError::Io { path, source })
}

/// Trust-on-first-use ssh host key check: the vendored libssh2 build has no
/// system `~/.ssh/known_hosts` to fall back on (that's what produces
/// libgit2's own "unknown remote ssh hostkey" error with no callback
/// registered), so this pins whatever key a host presents the first time and
/// rejects a later mismatch instead of trusting silently forever.
fn check_host_key(known_hosts_path: &Path, cert: &Cert<'_>, host: &str) -> Result<CertificateCheckStatus> {
    let Some(hostkey) = cert.as_hostkey() else {
        // Not an ssh transport (e.g. https/file) - nothing to pin.
        return Ok(CertificateCheckStatus::CertificateOk);
    };
    let fingerprint = hostkey
        .hash_sha256()
        .map(|h| hex_encode(h))
        .ok_or_else(|| BackupError::Malformed {
            what: "ssh host key".to_string(),
            reason: "remote did not present a sha256 hostkey hash".to_string(),
        })?;

    let mut pins = load_known_hosts(known_hosts_path)?;
    match pins.get(host) {
        Some(pinned) if *pinned == fingerprint => Ok(CertificateCheckStatus::CertificateOk),
        Some(_) => Err(BackupError::HostKeyMismatch {
            host: host.to_string(),
            known_hosts: known_hosts_path.to_path_buf(),
        }),
        None => {
            pins.insert(host.to_string(), fingerprint);
            save_known_hosts(known_hosts_path, &pins)?;
            Ok(CertificateCheckStatus::CertificateOk)
        }
    }
}

fn load_known_hosts(path: &Path) -> Result<HashMap<String, String>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(source) => return Err(BackupError::Io { path: path.to_path_buf(), source }),
    };
    Ok(contents
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(host, fp)| (host.to_string(), fp.to_string()))
        .collect())
}

fn save_known_hosts(path: &Path, pins: &HashMap<String, String>) -> Result<()> {
    let mut contents = String::new();
    for (host, fingerprint) in pins {
        contents.push_str(host);
        contents.push(' ');
        contents.push_str(fingerprint);
        contents.push('\n');
    }
    std::fs::write(path, contents).map_err(|source| BackupError::Io { path: path.to_path_buf(), source })
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Clones `url` (branch `branch`) into `dir`, authenticating with whatever
/// ssh agent is already running - for `clouduler restore`, which cannot rely
/// on the cluster's own deploy key being available yet, since that key is
/// itself part of the state being recovered.
pub fn clone_with_agent(url: &str, branch: &str, dir: &Path) -> Result<()> {
    let known_hosts_path = dir.join(".git").join("clouduler_known_hosts");

    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|_url, username_from_url, _allowed| {
        Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"))
    });
    callbacks.certificate_check(move |cert, host| {
        check_host_key(&known_hosts_path, cert, host)
            .map_err(|e| git2::Error::from_str(&e.to_string()))
    });
    let mut fetch_options = git2::FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_options);
    builder.branch(branch);
    builder.clone(url, dir)?;
    Ok(())
}

/// All non-`.git` files currently on disk under `dir`, so a full rebuild can
/// clear stale entries (e.g. a removed stack/config/secret) before writing
/// the fresh set.
fn walk_tracked(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(BackupError::Io { path: current.clone(), source }),
        };
        for entry in entries {
            let entry = entry.map_err(|source| BackupError::Io { path: current.clone(), source })?;
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_round_trip_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clouduler_known_hosts");

        assert!(load_known_hosts(&path).expect("missing file reads empty").is_empty());

        let mut pins = HashMap::new();
        pins.insert("github.com".to_string(), "ab:cd:ef".to_string());
        save_known_hosts(&path, &pins).expect("save");

        assert_eq!(load_known_hosts(&path).expect("load"), pins);
    }

    /// Pushes to a local bare repo over `file://`, exercising the whole
    /// working-tree-rewrite/commit/push path. Does not cover the ssh
    /// credential callback itself — that needs a real ssh remote, flagged
    /// for a manual smoke test.
    #[test]
    fn syncs_files_to_a_local_bare_repo() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");

        let work_dir = tempfile::tempdir().expect("work tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let backup = GitBackup::open_or_init(work_dir.path(), &repo_url, "main").expect("open");

        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];

        // file:// pushes need no credentials, so the callback is never
        // invoked; this key is never actually used.
        let dummy_key = "not-a-real-key";
        let oid = backup.sync(&files, "add cluster.yaml", dummy_key).expect("sync");
        assert!(oid.is_some());
        let head = backup.repo.head().expect("head").peel_to_commit().expect("commit");
        assert_eq!(head.message(), Some("add cluster.yaml"));
    }

    #[test]
    fn a_second_sync_with_identical_content_is_a_no_op() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");

        let work_dir = tempfile::tempdir().expect("work tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let backup = GitBackup::open_or_init(work_dir.path(), &repo_url, "main").expect("open");

        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];

        let dummy_key = "not-a-real-key";
        let first = backup.sync(&files, "first", dummy_key).expect("sync");
        assert!(first.is_some());
        let second = backup.sync(&files, "second", dummy_key).expect("sync");
        assert_eq!(second, None);
    }

    /// Sets up an `origin`-having local repo (`bare` is the remote, `work` is
    /// the local working copy) with one commit already pushed, for tests that
    /// need to fetch/reject/checkout against a populated history.
    fn seeded_backup(bare: &tempfile::TempDir, work: &tempfile::TempDir, files: &[TreeFile]) -> (GitBackup, git2::Oid) {
        Repository::init_bare(bare.path()).expect("init bare");
        let repo_url = format!("file://{}", bare.path().display());
        let backup = GitBackup::open_or_init(work.path(), &repo_url, "main").expect("open");
        let dummy_key = "not-a-real-key";
        let oid = backup.sync(files, "seed", dummy_key).expect("sync").expect("a real commit");
        (backup, oid)
    }

    #[test]
    fn fetch_remote_tip_sees_what_was_pushed_from_another_working_copy() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let work_dir = tempfile::tempdir().expect("work tempdir");
        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (_pusher, pushed_oid) = seeded_backup(&bare_dir, &work_dir, &files);

        let puller_dir = tempfile::tempdir().expect("puller tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let puller = GitBackup::open_or_init(puller_dir.path(), &repo_url, "main").expect("open puller");

        let dummy_key = "not-a-real-key";
        let tip = puller.fetch_remote_tip(dummy_key).expect("fetch").expect("a tip");
        assert_eq!(tip, pushed_oid);
        // Fetching alone must not touch the local branch or working tree.
        assert_eq!(puller.local_branch_oid().expect("local oid"), None);
    }

    #[test]
    fn fetch_remote_tip_is_none_when_the_remote_branch_does_not_exist_yet() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");
        let repo_url = format!("file://{}", bare_dir.path().display());

        let puller_dir = tempfile::tempdir().expect("puller tempdir");
        let puller = GitBackup::open_or_init(puller_dir.path(), &repo_url, "main").expect("open puller");

        let dummy_key = "not-a-real-key";
        assert_eq!(puller.fetch_remote_tip(dummy_key).expect("fetch"), None);
    }

    #[test]
    fn checkout_tree_to_disk_materializes_a_past_commits_files_and_clears_stale_ones() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let work_dir = tempfile::tempdir().expect("work tempdir");
        let first_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, first_oid) = seeded_backup(&bare_dir, &work_dir, &first_files);

        let dummy_key = "not-a-real-key";
        let second_files = vec![TreeFile {
            path: "stacks/a.yaml".to_string(),
            contents: b"services: {}\n".to_vec(),
        }];
        backup.sync(&second_files, "second", dummy_key).expect("sync").expect("a real commit");
        assert!(!work_dir.path().join("cluster.yaml").exists());
        assert!(work_dir.path().join("stacks/a.yaml").exists());

        backup.checkout_tree_to_disk(first_oid).expect("checkout");
        assert!(work_dir.path().join("cluster.yaml").exists());
        assert!(!work_dir.path().join("stacks/a.yaml").exists());
    }

    #[test]
    fn secrets_subtree_changed_ignores_non_secret_churn() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let work_dir = tempfile::tempdir().expect("work tempdir");
        let first_files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: []\n".to_vec(),
            },
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: b"ciphertext-v1".to_vec(),
            },
        ];
        let (backup, first_oid) = seeded_backup(&bare_dir, &work_dir, &first_files);

        let dummy_key = "not-a-real-key";
        let second_files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: [x]\n".to_vec(),
            },
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: b"ciphertext-v1".to_vec(),
            },
        ];
        let second_oid = backup.sync(&second_files, "second", dummy_key).expect("sync").expect("a real commit");

        assert!(!backup
            .secrets_subtree_changed(Some(first_oid), second_oid)
            .expect("diff"));

        let third_files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: [x]\n".to_vec(),
            },
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: b"ciphertext-v2".to_vec(),
            },
        ];
        let third_oid = backup.sync(&third_files, "third", dummy_key).expect("sync").expect("a real commit");

        assert!(backup
            .secrets_subtree_changed(Some(second_oid), third_oid)
            .expect("diff"));
    }

    #[test]
    fn secrets_subtree_changed_treats_no_known_good_as_changed_only_if_secrets_exist() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let work_dir = tempfile::tempdir().expect("work tempdir");
        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, no_secrets_oid) = seeded_backup(&bare_dir, &work_dir, &files);
        assert!(!backup.secrets_subtree_changed(None, no_secrets_oid).expect("diff"));

        let dummy_key = "not-a-real-key";
        let with_secret = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: []\n".to_vec(),
            },
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: b"ciphertext".to_vec(),
            },
        ];
        let with_secret_oid = backup.sync(&with_secret, "add secret", dummy_key).expect("sync").expect("a real commit");
        assert!(backup.secrets_subtree_changed(None, with_secret_oid).expect("diff"));
    }

    #[test]
    fn reject_and_restore_preserves_the_bad_commit_and_force_pushes_a_revert() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let work_dir = tempfile::tempdir().expect("work tempdir");
        let good_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, good_oid) = seeded_backup(&bare_dir, &work_dir, &good_files);

        let dummy_key = "not-a-real-key";
        let bad_files = vec![TreeFile {
            path: "stacks/broken.yaml".to_string(),
            contents: b"not: valid: yaml: at: all".to_vec(),
        }];
        let bad_oid = backup.sync(&bad_files, "bad", dummy_key).expect("sync").expect("a real commit");

        let outcome = backup
            .reject_and_restore(bad_oid, Some(good_oid), "stack broken.yaml failed to parse", dummy_key)
            .expect("reject_and_restore");

        // The bad commit is preserved on its own branch, not lost.
        let preserved_ref = backup
            .repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .expect("preserved branch exists");
        assert_eq!(preserved_ref.target(), Some(bad_oid));

        // The tracked branch now points at the new revert commit, whose tree
        // matches `good` and whose parent is `good`.
        let branch_oid = backup.local_branch_oid().expect("local oid").expect("some oid");
        assert_eq!(branch_oid, outcome.revert_commit);
        let revert_commit = backup.repo.find_commit(outcome.revert_commit).expect("find revert commit");
        assert_eq!(revert_commit.message(), Some("stack broken.yaml failed to parse"));
        assert_eq!(revert_commit.parent_id(0).expect("parent"), good_oid);
        let good_commit = backup.repo.find_commit(good_oid).expect("find good commit");
        assert_eq!(revert_commit.tree().expect("tree").id(), good_commit.tree().expect("tree").id());

        // The working directory was restored to match `good`, not left as
        // the bad tree or the empty revert tree.
        assert!(work_dir.path().join("cluster.yaml").exists());
        assert!(!work_dir.path().join("stacks/broken.yaml").exists());

        // Both the force-pushed branch and the preserved branch actually
        // landed on the remote.
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        assert_eq!(
            remote_repo.find_reference("refs/heads/main").expect("remote main").target(),
            Some(outcome.revert_commit)
        );
        assert!(remote_repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .is_ok());
    }

    #[test]
    fn reject_and_restore_with_no_known_good_reverts_to_an_empty_tree() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let work_dir = tempfile::tempdir().expect("work tempdir");
        let backup = GitBackup::open_or_init(work_dir.path(), &repo_url, "main").expect("open");

        let dummy_key = "not-a-real-key";
        let bad_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"not valid".to_vec(),
        }];
        let bad_oid = backup.sync(&bad_files, "bad", dummy_key).expect("sync").expect("a real commit");

        let outcome = backup
            .reject_and_restore(bad_oid, None, "cluster.yaml failed to parse", dummy_key)
            .expect("reject_and_restore");

        let revert_commit = backup.repo.find_commit(outcome.revert_commit).expect("find revert commit");
        assert_eq!(revert_commit.parent_count(), 0);
        assert_eq!(revert_commit.tree().expect("tree").len(), 0);
        assert!(!work_dir.path().join("cluster.yaml").exists());
    }

    #[test]
    fn force_push_overwrites_a_diverged_remote() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let work_dir = tempfile::tempdir().expect("work tempdir");
        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, local_oid) = seeded_backup(&bare_dir, &work_dir, &files);

        // Simulate the remote having diverged (e.g. rewritten by hand) after
        // the local commit was made: point its branch somewhere else that is
        // not an ancestor of `local_oid`, so an ordinary fast-forward push
        // would be rejected.
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        let unrelated_tree = remote_repo.treebuilder(None).expect("treebuilder").write().expect("write");
        let unrelated_tree = remote_repo.find_tree(unrelated_tree).expect("find tree");
        let signature = Signature::now("someone-else", "someone-else@localhost").expect("signature");
        let diverged = remote_repo
            .commit(None, &signature, &signature, "diverged", &unrelated_tree, &[])
            .expect("commit");
        remote_repo
            .reference("refs/heads/main", diverged, true, "simulate divergence")
            .expect("update remote ref");

        let dummy_key = "not-a-real-key";
        backup.force_push(dummy_key).expect("force_push");

        assert_eq!(
            remote_repo.find_reference("refs/heads/main").expect("remote main").target(),
            Some(local_oid)
        );
    }
}
