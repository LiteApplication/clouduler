//! The git2 IO layer: a bare repository where commits and trees are built
//! entirely via the object database (no working directory on disk). Each
//! export fetches the remote's current tip, builds a candidate tree from
//! the cluster's state, and pushes a new commit if the tree changed. Pulls
//! read fetched commits directly from the odb too — nothing is ever
//! checked out to disk except by `clouduler restore`, which clones a real
//! repo for a one-shot offline operation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use git2::cert::Cert;
use git2::{CertificateCheckStatus, Cred, PushOptions, RemoteCallbacks, Repository, RepositoryInitOptions, Signature};

use crate::error::{BackupError, Result};
use crate::tree::TreeFile;

pub struct GitBackup {
    pub(crate) repo: Repository,
    repo_url: String,
    branch: String,
}

impl GitBackup {
    /// Opens a bare repository at `dir`, initializing it (and wiring up
    /// `origin`) on first use. Bare repos have no working directory — all
    /// commits and trees live purely in the object database.
    pub fn open_or_init(dir: &Path, repo_url: &str, branch: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| BackupError::Io { path: dir.to_path_buf(), source })?;

        let repo = match Repository::open(dir) {
            Ok(repo) => repo,
            Err(_) => {
                let mut opts = RepositoryInitOptions::new();
                opts.bare(true);
                opts.initial_head(branch);
                let repo = Repository::init_opts(dir, &opts)?;
                // Bare repos need objects/tmp for libgit2's temp file operations.
                let tmp = dir.join("objects").join("tmp");
                std::fs::create_dir_all(&tmp)
                    .map_err(|source| BackupError::Io { path: tmp, source })?;
                repo
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
            repo_url: repo_url.to_string(),
            branch: branch.to_string(),
        })
    }

    pub fn repo_url(&self) -> &str {
        &self.repo_url
    }

    /// Public access to the underlying bare git2 repository — used by
    /// `pull_and_decide` to call `parse_tree` directly against the odb.
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Replaces the whole tracked tree with `files`, and if that produced
    /// any change, commits and pushes it. Returns `None` when nothing
    /// changed, so an unrelated raft-version bump doesn't produce an empty
    /// commit.
    ///
    /// Each call fetches the remote's current tip fresh (no reliance on
    /// a local branch ref surviving across ticks), builds the candidate
    /// tree entirely in the odb, and compares it against the remote tip's
    /// tree. If different, a new commit is created with the remote tip as
    /// parent and pushed. On a push rejection (remote moved concurrently),
    /// the remote tip is re-fetched and the commit is rebuilt once against
    /// the new parent.
    pub fn sync(&self, files: &[TreeFile], message: &str, private_key_pem: &str) -> Result<Option<git2::Oid>> {
        // Build the candidate tree once in the odb.
        let tree_oid = build_git_tree(&self.repo, files)?;
        let tree = self.repo.find_tree(tree_oid)?;

        // Fetch the remote's current tip.
        let remote_tip = self.fetch_remote_tip(private_key_pem)?;

        // If the remote tip's tree matches our candidate, nothing changed.
        if let Some(tip_oid) = remote_tip {
            let tip_commit = self.repo.find_commit(tip_oid)?;
            if tip_commit.tree()?.id() == tree_oid {
                return Ok(None);
            }
        }

        let signature = Signature::now("clouduler", "clouduler@localhost")?;

        // One commit-and-push attempt against a given parent.
        let attempt = |parent: Option<git2::Oid>| -> Result<git2::Oid> {
            let parents = match parent {
                Some(oid) => vec![self.repo.find_commit(oid)?],
                None => vec![],
            };
            let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
            let commit_oid = self.repo.commit(
                None, // detached — not attached to any local ref yet
                &signature,
                &signature,
                message,
                &tree,
                &parent_refs,
            )?;
            // Temporarily attach a local branch ref so `push` has
            // something to push — this ref is a push handle, not a
            // "known good" baseline.
            self.repo.reference(
                &format!("refs/heads/{}", self.branch),
                commit_oid,
                true,
                "export",
            )?;
            self.push(private_key_pem)?;
            Ok(commit_oid)
        };

        match attempt(remote_tip) {
            Ok(oid) => Ok(Some(oid)),
            Err(e) if is_push_rejection(&e) => {
                // Remote moved concurrently — re-fetch and retry once.
                let new_tip = self.fetch_remote_tip(private_key_pem)?;
                match new_tip {
                    Some(new_tip_oid) => attempt(Some(new_tip_oid)).map(Some),
                    // No remote tip at all (branch doesn't exist yet) —
                    // surface the original rejection.
                    None => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    fn push(&self, private_key_pem: &str) -> Result<()> {
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}", branch = self.branch);
        self.push_refspecs(&[refspec], private_key_pem)
    }

    /// Pushes `refspecs` to `origin`. A rejected ref update (e.g. a
    /// non-force push against a remote branch that has moved) is surfaced
    /// through the `push_update_reference` callback as
    /// `BackupError::PushRejected`.
    fn push_refspecs(&self, refspecs: &[String], private_key_pem: &str) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let known_hosts_path = self.known_hosts_path();
        let rejection: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let rejection_cb = Rc::clone(&rejection);

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
        callbacks.push_update_reference(move |refname, status| {
            if let Some(reason) = status {
                rejection_cb
                    .borrow_mut()
                    .get_or_insert_with(|| format!("{refname}: {reason}"));
            }
            Ok(())
        });

        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);

        let refs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
        remote.push(&refs, Some(&mut push_options))?;

        if let Some(reason) = rejection.borrow_mut().take() {
            return Err(BackupError::PushRejected(reason));
        }
        Ok(())
    }

    /// Fetches `origin`'s tracked branch and returns its tip commit oid,
    /// or `None` if the branch does not exist on the remote yet. Does not
    /// touch any local refs beyond the remote-tracking ref.
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

    /// Moves the local tracked branch ref to `oid` — used only as a push
    /// handle, not as a "known good" baseline.
    pub fn advance_local_ref(&self, oid: git2::Oid) -> Result<()> {
        self.repo
            .reference(&format!("refs/heads/{}", self.branch), oid, true, "advance after export/pull")?;
        Ok(())
    }

    /// Same as `advance_local_ref`, taking the oid as the hex string form.
    pub fn advance_local_ref_str(&self, oid: &str) -> Result<()> {
        let oid = git2::Oid::from_str(oid)?;
        self.advance_local_ref(oid)
    }

    /// Whether the `secrets/` subtree differs between `good_tree` and
    /// `candidate_commit`. `good_tree` is a tree oid representing what the
    /// cluster is currently running (computed fresh each call, not from a
    /// historical bookmark). `candidate_commit` is a fetched remote commit.
    /// `good_tree` being `None` counts as a change whenever `candidate`
    /// has any secrets at all.
    pub fn secrets_subtree_changed(
        &self,
        good_tree: Option<git2::Oid>,
        candidate_commit: git2::Oid,
    ) -> Result<bool> {
        let candidate_secrets = self.subtree_oid_from_commit(candidate_commit, "secrets")?;
        let good_secrets = match good_tree {
            Some(tree_oid) => self.subtree_oid_from_tree(tree_oid, "secrets")?,
            None => None,
        };
        Ok(candidate_secrets != good_secrets)
    }

    fn subtree_oid_from_commit(&self, commit_oid: git2::Oid, path: &str) -> Result<Option<git2::Oid>> {
        let commit = self.repo.find_commit(commit_oid)?;
        let tree = commit.tree()?;
        Ok(tree.get_path(Path::new(path)).ok().map(|entry| entry.id()))
    }

    fn subtree_oid_from_tree(&self, tree_oid: git2::Oid, path: &str) -> Result<Option<git2::Oid>> {
        let tree = self.repo.find_tree(tree_oid)?;
        Ok(tree.get_path(Path::new(path)).ok().map(|entry| entry.id()))
    }

    /// Reacts to a fetched commit that failed validation: preserves it under
    /// its own branch with an explanatory commit on top, then force-pushes
    /// the tracked branch to a tree representing what the cluster is
    /// actually running (`good_tree`), or a fresh empty tree when there is
    /// no known-good state.
    pub fn reject_and_restore(
        &self,
        bad: git2::Oid,
        good_tree: Option<git2::Oid>,
        reason: &str,
        private_key_pem: &str,
    ) -> Result<RejectOutcome> {
        let bad_commit = self.repo.find_commit(bad)?;
        let preserved_branch = format!("rejected/{}-{}", self.branch, &bad.to_string()[..12]);

        let signature = Signature::now("clouduler", "clouduler@localhost")?;
        // An empty-diff commit on top of `bad`, on the preserved branch only,
        // so the explanation travels with the rejected history instead of
        // polluting the tracked branch with a synthetic commit.
        let explanation_commit = self.repo.commit(
            None,
            &signature,
            &signature,
            &format!("rejected: {reason}"),
            &bad_commit.tree()?,
            &[&bad_commit],
        )?;
        self.repo.reference(
            &format!("refs/heads/{preserved_branch}"),
            explanation_commit,
            true,
            "preserve rejected commit with explanation",
        )?;

        // Build the restore tree from the "good" state, or an empty tree.
        let restore_tree = match good_tree {
            Some(tree_oid) => self.repo.find_tree(tree_oid)?,
            None => {
                let empty_tree_oid = self.repo.treebuilder(None)?.write()?;
                self.repo.find_tree(empty_tree_oid)?
            }
        };
        let restore_commit = self.repo.commit(
            None,
            &signature,
            &signature,
            "revert after rejected pull",
            &restore_tree,
            &[&bad_commit],
        )?;
        let branch_ref = format!("refs/heads/{}", self.branch);
        self.repo.reference(&branch_ref, restore_commit, true, "force revert after a rejected pull")?;

        let force_refspec = format!("+refs/heads/{branch}:refs/heads/{branch}", branch = self.branch);
        let preserve_refspec = format!("refs/heads/{preserved_branch}:refs/heads/{preserved_branch}");
        self.push_refspecs(&[force_refspec, preserve_refspec], private_key_pem)?;

        Ok(RejectOutcome {
            preserved_branch,
            revert_commit: restore_commit,
        })
    }

    /// Where trust-on-first-use host key pins for this repo's remote are
    /// kept - inside the bare repo's own `.git`-style directory.
    fn known_hosts_path(&self) -> PathBuf {
        self.repo.path().join("clouduler_known_hosts")
    }
}

