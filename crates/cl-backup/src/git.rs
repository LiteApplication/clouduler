//! The git2 IO layer: a local working copy that gets fully rewritten each
//! tick, then staged, committed, and pushed with the cluster's ssh deploy
//! key.

use std::path::{Path, PathBuf};

use git2::{Cred, PushOptions, RemoteCallbacks, Repository, RepositoryInitOptions, Signature};

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
        let mut remote = self.repo.find_remote("origin")?;

        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, _allowed| {
            Cred::ssh_key_from_memory(
                username_from_url.unwrap_or("git"),
                None,
                private_key_pem,
                None,
            )
        });

        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);

        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}", branch = self.branch);
        remote.push(&[refspec.as_str()], Some(&mut push_options))?;
        Ok(())
    }
}

/// Clones `url` (branch `branch`) into `dir`, authenticating with whatever
/// ssh agent is already running - for `clouduler restore`, which cannot rely
/// on the cluster's own deploy key being available yet, since that key is
/// itself part of the state being recovered.
pub fn clone_with_agent(url: &str, branch: &str, dir: &Path) -> Result<()> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(|_url, username_from_url, _allowed| {
        Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"))
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
}