/// Builds a nested git tree object from a list of `TreeFile`s, entirely via
/// the repository's object database (no working directory, no index). Returns
/// the root tree's oid. Paths use forward slashes; each segment becomes a
/// nested tree, with blobs at the leaves.
pub fn build_git_tree(repo: &Repository, files: &[TreeFile]) -> Result<git2::Oid> {
    // top-level path -> (tree_builder, or blob_oid if it's a file)
    // We'll build a nested structure: for "stacks/web.yaml", we need
    // root -> stacks/ -> web.yaml
    let mut subtrees: HashMap<String, git2::TreeBuilder> = HashMap::new();
    let mut root_entries: Vec<(String, git2::Oid, u32)> = Vec::new();

    for file in files {
        let parts: Vec<&str> = file.path.split('/').collect();
        let blob_oid = repo.blob(&file.contents)?;

        if parts.len() == 1 {
            // Top-level file.
            root_entries.push((parts[0].to_string(), blob_oid, 0o100644));
        } else {
            // Nested path: ensure each parent tree exists.
            let dir_name = parts[0];
            let file_name = parts[parts.len() - 1];
            let subtrees = subtrees
                .entry(dir_name.to_string())
                .or_insert_with(|| repo.treebuilder(None).expect("treebuilder"));
            // For deeply nested paths (e.g. a/b/c.yaml), we'd need more
            // levels, but our layout is flat one level deep (stacks/*.yaml,
            // configs/*, secrets/*.age) — so one level of nesting is enough.
            subtrees.insert(file_name, blob_oid, 0o100644)?;
        }
    }

    // Finalize subtree builders and collect their oids.
    let mut root = repo.treebuilder(None)?;
    for (name, builder) in subtrees {
        let tree_oid = builder.write()?;
        root.insert(&name, tree_oid, 0o040000)?;
    }
    for (name, blob_oid, mode) in root_entries {
        root.insert(&name, blob_oid, mode as i32)?;
    }

    let tree_oid = root.write()?;
    Ok(tree_oid)
}

/// Walks a git tree object (by oid) and extracts the four tracked kinds
/// into a `ParsedTree`, reading blob contents directly from the object
/// database — no working directory involved. Any files outside the four
/// tracked top-level paths (`cluster.yaml`, `stacks/`, `configs/`,
/// `secrets/`) are silently ignored (see the "raft should ignore
/// modifications to things that are not part of the raft" design rule).
pub fn parse_tree(repo: &Repository, tree_oid: git2::Oid) -> Result<crate::tree::ParsedTree> {
    use crate::tree::ParsedTree;
    use std::collections::BTreeMap;

    let tree = repo.find_tree(tree_oid)?;

    let cluster_yaml = tree
        .get_name("cluster.yaml")
        .map(|entry| {
            entry
                .to_object(repo)?
                .into_blob()
                .map(|b| String::from_utf8_lossy(b.content()).into_owned())
                .map_err(|_| BackupError::Malformed {
                    what: "cluster.yaml".to_string(),
                    reason: "not a blob".to_string(),
                })
        })
        .transpose()?;

    let mut stacks = BTreeMap::new();
    if let Some(stacks_tree) = tree.get_name("stacks") {
        let stacks_tree = stacks_tree
            .to_object(repo)?
            .into_tree()
            .map_err(|_| BackupError::Malformed {
                what: "stacks".to_string(),
                reason: "not a tree".to_string(),
            })?;
        for entry in stacks_tree.iter() {
            let Some(name) = entry.name() else {
                continue;
            };
            let Some(yaml_name) = name.strip_suffix(".yaml") else {
                continue;
            };
            let content = entry
                .to_object(repo)?
                .into_blob()
                .map(|b| String::from_utf8_lossy(b.content()).into_owned())
                .map_err(|_| BackupError::Malformed {
                    what: name.to_string(),
                    reason: "not a blob".to_string(),
                })?;
            stacks.insert(yaml_name.to_string(), content);
        }
    }

    let mut configs = BTreeMap::new();
    if let Some(configs_tree) = tree.get_name("configs") {
        let configs_tree = configs_tree
            .to_object(repo)?
            .into_tree()
            .map_err(|_| BackupError::Malformed {
                what: "configs".to_string(),
                reason: "not a tree".to_string(),
            })?;
        for entry in configs_tree.iter() {
            let Some(name) = entry.name() else {
                continue;
            };
            let content = entry
                .to_object(repo)?
                .into_blob()
                .map(|b| b.content().to_vec())
                .map_err(|_| BackupError::Malformed {
                    what: name.to_string(),
                    reason: "not a blob".to_string(),
                })?;
            configs.insert(name.to_string(), content);
        }
    }

    let mut secrets = BTreeMap::new();
    if let Some(secrets_tree) = tree.get_name("secrets") {
        let secrets_tree = secrets_tree
            .to_object(repo)?
            .into_tree()
            .map_err(|_| BackupError::Malformed {
                what: "secrets".to_string(),
                reason: "not a tree".to_string(),
            })?;
        for entry in secrets_tree.iter() {
            let Some(name) = entry.name() else {
                continue;
            };
            let Some(age_name) = name.strip_suffix(".age") else {
                continue;
            };
            let content = entry
                .to_object(repo)?
                .into_blob()
                .map(|b| b.content().to_vec())
                .map_err(|_| BackupError::Malformed {
                    what: name.to_string(),
                    reason: "not a blob".to_string(),
                })?;
            secrets.insert(age_name.to_string(), content);
        }
    }

    Ok(ParsedTree {
        cluster_yaml,
        stacks,
        configs,
        secrets,
    })
}

/// What `reject_and_restore` did with a rejected commit.
pub struct RejectOutcome {
    /// Name of the branch the rejected commit was preserved on (with an
    /// explanatory commit on top), for the operator to go inspect and fix.
    pub preserved_branch: String,
    /// What the tracked branch was force-pushed back to: the "good" tree
    /// committed, or a fresh empty root commit when there was no
    /// known-good state yet.
    pub revert_commit: git2::Oid,
}

/// Whether `err` represents a rejected (non-fast-forward) push.
fn is_push_rejection(err: &BackupError) -> bool {
    matches!(err, BackupError::PushRejected(_))
        || matches!(err, BackupError::Git(e) if e.code() == git2::ErrorCode::NotFastForward)
}

/// Trust-on-first-use ssh host key check.
fn check_host_key(known_hosts_path: &Path, cert: &Cert<'_>, host: &str) -> Result<CertificateCheckStatus> {
    let Some(hostkey) = cert.as_hostkey() else {
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
    /// virtual-tree/commit/push path. Does not cover the ssh
    /// credential callback itself — that needs a real ssh remote, flagged
    /// for a manual smoke test.
    #[test]
    fn syncs_files_to_a_local_bare_repo() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");

        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let backup = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");

        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];

        let dummy_key = "not-a-real-key";
        let oid = backup.sync(&files, "add cluster.yaml", dummy_key).expect("sync");
        assert!(oid.is_some());
        // The commit is reachable from the remote's branch ref.
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        let remote_tip = remote_repo
            .find_reference("refs/heads/main")
            .expect("remote main")
            .target()
            .expect("has target");
        assert_eq!(remote_tip, oid.unwrap());
        let commit = remote_repo.find_commit(remote_tip).expect("find commit");
        assert_eq!(commit.message(), Some("add cluster.yaml"));
    }

    #[test]
    fn a_second_sync_with_identical_content_is_a_no_op() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");

        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let backup = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");

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

    /// Sets up a bare repo with one commit already pushed, for tests that
    /// need to fetch/reject against a populated history.
    fn seeded_backup(bare: &tempfile::TempDir, files: &[TreeFile]) -> (GitBackup, git2::Oid, tempfile::TempDir) {
        Repository::init_bare(bare.path()).expect("init bare");
        // Bare repos need objects/tmp for libgit2 temp files.
        std::fs::create_dir_all(bare.path().join("objects/tmp")).expect("create objects/tmp");
        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare.path().display());
        let backup = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");
        let dummy_key = "not-a-real-key";
        let oid = backup.sync(files, "seed", dummy_key).expect("sync").expect("a real commit");
        (backup, oid, local_dir)
    }

    #[test]
    fn a_fresh_repo_builds_on_the_remotes_existing_history() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let first_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (_first_leader, first_oid, _local) = seeded_backup(&bare_dir, &first_files);

        // A new leader with no prior export history.
        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let second_leader = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");

        let dummy_key = "not-a-real-key";
        let second_files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: []\n".to_vec(),
            },
            TreeFile {
                path: "stacks/a.yaml".to_string(),
                contents: b"services: {}\n".to_vec(),
            },
        ];
        let second_oid = second_leader
            .sync(&second_files, "second leader's first export", dummy_key)
            .expect("sync")
            .expect("a real commit");

        // The new commit must be a fast-forward child of the remote's tip.
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        assert_eq!(
            remote_repo.find_reference("refs/heads/main").expect("remote main").target(),
            Some(second_oid)
        );
        let remote_commit = remote_repo.find_commit(second_oid).expect("find commit");
        assert_eq!(remote_commit.parent_id(0).expect("parent"), first_oid);
    }

    #[test]
    fn fetch_remote_tip_sees_what_was_pushed() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (_pusher, pushed_oid, _local) = seeded_backup(&bare_dir, &files);

        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let puller = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open puller");

        let dummy_key = "not-a-real-key";
        let tip = puller.fetch_remote_tip(dummy_key).expect("fetch").expect("a tip");
        assert_eq!(tip, pushed_oid);
    }

    #[test]
    fn fetch_remote_tip_is_none_when_the_remote_branch_does_not_exist_yet() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");
        let repo_url = format!("file://{}", bare_dir.path().display());

        let local_dir = tempfile::tempdir().expect("local tempdir");
        let puller = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open puller");

        let dummy_key = "not-a-real-key";
        assert_eq!(puller.fetch_remote_tip(dummy_key).expect("fetch"), None);
    }

    #[test]
    fn secrets_subtree_changed_ignores_non_secret_churn() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
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
        let (backup, first_oid, _local) = seeded_backup(&bare_dir, &first_files);

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

        // Extract tree oids from commits for comparison.
        let first_tree = backup.repo.find_commit(first_oid).expect("find commit").tree().expect("tree").id();
        assert!(!backup
            .secrets_subtree_changed(Some(first_tree), second_oid)
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

        let second_tree = backup.repo.find_commit(second_oid).expect("find commit").tree().expect("tree").id();
        assert!(backup
            .secrets_subtree_changed(Some(second_tree), third_oid)
            .expect("diff"));
    }

    #[test]
    fn secrets_subtree_changed_treats_no_known_good_as_changed_only_if_secrets_exist() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, no_secrets_oid, _local) = seeded_backup(&bare_dir, &files);
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
        let good_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, good_oid, _local) = seeded_backup(&bare_dir, &good_files);

        let dummy_key = "not-a-real-key";
        let bad_files = vec![TreeFile {
            path: "stacks/broken.yaml".to_string(),
            contents: b"not: valid: yaml: at: all".to_vec(),
        }];
        let bad_oid = backup.sync(&bad_files, "bad", dummy_key).expect("sync").expect("a real commit");

        // Good state is what the cluster is running: a tree from the good commit.
        let good_tree = backup.repo.find_commit(good_oid).expect("find commit").tree().expect("tree").id();
        let outcome = backup
            .reject_and_restore(bad_oid, Some(good_tree), "stack broken.yaml failed to parse", dummy_key)
            .expect("reject_and_restore");

        // The bad commit is preserved on its own branch, with an explanation on top.
        let preserved_ref = backup
            .repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .expect("preserved branch exists");
        let explanation_commit = backup
            .repo
            .find_commit(preserved_ref.target().expect("preserved target"))
            .expect("find explanation commit");
        assert_eq!(explanation_commit.message(), Some("rejected: stack broken.yaml failed to parse"));
        assert_eq!(explanation_commit.parent_id(0).expect("parent"), bad_oid);
        let bad_commit = backup.repo.find_commit(bad_oid).expect("find bad commit");
        assert_eq!(explanation_commit.tree().expect("tree").id(), bad_commit.tree().expect("tree").id());

        // The tracked branch is force-pushed to the revert commit.
        let branch_ref = format!("refs/heads/{}", backup.branch);
        let branch_oid = backup.repo.find_reference(&branch_ref).expect("branch exists").target().expect("has target");
        assert_eq!(branch_oid, outcome.revert_commit);

        // The revert commit's tree matches the good state.
        let revert_tree = backup.repo.find_commit(outcome.revert_commit).expect("find revert").tree().expect("tree").id();
        assert_eq!(revert_tree, good_tree);

        // Both refs actually landed on the remote.
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
        let local_dir = tempfile::tempdir().expect("local tempdir");
        let backup = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");

        let dummy_key = "not-a-real-key";
        let bad_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"not valid".to_vec(),
        }];
        let bad_oid = backup.sync(&bad_files, "bad", dummy_key).expect("sync").expect("a real commit");

        let outcome = backup
            .reject_and_restore(bad_oid, None, "cluster.yaml failed to parse", dummy_key)
            .expect("reject_and_restore");

        let root_commit = backup.repo.find_commit(outcome.revert_commit).expect("find root commit");
        assert_eq!(root_commit.parent_id(0).expect("parent"), bad_oid);
        assert_eq!(root_commit.tree().expect("tree").len(), 0);

        // The rejection reason lives on the preserved branch, not on main.
        let preserved_ref = backup
            .repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .expect("preserved branch exists");
        let explanation_commit = backup
            .repo
            .find_commit(preserved_ref.target().expect("preserved target"))
            .expect("find explanation commit");
        assert_eq!(explanation_commit.message(), Some("rejected: cluster.yaml failed to parse"));
        assert_eq!(explanation_commit.parent_id(0).expect("parent"), bad_oid);
    }

    /// Regression test: a rejected (non-fast-forward) push must not be
    /// silently treated as success, and the next `sync` must recover by
    /// pulling the new remote tip.
    #[test]
    fn a_rejected_push_is_recovered_by_pulling_instead_of_retried_forever() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, _local_oid, _local) = seeded_backup(&bare_dir, &files);

        // Simulate the remote moving out from under us.
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        let diverged_tree = {
            let mut builder = remote_repo.treebuilder(None).expect("treebuilder");
            let blob = remote_repo.blob(b"nodes: [diverged]\n").expect("blob");
            builder.insert("cluster.yaml", blob, 0o100644).expect("insert");
            let oid = builder.write().expect("write");
            remote_repo.find_tree(oid).expect("find tree")
        };
        let signature = Signature::now("someone-else", "someone-else@localhost").expect("signature");
        let diverged = remote_repo
            .commit(None, &signature, &signature, "diverged", &diverged_tree, &[])
            .expect("commit");
        remote_repo
            .reference("refs/heads/main", diverged, true, "simulate divergence")
            .expect("update remote ref");

        let dummy_key = "not-a-real-key";
        let new_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: [new]\n".to_vec(),
        }];

        let oid = backup
            .sync(&new_files, "export after divergence", dummy_key)
            .expect("sync should recover from the rejected push")
            .expect("a real commit");

        let remote_tip = remote_repo
            .find_reference("refs/heads/main")
            .expect("remote main")
            .target()
            .expect("remote main has a target");
        assert_eq!(remote_tip, oid, "remote should now point at the recovered export commit");

        let remote_commit = remote_repo.find_commit(remote_tip).expect("find remote commit");
        assert_eq!(
            remote_commit.parent_id(0).expect("recovered commit has a parent"),
            diverged,
            "the recovered commit should be built on top of the diverged remote tip, not orphaned"
        );
    }

    #[test]
    fn build_git_tree_produces_correct_nested_structure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init");

        let files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: []\n".to_vec(),
            },
            TreeFile {
                path: "stacks/web.yaml".to_string(),
                contents: b"services: {}\n".to_vec(),
            },
            TreeFile {
                path: "configs/nginx.conf".to_string(),
                contents: b"server {}".to_vec(),
            },
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: b"ciphertext".to_vec(),
            },
        ];

        let tree_oid = build_git_tree(&repo, &files).expect("build tree");
        let parsed = parse_tree(&repo, tree_oid).expect("parse tree");

        assert_eq!(parsed.cluster_yaml.as_deref(), Some("nodes: []\n"));
        assert_eq!(parsed.stacks.get("web").map(String::as_str), Some("services: {}\n"));
        assert_eq!(parsed.configs.get("nginx.conf").map(Vec::as_slice), Some(b"server {}".as_slice()));
        assert_eq!(parsed.secrets.get("db").map(Vec::as_slice), Some(b"ciphertext".as_slice()));
    }

    #[test]
    fn parse_tree_ignores_untracked_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init");

        let files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: []\n".to_vec(),
            },
            TreeFile {
                path: "README.md".to_string(),
                contents: b"# backup repo".to_vec(),
            },
            TreeFile {
                path: ".gitignore".to_string(),
                contents: b"target/\n".to_vec(),
            },
        ];

        let tree_oid = build_git_tree(&repo, &files).expect("build tree");
        let parsed = parse_tree(&repo, tree_oid).expect("parse tree");

        assert_eq!(parsed.cluster_yaml.as_deref(), Some("nodes: []\n"));
        assert!(parsed.stacks.is_empty());
        assert!(parsed.configs.is_empty());
        assert!(parsed.secrets.is_empty());
    }

    #[test]
    fn build_and_parse_round_trip_with_empty_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init");

        let tree_oid = build_git_tree(&repo, &[]).expect("build tree");
        let parsed = parse_tree(&repo, tree_oid).expect("parse tree");

        assert!(parsed.cluster_yaml.is_none());
        assert!(parsed.stacks.is_empty());
        assert!(parsed.configs.is_empty());
        assert!(parsed.secrets.is_empty());
    }
}
