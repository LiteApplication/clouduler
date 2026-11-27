//! The git2 IO layer: a bare repository with commits and trees built entirely
//! in the object database. Nothing is checked out to disk except by
//! `clouduler restore`, which clones a real repo.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use git2::cert::Cert;
use git2::{
    CertificateCheckStatus, Cred, PushOptions, RemoteCallbacks, Repository, RepositoryInitOptions,
    Signature,
};

use crate::error::{BackupError, Result};
use crate::tree::TreeFile;

/// Serializes every operation on the on-disk `git-backup` repository in this
/// process (export loop, pulls, reconcile): libgit2 ref updates and the
/// remote-tracking ref they fetch into are not safe to interleave.
pub fn lock_repo() -> parking_lot::MutexGuard<'static, ()> {
    static REPO_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    REPO_LOCK.lock()
}

pub struct GitBackup {
    pub(crate) repo: Repository,
    repo_url: String,
    branch: String,
}

impl GitBackup {
    /// Opens a bare repository at `dir`, initializing it (and `origin`) on first use.
    pub fn open_or_init(dir: &Path, repo_url: &str, branch: &str) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|source| BackupError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

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
            Ok(remote) if remote.url().ok() == Some(repo_url) => {}
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

    /// The underlying bare repository, for `parse_tree` against the odb.
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Replaces the tracked tree with `files`; if that changed anything, commits
    /// and pushes. Returns `None` when nothing changed. On a push rejection the
    /// remote tip is re-fetched and the commit rebuilt once.
    pub fn sync(
        &self,
        files: &[TreeFile],
        message: &str,
        private_key_pem: &str,
        author: Option<(&str, &str)>,
    ) -> Result<Option<git2::Oid>> {
        let (oid, changed) =
            self.commit_and_push(files, message, private_key_pem, author, "export")?;
        Ok(changed.then_some(oid))
    }

    /// Reconciles the repo with `files` on top of the remote tip, preserving
    /// unmanaged files. Returns the pushed commit OID (or the existing tip if already matching).
    pub fn reconcile(
        &self,
        files: &[TreeFile],
        message: &str,
        private_key_pem: &str,
        author: Option<(&str, &str)>,
    ) -> Result<git2::Oid> {
        Ok(self
            .commit_and_push(files, message, private_key_pem, author, "reconcile")?
            .0)
    }

    /// Builds `files` on top of the remote tip and pushes the result; a rejected push (the
    /// remote moved concurrently) re-fetches and rebuilds once. Returns the commit oid and
    /// whether a new commit was pushed (false: the tip already had that tree, its id is returned).
    fn commit_and_push(
        &self,
        files: &[TreeFile],
        message: &str,
        private_key_pem: &str,
        author: Option<(&str, &str)>,
        reflog: &str,
    ) -> Result<(git2::Oid, bool)> {
        let mut reflog = reflog.to_string();
        let mut retried = false;
        loop {
            let remote_tip = self.fetch_remote_tip(private_key_pem)?;
            let tip_commit = match remote_tip {
                Some(oid) => Some(self.repo.find_commit(oid)?),
                None => None,
            };
            let parent_tree = tip_commit.as_ref().map(|c| c.tree()).transpose()?;

            // Build the candidate tree in the odb, preserving unmanaged files from the parent tree.
            let tree_oid = build_git_tree_on_parent(&self.repo, parent_tree.as_ref(), files)?;
            let tree = self.repo.find_tree(tree_oid)?;
            if let Some(tip_commit) = &tip_commit {
                if tip_commit.tree()?.id() == tree_oid {
                    return Ok((tip_commit.id(), false));
                }
            }

            let signature = signature(author)?;
            let parents: Vec<&git2::Commit> = tip_commit.iter().collect();
            let attempt = || -> Result<git2::Oid> {
                let commit_oid = self.repo.commit(
                    None, // detached - not attached to any local ref yet
                    &signature, &signature, message, &tree, &parents,
                )?;
                // Temporarily attach a local branch ref so `push` has
                // something to push - this ref is a push handle, not a
                // "known good" baseline.
                self.repo.reference(
                    &format!("refs/heads/{}", self.branch),
                    commit_oid,
                    true,
                    &reflog,
                )?;
                self.push(private_key_pem)?;
                Ok(commit_oid)
            };
            match attempt() {
                Ok(oid) => return Ok((oid, true)),
                Err(e) if !retried && is_push_rejection(&e) => {
                    retried = true;
                    reflog = format!("{reflog} retry");
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn push(&self, private_key_pem: &str) -> Result<()> {
        let refspec = format!(
            "refs/heads/{branch}:refs/heads/{branch}",
            branch = self.branch
        );
        self.push_refspecs(&[refspec], private_key_pem)
    }

    /// Pushes `refspecs` to `origin`; a rejected ref update becomes `BackupError::PushRejected`.
    fn push_refspecs(&self, refspecs: &[String], private_key_pem: &str) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let rejection: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let rejection_cb = Rc::clone(&rejection);

        let mut callbacks = self.key_callbacks(private_key_pem);
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

    /// Fetches `origin`'s tracked branch and returns its tip oid, or `None` if the
    /// remote has no such branch yet.
    pub fn fetch_remote_tip(&self, private_key_pem: &str) -> Result<Option<git2::Oid>> {
        let mut remote = self.repo.find_remote("origin")?;
        let callbacks = self.key_callbacks(private_key_pem);

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

    /// Seeds `cache` with the secret ciphertexts the remote tip already holds,
    /// so a node with no local cache (a fresh leader) reuses them rather than
    /// re-encrypting - age output differs every time, so that would commit
    /// every secret again with identical content. Best effort: anything
    /// missing or unreadable just means those secrets get re-encrypted.
    pub fn seed_secret_cache(
        &self,
        cache: &mut crate::tree::SecretCache,
        private_key_pem: &str,
    ) -> Result<()> {
        let Some(tip) = self.fetch_remote_tip(private_key_pem)? else {
            return Ok(());
        };
        let tree = self.repo.find_commit(tip)?.tree()?;
        let blob = |path: &str| -> Option<Vec<u8>> {
            let entry = tree.get_path(Path::new(path)).ok()?;
            let blob = entry.to_object(&self.repo).ok()?.into_blob().ok()?;
            Some(blob.content().to_vec())
        };
        let revisions = blob(crate::tree::SECRET_REVISIONS_PATH)
            .and_then(|raw| serde_json::from_slice::<BTreeMap<String, u64>>(&raw).ok())
            .unwrap_or_default();
        for (name, revision) in revisions {
            if let Some(ciphertext) = blob(&format!("secrets/{name}.age")) {
                cache.seed(name, revision, ciphertext);
            }
        }
        if let Some(raw) = blob(crate::tree::TERRAFORM_REVISIONS_PATH) {
            if let Ok(serials) = serde_json::from_slice::<BTreeMap<String, u64>>(&raw) {
                for (key, serial) in serials {
                    if let Some((stack, service)) = key.split_once('/') {
                        if let Some(ciphertext) =
                            blob(&format!("terraform/{stack}/{service}.tfstate.age"))
                        {
                            cache.seed_tfstate(key, serial, ciphertext);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Moves the local tracked branch ref to `oid` — used only as a push
    /// handle, not as a "known good" baseline.
    pub fn advance_local_ref(&self, oid: git2::Oid) -> Result<()> {
        self.repo.reference(
            &format!("refs/heads/{}", self.branch),
            oid,
            true,
            "advance after export/pull",
        )?;
        Ok(())
    }

    /// Same as `advance_local_ref`, taking the oid as hex.
    pub fn advance_local_ref_str(&self, oid: &str) -> Result<()> {
        let oid = git2::Oid::from_str(oid)?;
        self.advance_local_ref(oid)
    }

    /// Whether the `secrets/` subtree differs between `good_commit` and
    /// `candidate_commit`. If `good_commit` is None, any presence of `secrets/`
    /// in `candidate_commit` is considered a change.
    pub fn secrets_subtree_changed(
        &self,
        good_commit: Option<git2::Oid>,
        candidate_commit: git2::Oid,
    ) -> Result<bool> {
        let candidate_secrets = self.subtree_oid_from_commit(candidate_commit, "secrets")?;
        let good_secrets = match good_commit {
            Some(oid) => self.subtree_oid_from_commit(oid, "secrets")?,
            None => None,
        };
        Ok(candidate_secrets != good_secrets)
    }

    /// Like `secrets_subtree_changed`, but for a cluster with no known good
    /// commit on record (a fresh leader, or a status entry lost to a failed
    /// export): the candidate's own parent is the baseline instead of "no
    /// secrets at all", so a commit that leaves `secrets/` alone is not
    /// mistaken for a secret rotation. A root commit has no parent and keeps
    /// the strict comparison.
    pub fn secrets_changed_against_baseline(
        &self,
        good_commit: Option<git2::Oid>,
        candidate_commit: git2::Oid,
    ) -> Result<bool> {
        let baseline = match good_commit {
            Some(oid) => Some(oid),
            None => self.repo.find_commit(candidate_commit)?.parent_id(0).ok(),
        };
        self.secrets_subtree_changed(baseline, candidate_commit)
    }

    /// Whether any `terraform/<stack>/<service>.tfstate.age` differs between the
    /// baseline (same rule as `secrets_changed_against_baseline`) and the
    /// candidate. The state in git is export-only, like `secrets/`.
    pub fn tfstate_changed_against_baseline(
        &self,
        good_commit: Option<git2::Oid>,
        candidate_commit: git2::Oid,
    ) -> Result<bool> {
        let baseline = match good_commit {
            Some(oid) => Some(oid),
            None => self.repo.find_commit(candidate_commit)?.parent_id(0).ok(),
        };
        let candidate = self.tfstate_blobs(Some(candidate_commit))?;
        let good = self.tfstate_blobs(baseline)?;
        Ok(candidate != good)
    }

    /// `terraform/<stack>/<service>.tfstate.age` -> blob id, for one commit.
    fn tfstate_blobs(&self, commit: Option<git2::Oid>) -> Result<BTreeMap<String, git2::Oid>> {
        let mut out = BTreeMap::new();
        let Some(oid) = commit else {
            return Ok(out);
        };
        let tree = self.repo.find_commit(oid)?.tree()?;
        let Some(tf) = tree.get_name("terraform") else {
            return Ok(out);
        };
        let Ok(tf) = tf.to_object(&self.repo)?.into_tree() else {
            return Ok(out);
        };
        for stack in tf.iter() {
            let Some(stack_name) = stack.name().ok() else {
                continue;
            };
            let Ok(stack_tree) = stack.to_object(&self.repo)?.into_tree() else {
                continue;
            };
            for entry in stack_tree.iter() {
                if let Some(name) = entry.name().ok().filter(|n| n.ends_with(".tfstate.age")) {
                    out.insert(format!("{stack_name}/{name}"), entry.id());
                }
            }
        }
        Ok(out)
    }

    fn subtree_oid_from_commit(
        &self,
        commit_oid: git2::Oid,
        path: &str,
    ) -> Result<Option<git2::Oid>> {
        let commit = self.repo.find_commit(commit_oid)?;
        let tree = commit.tree()?;
        Ok(tree.get_path(Path::new(path)).ok().map(|entry| entry.id()))
    }

    /// Reacts to a fetched commit that failed validation: preserves it under
    /// its own branch with an explanatory commit on top, then force-pushes
    /// the tracked branch back to the last known good commit (or the bad
    /// commit's parent if no known good commit was provided) - but only if
    /// the remote tip is still the bad commit when re-checked right before
    /// the force push; a remote that moved in the meantime is left alone.
    pub fn reject_and_restore(
        &self,
        bad: git2::Oid,
        good_commit: Option<git2::Oid>,
        reason: &str,
        private_key_pem: &str,
        author: Option<(&str, &str)>,
    ) -> Result<RejectOutcome> {
        let bad_commit = self.repo.find_commit(bad)?;
        let preserved_branch = format!("rejected/{}-{}", self.branch, &bad.to_string()[..12]);

        let signature = signature(author)?;
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

        // The remote may have moved on since the caller fetched `bad` (a
        // second manual push, an export from another leader term). A forced
        // push of the tracked branch would silently destroy that newer work,
        // so re-fetch right before and only force-restore if the tip is still
        // exactly the commit being rejected. Otherwise leave the branch alone
        // (only the preserved branch is pushed) - the next pull evaluates
        // whatever is on the remote now.
        let still_the_tip = self.fetch_remote_tip(private_key_pem)? == Some(bad);

        // Find the target commit to restore `self.branch` to:
        // prefer the known good commit, falling back to bad_commit's parent if available.
        let target_oid = if still_the_tip {
            good_commit.or_else(|| bad_commit.parent_id(0).ok())
        } else {
            tracing::warn!(
                %bad,
                "git backup: the remote tip moved while rejecting a pull, leaving the tracked branch untouched"
            );
            None
        };
        let preserve_refspec =
            format!("refs/heads/{preserved_branch}:refs/heads/{preserved_branch}");

        if let Some(target_oid) = target_oid {
            let branch_ref = format!("refs/heads/{}", self.branch);
            self.repo.reference(
                &branch_ref,
                target_oid,
                true,
                "force restore after a rejected pull",
            )?;

            let force_refspec = format!(
                "+refs/heads/{branch}:refs/heads/{branch}",
                branch = self.branch
            );
            self.push_refspecs(&[force_refspec, preserve_refspec], private_key_pem)?;
        } else {
            self.push_refspecs(&[preserve_refspec], private_key_pem)?;
        }

        Ok(RejectOutcome {
            preserved_branch,
            restored_commit: target_oid,
        })
    }

    /// The newest commits of the tracked branch (fetched first), newest first, at most `limit`.
    /// Empty when the remote has no such branch yet.
    pub fn list_commits(&self, private_key_pem: &str, limit: usize) -> Result<Vec<CommitInfo>> {
        let Some(tip) = self.fetch_remote_tip(private_key_pem)? else {
            return Ok(Vec::new());
        };
        let mut walk = self.repo.revwalk()?;
        walk.push(tip)?;
        walk.set_sorting(git2::Sort::TIME)?;
        let mut out = Vec::new();
        for oid in walk.take(limit) {
            let commit = self.repo.find_commit(oid?)?;
            let sha = commit.id().to_string();
            out.push(CommitInfo {
                short: sha[..12].to_string(),
                sha,
                message: String::from_utf8_lossy(commit.message_bytes())
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                author: commit.author().name().unwrap_or_default().to_string(),
                time: u64::try_from(commit.time().seconds()).unwrap_or_default(),
            });
        }
        Ok(out)
    }

    /// Resolves a full sha or unique hex prefix (at least 4 characters) to a commit in the history
    /// of `tip`.
    pub fn resolve_commit(&self, tip: git2::Oid, spec: &str) -> Result<git2::Oid> {
        let spec = spec.trim().to_ascii_lowercase();
        if spec.len() < 4 || spec.len() > 40 || !spec.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(BackupError::BadCommit(format!(
                "{spec:?} is not a commit sha (give at least 4 hex characters)"
            )));
        }
        let mut walk = self.repo.revwalk()?;
        walk.push(tip)?;
        let mut found = Vec::new();
        for oid in walk {
            let oid = oid?;
            if oid.to_string().starts_with(&spec) {
                found.push(oid);
            }
        }
        match found.len() {
            0 => Err(BackupError::BadCommit(format!(
                "no commit {spec} in the history of branch {}",
                self.branch
            ))),
            1 => Ok(found[0]),
            _ => Err(BackupError::BadCommit(format!(
                "commit prefix {spec} is ambiguous"
            ))),
        }
    }

    /// Reverts the configuration to `target` (a sha or unique prefix in the branch history) by
    /// pushing a NEW commit on top of the freshly fetched remote tip; history is never rewritten.
    /// The new tree is the target's, except `secrets/`, every `*.tfstate.age` and the bookkeeping
    /// files (revision maps, version record), which come from the tip: git cannot supply secret
    /// content and the pull path rejects any change to them. Returns `None` when that tree equals
    /// the tip's (nothing to roll back). A push rejection is not retried: the caller decides.
    pub fn rollback(
        &self,
        target: &str,
        private_key_pem: &str,
        author: Option<(&str, &str)>,
    ) -> Result<Option<git2::Oid>> {
        let tip = self.fetch_remote_tip(private_key_pem)?.ok_or_else(|| {
            BackupError::BadCommit("the remote branch has no commits to roll back to".to_string())
        })?;
        let target = self.resolve_commit(tip, target)?;
        let tip_commit = self.repo.find_commit(tip)?;
        let tree_oid =
            self.rollback_tree(&tip_commit.tree()?, &self.repo.find_commit(target)?.tree()?)?;
        if tip_commit.tree()?.id() == tree_oid {
            return Ok(None);
        }
        let tree = self.repo.find_tree(tree_oid)?;
        let signature = signature(author)?;
        let message = format!("Roll back configuration to {}", &target.to_string()[..12]);
        let commit_oid = self.repo.commit(
            None,
            &signature,
            &signature,
            &message,
            &tree,
            &[&tip_commit],
        )?;
        self.repo.reference(
            &format!("refs/heads/{}", self.branch),
            commit_oid,
            true,
            "rollback",
        )?;
        self.push(private_key_pem)?;
        Ok(Some(commit_oid))
    }

    /// `target`'s tree with the export-only parts replaced by `tip`'s (see [`Self::rollback`]).
    fn rollback_tree(&self, tip: &git2::Tree, target: &git2::Tree) -> Result<git2::Oid> {
        let mut root = self.repo.treebuilder(Some(target))?;
        let from_tip = [
            "secrets",
            crate::tree::SECRET_REVISIONS_PATH,
            crate::tree::TERRAFORM_REVISIONS_PATH,
            crate::upgrade::VERSION_PATH,
        ];
        for name in from_tip {
            match tip.get_name(name) {
                Some(entry) => {
                    root.insert(name, entry.id(), entry.filemode())?;
                }
                None => {
                    let _ = root.remove(name);
                }
            }
        }

        // terraform/<stack>/: the target's modules, the tip's states.
        type Entries = BTreeMap<String, BTreeMap<String, (git2::Oid, i32)>>;
        let collect = |tree: &git2::Tree, states: bool, out: &mut Entries| -> Result<()> {
            let Some(tf) = tree.get_name("terraform") else {
                return Ok(());
            };
            let Ok(tf) = tf.to_object(&self.repo)?.into_tree() else {
                return Ok(());
            };
            for stack in tf.iter() {
                let Some(stack_name) = stack.name().ok() else {
                    continue;
                };
                let Ok(stack_tree) = stack.to_object(&self.repo)?.into_tree() else {
                    continue;
                };
                for entry in stack_tree.iter() {
                    let Some(name) = entry.name().ok() else {
                        continue;
                    };
                    if name.ends_with(".tfstate.age") == states {
                        out.entry(stack_name.to_string())
                            .or_default()
                            .insert(name.to_string(), (entry.id(), entry.filemode()));
                    }
                }
            }
            Ok(())
        };
        let mut stacks = Entries::new();
        collect(target, false, &mut stacks)?;
        collect(tip, true, &mut stacks)?;
        if stacks.is_empty() {
            let _ = root.remove("terraform");
        } else {
            let mut tf = self.repo.treebuilder(None)?;
            for (stack, entries) in stacks {
                let mut sub = self.repo.treebuilder(None)?;
                for (name, (oid, mode)) in entries {
                    sub.insert(&name, oid, mode)?;
                }
                tf.insert(&stack, sub.write()?, 0o040000)?;
            }
            root.insert("terraform", tf.write()?, 0o040000)?;
        }
        Ok(root.write()?)
    }

    /// Fetches the remote tip and resolves `spec` (a sha or unique prefix of at least 4 hex
    /// characters) to a commit in its history, so only an ancestor of the tip is accepted. Also
    /// reads the release recorded in `.clouduler-version.json` at that commit. Pushes nothing:
    /// the caller validates the target's tree first, then calls [`Self::push_reset`].
    pub fn prepare_reset(&self, spec: &str, private_key_pem: &str) -> Result<ResetTarget> {
        let tip = self.fetch_remote_tip(private_key_pem)?.ok_or_else(|| {
            BackupError::BadCommit("the remote branch has no commits to reset to".to_string())
        })?;
        let target = self.resolve_commit(tip, spec)?;
        let version = self
            .repo
            .find_commit(target)?
            .tree()?
            .get_path(Path::new(crate::upgrade::VERSION_PATH))
            .ok()
            .and_then(|entry| entry.to_object(&self.repo).ok()?.into_blob().ok())
            .and_then(|blob| serde_json::from_slice::<serde_json::Value>(blob.content()).ok())
            .and_then(|json| json.get("version")?.as_str().map(str::to_string))
            .filter(|v| !v.is_empty());
        Ok(ResetTarget {
            tip,
            target,
            version,
        })
    }

    /// Moves the tracked branch of the remote back to `reset.target`, a force push. The old tip
    /// first goes to a new `rolled-back/<utc-timestamp>-<sha12 of the target>` branch (pushed
    /// before anything is destroyed), so nothing done since is lost. Refuses (`PushRejected`) when
    /// the remote tip is no longer the one `prepare_reset` saw. Returns the new branch name.
    pub fn push_reset(
        &self,
        reset: &ResetTarget,
        now_unix: i64,
        private_key_pem: &str,
    ) -> Result<String> {
        let branch = format!(
            "rolled-back/{}-{}",
            utc_timestamp(now_unix),
            &reset.target.to_string()[..12]
        );
        let preserved_ref = format!("refs/heads/{branch}");
        self.repo.reference(
            &preserved_ref,
            reset.tip,
            true,
            "preserve the tip before a reset",
        )?;
        if self.fetch_remote_tip(private_key_pem)? != Some(reset.tip) {
            return Err(BackupError::PushRejected(
                "the remote branch moved since the reset was prepared".to_string(),
            ));
        }
        self.push_refspecs(
            &[format!("{preserved_ref}:{preserved_ref}")],
            private_key_pem,
        )?;
        self.repo.reference(
            &format!("refs/heads/{}", self.branch),
            reset.target,
            true,
            "reset to an earlier commit",
        )?;
        let force = format!(
            "+refs/heads/{branch}:refs/heads/{branch}",
            branch = self.branch
        );
        self.push_refspecs(&[force], private_key_pem)?;
        Ok(branch)
    }

    /// Where trust-on-first-use host key pins for this repo's remote are
    /// kept - inside the bare repo's own `.git`-style directory.
    /// Remote callbacks authenticating with `private_key_pem` and pinning host keys.
    fn key_callbacks<'a>(&self, private_key_pem: &'a str) -> RemoteCallbacks<'a> {
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
        callbacks
    }

    fn known_hosts_path(&self) -> PathBuf {
        self.repo.path().join("clouduler_known_hosts")
    }
}

/// Builds a nested git tree from `TreeFile`s. Unmanaged files in `parent_tree`
/// (README.md, docs/, ...) are preserved; the cluster-managed paths are replaced.
pub fn build_git_tree_on_parent(
    repo: &Repository,
    parent_tree: Option<&git2::Tree>,
    files: &[TreeFile],
) -> Result<git2::Oid> {
    let mut root = match parent_tree {
        Some(tree) => {
            let mut builder = repo.treebuilder(Some(tree))?;
            let _ = builder.remove("cluster.yaml");
            let _ = builder.remove("cluster.yml");
            let _ = builder.remove("stacks");
            let _ = builder.remove("configs");
            let _ = builder.remove("secrets");
            let _ = builder.remove("terraform");
            let _ = builder.remove(crate::tree::SECRET_REVISIONS_PATH);
            let _ = builder.remove(crate::tree::TERRAFORM_REVISIONS_PATH);
            builder
        }
        None => repo.treebuilder(None)?,
    };

    let mut dir = TreeDir::default();
    for file in files {
        let parts: Vec<&str> = file.path.split('/').collect();
        // The layout is at most one directory deep, except `terraform/<stack>/<service>.tf`.
        // Anything deeper would keep only its last component, silently overwriting a sibling.
        let max_parts = if parts[0] == "terraform" { 3 } else { 2 };
        if parts.len() > max_parts || parts.iter().any(|p| p.is_empty()) {
            return Err(BackupError::Malformed {
                what: format!("tree path {:?}", file.path),
                reason: "expected `<name>`, `<dir>/<name>` or `terraform/<stack>/<name>`"
                    .to_string(),
            });
        }
        let blob_oid = repo.blob(&file.contents)?;
        let mut cur = &mut dir;
        for part in &parts[..parts.len() - 1] {
            cur = cur.dirs.entry((*part).to_string()).or_default();
        }
        cur.blobs
            .insert(parts[parts.len() - 1].to_string(), blob_oid);
    }

    for (name, oid, mode) in dir.entries(repo)? {
        root.insert(&name, oid, mode)?;
    }

    let tree_oid = root.write()?;
    Ok(tree_oid)
}

/// In-memory directory used to assemble nested git trees.
#[derive(Default)]
struct TreeDir {
    blobs: BTreeMap<String, git2::Oid>,
    dirs: BTreeMap<String, TreeDir>,
}

impl TreeDir {
    /// Writes the subdirectories and returns every entry (name, oid, filemode).
    fn entries(&self, repo: &Repository) -> Result<Vec<(String, git2::Oid, i32)>> {
        let mut out = Vec::new();
        for (name, sub) in &self.dirs {
            let mut builder = repo.treebuilder(None)?;
            for (n, oid, mode) in sub.entries(repo)? {
                builder.insert(&n, oid, mode)?;
            }
            out.push((name.clone(), builder.write()?, 0o040000));
        }
        for (name, oid) in &self.blobs {
            out.push((name.clone(), *oid, 0o100644));
        }
        Ok(out)
    }
}

/// Builds a tree with no parent; see [`build_git_tree_on_parent`].
pub fn build_git_tree(repo: &Repository, files: &[TreeFile]) -> Result<git2::Oid> {
    build_git_tree_on_parent(repo, None, files)
}

/// Extracts the four tracked kinds from a git tree into a `ParsedTree`, reading
/// blobs from the odb. Files outside the tracked paths are ignored (raft must
/// ignore things that are not part of the raft).
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
        let stacks_tree =
            stacks_tree
                .to_object(repo)?
                .into_tree()
                .map_err(|_| BackupError::Malformed {
                    what: "stacks".to_string(),
                    reason: "not a tree".to_string(),
                })?;
        for entry in stacks_tree.iter() {
            let Some(name) = entry.name().ok() else {
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
        let configs_tree =
            configs_tree
                .to_object(repo)?
                .into_tree()
                .map_err(|_| BackupError::Malformed {
                    what: "configs".to_string(),
                    reason: "not a tree".to_string(),
                })?;
        for entry in configs_tree.iter() {
            let Some(name) = entry.name().ok() else {
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
        let secrets_tree =
            secrets_tree
                .to_object(repo)?
                .into_tree()
                .map_err(|_| BackupError::Malformed {
                    what: "secrets".to_string(),
                    reason: "not a tree".to_string(),
                })?;
        for entry in secrets_tree.iter() {
            let Some(name) = entry.name().ok() else {
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

    let mut terraform: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut terraform_states = BTreeMap::new();
    if let Some(tf_entry) = tree.get_name("terraform") {
        let tf_tree =
            tf_entry
                .to_object(repo)?
                .into_tree()
                .map_err(|_| BackupError::Malformed {
                    what: "terraform".to_string(),
                    reason: "not a tree".to_string(),
                })?;
        for stack_entry in tf_tree.iter() {
            let Some(stack) = stack_entry.name().ok() else {
                continue;
            };
            // Stray files directly under terraform/ are ignored.
            let Ok(stack_tree) = stack_entry.to_object(repo)?.into_tree() else {
                continue;
            };
            for entry in stack_tree.iter() {
                let Some(name) = entry.name().ok() else {
                    continue;
                };
                let Ok(blob) = entry.to_object(repo)?.into_blob() else {
                    continue;
                };
                if let Some(service) = name.strip_suffix(".tfstate.age") {
                    terraform_states.insert(format!("{stack}/{service}"), blob.content().to_vec());
                } else if let Some(service) = name.strip_suffix(".tf") {
                    terraform.entry(stack.to_string()).or_default().insert(
                        service.to_string(),
                        String::from_utf8_lossy(blob.content()).into_owned(),
                    );
                }
            }
        }
    }

    Ok(ParsedTree {
        cluster_yaml,
        stacks,
        configs,
        secrets,
        terraform,
        terraform_states,
    })
}

/// One line of the commit list (`GitBackup::list_commits`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub short: String,
    /// First line of the message.
    pub message: String,
    pub author: String,
    /// Unix seconds.
    pub time: u64,
}

/// What [`GitBackup::prepare_reset`] resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetTarget {
    /// The remote tip being replaced.
    pub tip: git2::Oid,
    /// The commit to go back to; an ancestor of (or equal to) `tip`.
    pub target: git2::Oid,
    /// `version` of `.clouduler-version.json` at `target`, if it has one.
    pub version: Option<String>,
}

/// `YYYYMMDDTHHMMSSZ` for a unix time (branch names must not contain colons).
fn utc_timestamp(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

/// What `reject_and_restore` did with a rejected commit.
pub struct RejectOutcome {
    /// Name of the branch the rejected commit was preserved on (with an
    /// explanatory commit on top), for the operator to go inspect and fix.
    pub preserved_branch: String,
    /// What the tracked branch was force-pushed back to: the last known good commit
    /// or bad commit's parent, if any.
    pub restored_commit: Option<git2::Oid>,
}

/// Whether `err` represents a rejected (non-fast-forward) push.
/// The commit signature: `author`, or the default clouduler identity.
fn signature(author: Option<(&str, &str)>) -> Result<Signature<'static>> {
    Ok(match author {
        Some((name, email)) => Signature::now(name, email)?,
        None => Signature::now("clouduler", "clouduler@localhost")?,
    })
}

fn is_push_rejection(err: &BackupError) -> bool {
    matches!(err, BackupError::PushRejected(_))
        || matches!(err, BackupError::Git(e) if e.code() == git2::ErrorCode::NotFastForward)
}

/// Trust-on-first-use ssh host key check.
fn check_host_key(
    known_hosts_path: &Path,
    cert: &Cert<'_>,
    host: &str,
) -> Result<CertificateCheckStatus> {
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
        Err(source) => {
            return Err(BackupError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
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
    std::fs::write(path, contents).map_err(|source| BackupError::Io {
        path: path.to_path_buf(),
        source,
    })
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

        assert!(load_known_hosts(&path)
            .expect("missing file reads empty")
            .is_empty());

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
        let oid = backup
            .sync(&files, "add cluster.yaml", dummy_key, None)
            .expect("sync");
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
        assert_eq!(commit.message().ok(), Some("add cluster.yaml"));
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
        let first = backup.sync(&files, "first", dummy_key, None).expect("sync");
        assert!(first.is_some());
        let second = backup
            .sync(&files, "second", dummy_key, None)
            .expect("sync");
        assert_eq!(second, None);
    }

    /// Sets up a bare repo with one commit already pushed, for tests that
    /// need to fetch/reject against a populated history.
    fn seeded_backup(
        bare: &tempfile::TempDir,
        files: &[TreeFile],
    ) -> (GitBackup, git2::Oid, tempfile::TempDir) {
        Repository::init_bare(bare.path()).expect("init bare");
        // Bare repos need objects/tmp for libgit2 temp files.
        std::fs::create_dir_all(bare.path().join("objects/tmp")).expect("create objects/tmp");
        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare.path().display());
        let backup = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");
        let dummy_key = "not-a-real-key";
        let oid = backup
            .sync(files, "seed", dummy_key, None)
            .expect("sync")
            .expect("a real commit");
        (backup, oid, local_dir)
    }

    #[test]
    fn a_fresh_leader_reuses_the_ciphertext_already_committed() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let files = vec![
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: b"ciphertext-v1".to_vec(),
            },
            TreeFile {
                path: crate::tree::SECRET_REVISIONS_PATH.to_string(),
                contents: br#"{"db": 4}"#.to_vec(),
            },
        ];
        let (_first, _oid, _local) = seeded_backup(&bare_dir, &files);

        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let fresh = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");
        let mut cache = crate::tree::SecretCache::new();
        fresh
            .seed_secret_cache(&mut cache, "not-a-real-key")
            .expect("seed");
        assert_eq!(cache.0.get("db"), Some(&(4, b"ciphertext-v1".to_vec())),);
    }

    #[test]
    fn a_fresh_leader_reuses_the_committed_tfstate_ciphertext() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let files = vec![
            TreeFile {
                path: "terraform/s/w.tfstate.age".to_string(),
                contents: b"state-ct".to_vec(),
            },
            TreeFile {
                path: crate::tree::TERRAFORM_REVISIONS_PATH.to_string(),
                contents: br#"{"s/w": 7}"#.to_vec(),
            },
        ];
        let (_first, _oid, _local) = seeded_backup(&bare_dir, &files);
        let local_dir = tempfile::tempdir().expect("local tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let fresh = GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");
        let mut cache = crate::tree::SecretCache::new();
        fresh
            .seed_secret_cache(&mut cache, "not-a-real-key")
            .expect("seed");
        assert_eq!(
            cache.0.get("/tfstate/s/w"),
            Some(&(7, b"state-ct".to_vec()))
        );
    }

    fn rollback_files(
        stack: &str,
        secret: &str,
        state: &str,
        version: Option<&str>,
    ) -> Vec<TreeFile> {
        let mut files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: []\n".to_vec(),
            },
            TreeFile {
                path: "stacks/a.yaml".to_string(),
                contents: stack.as_bytes().to_vec(),
            },
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: secret.as_bytes().to_vec(),
            },
            TreeFile {
                path: "terraform/a/web.tf".to_string(),
                contents: format!("module-of-{stack}").into_bytes(),
            },
            TreeFile {
                path: "terraform/a/web.tfstate.age".to_string(),
                contents: state.as_bytes().to_vec(),
            },
            TreeFile {
                path: crate::tree::SECRET_REVISIONS_PATH.to_string(),
                contents: format!(r#"{{"db": {}}}"#, secret.len()).into_bytes(),
            },
        ];
        if let Some(v) = version {
            files.push(TreeFile {
                path: crate::upgrade::VERSION_PATH.to_string(),
                contents: v.as_bytes().to_vec(),
            });
        }
        files
    }

    fn blob_at(repo: &Repository, commit: git2::Oid, path: &str) -> Option<String> {
        let tree = repo.find_commit(commit).ok()?.tree().ok()?;
        let entry = tree.get_path(Path::new(path)).ok()?;
        let blob = entry.to_object(repo).ok()?.into_blob().ok()?;
        Some(String::from_utf8_lossy(blob.content()).into_owned())
    }

    #[test]
    fn rollback_reverts_the_config_but_keeps_secrets_state_and_bookkeeping_from_the_tip() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let (backup, first, _local) =
            seeded_backup(&bare_dir, &rollback_files("v1", "ct-1", "state-1", None));
        let key = "not-a-real-key";
        let second = backup
            .sync(
                &rollback_files("v2", "ct-22", "state-22", Some("ver-2")),
                "second",
                key,
                None,
            )
            .expect("sync")
            .expect("commit");

        let listed = backup.list_commits(key, 50).expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].sha, second.to_string());
        assert_eq!(listed[0].short, second.to_string()[..12]);
        assert_eq!(listed[0].message, "second");
        assert_eq!(listed[0].author, "clouduler");
        assert_eq!(backup.list_commits(key, 1).expect("list").len(), 1);

        let new = backup
            .rollback(
                &first.to_string()[..10],
                key,
                Some(("alice", "alice@localhost")),
            )
            .expect("rollback")
            .expect("a commit");

        let remote = Repository::open_bare(bare_dir.path()).expect("open bare");
        let head = remote
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap();
        assert_eq!(head, new);
        let commit = remote.find_commit(new).unwrap();
        assert_eq!(
            commit.parent_id(0).unwrap(),
            second,
            "a new commit on top, no rewrite"
        );
        assert_eq!(
            commit.message().unwrap(),
            format!("Roll back configuration to {}", &first.to_string()[..12])
        );
        assert_eq!(commit.author().name().ok(), Some("alice"));

        assert_eq!(
            blob_at(&remote, new, "stacks/a.yaml").as_deref(),
            Some("v1")
        );
        assert_eq!(
            blob_at(&remote, new, "terraform/a/web.tf").as_deref(),
            Some("module-of-v1")
        );
        assert_eq!(
            blob_at(&remote, new, "secrets/db.age").as_deref(),
            Some("ct-22")
        );
        assert_eq!(
            blob_at(&remote, new, "terraform/a/web.tfstate.age").as_deref(),
            Some("state-22")
        );
        assert_eq!(
            blob_at(&remote, new, crate::tree::SECRET_REVISIONS_PATH).as_deref(),
            Some(r#"{"db": 5}"#)
        );
        assert_eq!(
            blob_at(&remote, new, crate::upgrade::VERSION_PATH).as_deref(),
            Some("ver-2")
        );

        // The pull path's own checks see no secret or state change against the tip.
        assert!(!backup
            .secrets_changed_against_baseline(Some(second), new)
            .unwrap());
        assert!(!backup
            .tfstate_changed_against_baseline(Some(second), new)
            .unwrap());

        // Rolling back to where the tree already is changes nothing.
        assert_eq!(
            backup.rollback(&new.to_string(), key, None).expect("noop"),
            None
        );
    }

    #[test]
    fn rollback_rejects_unknown_ambiguous_and_malformed_commits() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let (backup, first, _local) =
            seeded_backup(&bare_dir, &rollback_files("v1", "ct-1", "state-1", None));
        let key = "not-a-real-key";
        let err = |spec: &str| {
            backup
                .rollback(spec, key, None)
                .expect_err("must fail")
                .to_string()
        };
        assert!(err("zzzz").contains("not a commit sha"));
        assert!(err("ab").contains("not a commit sha"));
        let other = if first.to_string().starts_with('0') {
            "ffff"
        } else {
            "0000"
        };
        assert!(err(other).contains("no commit"));

        // A remote with no branch at all has nothing to roll back to.
        let empty = tempfile::tempdir().expect("bare");
        Repository::init_bare(empty.path()).expect("init");
        let local = tempfile::tempdir().expect("local");
        let b = GitBackup::open_or_init(
            local.path(),
            &format!("file://{}", empty.path().display()),
            "main",
        )
        .expect("open");
        assert!(b.list_commits(key, 50).expect("list").is_empty());
        assert!(b.rollback("abcd", key, None).is_err());
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
        let second_leader =
            GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open");

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
            .sync(
                &second_files,
                "second leader's first export",
                dummy_key,
                None,
            )
            .expect("sync")
            .expect("a real commit");

        // The new commit must be a fast-forward child of the remote's tip.
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        assert_eq!(
            remote_repo
                .find_reference("refs/heads/main")
                .expect("remote main")
                .target(),
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
        let puller =
            GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open puller");

        let dummy_key = "not-a-real-key";
        let tip = puller
            .fetch_remote_tip(dummy_key)
            .expect("fetch")
            .expect("a tip");
        assert_eq!(tip, pushed_oid);
    }

    #[test]
    fn fetch_remote_tip_is_none_when_the_remote_branch_does_not_exist_yet() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        Repository::init_bare(bare_dir.path()).expect("init bare");
        let repo_url = format!("file://{}", bare_dir.path().display());

        let local_dir = tempfile::tempdir().expect("local tempdir");
        let puller =
            GitBackup::open_or_init(local_dir.path(), &repo_url, "main").expect("open puller");

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
        let second_oid = backup
            .sync(&second_files, "second", dummy_key, None)
            .expect("sync")
            .expect("a real commit");

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
        let third_oid = backup
            .sync(&third_files, "third", dummy_key, None)
            .expect("sync")
            .expect("a real commit");

        assert!(backup
            .secrets_subtree_changed(Some(second_oid), third_oid)
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
        assert!(!backup
            .secrets_subtree_changed(None, no_secrets_oid)
            .expect("diff"));

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
        let with_secret_oid = backup
            .sync(&with_secret, "add secret", dummy_key, None)
            .expect("sync")
            .expect("a real commit");
        assert!(backup
            .secrets_subtree_changed(None, with_secret_oid)
            .expect("diff"));
    }

    #[test]
    fn no_known_good_compares_secrets_against_the_parent() {
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
        let files = |cluster: &[u8], secret: &[u8]| {
            vec![
                TreeFile {
                    path: "cluster.yaml".to_string(),
                    contents: cluster.to_vec(),
                },
                TreeFile {
                    path: "secrets/db.age".to_string(),
                    contents: secret.to_vec(),
                },
            ]
        };
        let second_files = files(b"nodes: [x]\n", b"ciphertext-v1");
        let second_oid = backup
            .sync(&second_files, "second", dummy_key, None)
            .expect("sync")
            .expect("a real commit");

        // Untouched secrets/: not a rotation, even with no known good commit.
        assert!(!backup
            .secrets_changed_against_baseline(None, second_oid)
            .expect("diff"));

        let third_files = files(b"nodes: [x]\n", b"ciphertext-v2");
        let third_oid = backup
            .sync(&third_files, "third", dummy_key, None)
            .expect("sync")
            .expect("a real commit");
        assert!(backup
            .secrets_changed_against_baseline(None, third_oid)
            .expect("diff"));

        // An explicit known good commit still wins over the parent.
        assert!(backup
            .secrets_changed_against_baseline(Some(first_oid), second_oid)
            .map(|c| !c)
            .expect("diff"));
        assert!(backup
            .secrets_changed_against_baseline(Some(first_oid), third_oid)
            .expect("diff"));
    }

    #[test]
    fn utc_timestamps_are_formatted_without_colons() {
        assert_eq!(utc_timestamp(0), "19700101T000000Z");
        assert_eq!(utc_timestamp(1_700_000_000), "20231114T221320Z");
    }

    #[test]
    fn reset_force_pushes_the_target_and_keeps_the_old_tip_on_a_branch() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let key = "not-a-real-key";
        let (backup, first, _local) = seeded_backup(
            &bare_dir,
            &rollback_files("v1", "ct-1", "state-1", Some(r#"{"version": "v0.1.0"}"#)),
        );
        let second = backup
            .sync(
                &rollback_files("v2", "ct-2", "state-2", None),
                "two",
                key,
                None,
            )
            .expect("sync")
            .expect("commit");
        let third = backup
            .sync(
                &rollback_files("v3", "ct-3", "state-3", None),
                "three",
                key,
                None,
            )
            .expect("sync")
            .expect("commit");

        let prefix = &first.to_string()[..8];
        let reset = backup.prepare_reset(prefix, key).expect("prepare");
        assert_eq!(reset.tip, third);
        assert_eq!(reset.target, first);
        assert_eq!(reset.version.as_deref(), Some("v0.1.0"));

        let branch = backup.push_reset(&reset, 1_700_000_000, key).expect("push");
        assert_eq!(
            branch,
            format!("rolled-back/20231114T221320Z-{}", &first.to_string()[..12])
        );

        let remote = Repository::open_bare(bare_dir.path()).expect("open bare");
        let target_of = |name: &str| {
            remote
                .find_reference(name)
                .expect("remote ref")
                .target()
                .expect("target")
        };
        assert_eq!(target_of("refs/heads/main"), first);
        assert_eq!(target_of(&format!("refs/heads/{branch}")), third);
        // The history between the two is reachable from the preserved branch only.
        assert!(remote.graph_descendant_of(third, second).expect("graph"));
        // The local tracked ref follows so the next export commits on top of the target.
        assert_eq!(
            backup
                .repo
                .find_reference("refs/heads/main")
                .expect("local")
                .target(),
            Some(first)
        );
    }

    #[test]
    fn reset_accepts_only_commits_in_the_history_of_the_remote_tip() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let key = "not-a-real-key";
        let (backup, _first, _local) =
            seeded_backup(&bare_dir, &rollback_files("v1", "ct-1", "state-1", None));
        backup
            .sync(
                &rollback_files("v2", "ct-2", "state-2", None),
                "two",
                key,
                None,
            )
            .expect("sync");
        // A commit that exists locally but was never pushed is not an ancestor of the tip.
        let tree = backup
            .repo
            .find_commit(backup.fetch_remote_tip(key).expect("tip").expect("some"))
            .expect("commit")
            .tree()
            .expect("tree");
        let stray = backup
            .repo
            .commit(
                None,
                &Signature::now("t", "t@localhost").expect("sig"),
                &Signature::now("t", "t@localhost").expect("sig"),
                "stray",
                &tree,
                &[],
            )
            .expect("stray commit");

        for bad in [stray.to_string(), "abc".to_string(), "zzzz1234".to_string()] {
            let err = backup.prepare_reset(&bad, key).expect_err("rejected");
            assert!(matches!(err, BackupError::BadCommit(_)), "{bad}: {err}");
        }
        // The tip itself is a valid target; nothing was pushed by the failures above.
        let tip = backup.fetch_remote_tip(key).expect("tip").expect("some");
        let reset = backup.prepare_reset(&tip.to_string(), key).expect("tip");
        assert_eq!(reset.target, tip);
        assert_eq!(reset.version, None);
    }

    #[test]
    fn reset_refuses_when_the_remote_moved_after_it_was_prepared() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let key = "not-a-real-key";
        let (backup, first, _local) =
            seeded_backup(&bare_dir, &rollback_files("v1", "ct-1", "state-1", None));
        let second = backup
            .sync(
                &rollback_files("v2", "ct-2", "state-2", None),
                "two",
                key,
                None,
            )
            .expect("sync")
            .expect("commit");
        let reset = backup
            .prepare_reset(&first.to_string(), key)
            .expect("prepare");

        // Someone else pushes before the reset lands.
        let other_dir = tempfile::tempdir().expect("other tempdir");
        let repo_url = format!("file://{}", bare_dir.path().display());
        let other = GitBackup::open_or_init(other_dir.path(), &repo_url, "main").expect("open");
        other.fetch_remote_tip(key).expect("fetch");
        other
            .sync(
                &rollback_files("v9", "ct-9", "state-9", None),
                "nine",
                key,
                None,
            )
            .expect("sync");

        let err = backup.push_reset(&reset, 0, key).expect_err("moved");
        assert!(matches!(err, BackupError::PushRejected(_)), "{err}");
        let remote = Repository::open_bare(bare_dir.path()).expect("open bare");
        assert_ne!(
            remote.find_reference("refs/heads/main").unwrap().target(),
            Some(first)
        );
        let _ = second;
    }

    #[test]
    fn reject_and_restore_preserves_the_bad_commit_and_force_pushes_last_good_commit() {
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
        let bad_oid = backup
            .sync(&bad_files, "bad", dummy_key, None)
            .expect("sync")
            .expect("a real commit");

        let outcome = backup
            .reject_and_restore(
                bad_oid,
                Some(good_oid),
                "stack broken.yaml failed to parse",
                dummy_key,
                None,
            )
            .expect("reject_and_restore");

        assert_eq!(outcome.restored_commit, Some(good_oid));

        // The bad commit is preserved on its own branch, with an explanation on top.
        let preserved_ref = backup
            .repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .expect("preserved branch exists");
        let explanation_commit = backup
            .repo
            .find_commit(preserved_ref.target().expect("preserved target"))
            .expect("find explanation commit");
        assert_eq!(
            explanation_commit.message().ok(),
            Some("rejected: stack broken.yaml failed to parse")
        );
        assert_eq!(explanation_commit.parent_id(0).expect("parent"), bad_oid);
        let bad_commit = backup.repo.find_commit(bad_oid).expect("find bad commit");
        assert_eq!(
            explanation_commit.tree().expect("tree").id(),
            bad_commit.tree().expect("tree").id()
        );

        // The tracked branch is force-pushed back to the last known good commit.
        let branch_ref = format!("refs/heads/{}", backup.branch);
        let branch_oid = backup
            .repo
            .find_reference(&branch_ref)
            .expect("branch exists")
            .target()
            .expect("has target");
        assert_eq!(branch_oid, good_oid);

        // Both refs actually landed on the remote.
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        assert_eq!(
            remote_repo
                .find_reference("refs/heads/main")
                .expect("remote main")
                .target(),
            Some(good_oid)
        );
        assert!(remote_repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .is_ok());
    }

    #[test]
    fn reject_and_restore_does_not_force_push_over_a_remote_that_moved() {
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
        let bad_oid = backup
            .sync(&bad_files, "bad", dummy_key, None)
            .expect("sync")
            .expect("a real commit");

        // Somebody else pushes on top of the bad commit before the reject
        // gets to run.
        let other_dir = tempfile::tempdir().expect("other tempdir");
        let url = format!("file://{}", bare_dir.path().display());
        let other = GitBackup::open_or_init(other_dir.path(), &url, "main").expect("open");
        let newer_files = vec![TreeFile {
            path: "stacks/fixed.yaml".to_string(),
            contents: b"services: {}\n".to_vec(),
        }];
        let newer_oid = other
            .sync(&newer_files, "newer", dummy_key, None)
            .expect("sync")
            .expect("a real commit");

        let outcome = backup
            .reject_and_restore(bad_oid, Some(good_oid), "stale", dummy_key, None)
            .expect("reject_and_restore");

        assert_eq!(outcome.restored_commit, None);
        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        assert_eq!(
            remote_repo
                .find_reference("refs/heads/main")
                .expect("remote main")
                .target(),
            Some(newer_oid),
            "the newer remote commit must survive"
        );
        assert!(remote_repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .is_ok());
    }

    #[test]
    fn reject_and_restore_with_no_known_good_falls_back_to_parent() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let initial_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"nodes: []\n".to_vec(),
        }];
        let (backup, initial_oid, _local) = seeded_backup(&bare_dir, &initial_files);

        let dummy_key = "not-a-real-key";
        let bad_files = vec![TreeFile {
            path: "cluster.yaml".to_string(),
            contents: b"not valid".to_vec(),
        }];
        let bad_oid = backup
            .sync(&bad_files, "bad", dummy_key, None)
            .expect("sync")
            .expect("a real commit");

        let outcome = backup
            .reject_and_restore(
                bad_oid,
                None,
                "cluster.yaml failed to parse",
                dummy_key,
                None,
            )
            .expect("reject_and_restore");

        assert_eq!(outcome.restored_commit, Some(initial_oid));

        // The rejection reason lives on the preserved branch, not on main.
        let preserved_ref = backup
            .repo
            .find_reference(&format!("refs/heads/{}", outcome.preserved_branch))
            .expect("preserved branch exists");
        let explanation_commit = backup
            .repo
            .find_commit(preserved_ref.target().expect("preserved target"))
            .expect("find explanation commit");
        assert_eq!(
            explanation_commit.message().ok(),
            Some("rejected: cluster.yaml failed to parse")
        );
        assert_eq!(explanation_commit.parent_id(0).expect("parent"), bad_oid);

        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        assert_eq!(
            remote_repo
                .find_reference("refs/heads/main")
                .expect("remote main")
                .target(),
            Some(initial_oid)
        );
    }

    #[test]
    fn reconcile_preserves_unmanaged_files_and_updates_managed_ones() {
        let bare_dir = tempfile::tempdir().expect("bare tempdir");
        let initial_files = vec![
            TreeFile {
                path: "AGENTS.md".to_string(),
                contents: b"# Agents documentation".to_vec(),
            },
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: []\n".to_vec(),
            },
            TreeFile {
                path: "stacks/old.yaml".to_string(),
                contents: b"services: {}\n".to_vec(),
            },
        ];
        let (backup, first_oid, _local) = seeded_backup(&bare_dir, &initial_files);
        let dummy_key = "not-a-real-key";

        let raft_files = vec![
            TreeFile {
                path: "cluster.yaml".to_string(),
                contents: b"nodes: [updated]\n".to_vec(),
            },
            TreeFile {
                path: "stacks/new.yaml".to_string(),
                contents: b"services: { new: {} }\n".to_vec(),
            },
            TreeFile {
                path: "configs/app_conf".to_string(),
                contents: b"some plaintext config".to_vec(),
            },
            TreeFile {
                path: "secrets/db.age".to_string(),
                contents: b"encrypted".to_vec(),
            },
        ];

        let reconcile_oid = backup
            .reconcile(
                &raft_files,
                "reconcile: mirror cluster state",
                dummy_key,
                None,
            )
            .expect("reconcile");
        assert_ne!(reconcile_oid, first_oid);

        let parsed = parse_tree(
            backup.repo(),
            backup
                .repo()
                .find_commit(reconcile_oid)
                .unwrap()
                .tree()
                .unwrap()
                .id(),
        )
        .expect("parse tree");
        assert_eq!(parsed.cluster_yaml, Some("nodes: [updated]\n".to_string()));
        assert!(!parsed.stacks.contains_key("old"));
        assert!(parsed.stacks.contains_key("new"));
        assert!(parsed.configs.contains_key("app_conf"));
        assert!(parsed.secrets.contains_key("db"));

        // Check that AGENTS.md was preserved in the git tree
        let commit = backup.repo().find_commit(reconcile_oid).expect("commit");
        let tree = commit.tree().expect("tree");
        assert!(tree.get_name("AGENTS.md").is_some());

        // Reconciling again with the same files should return the same commit without creating a new one
        let second_oid = backup
            .reconcile(&raft_files, "reconcile again", dummy_key, None)
            .expect("second reconcile");
        assert_eq!(second_oid, reconcile_oid);
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
    fn terraform_paths_may_be_three_deep_but_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init");
        let f = |p: &str| TreeFile {
            path: p.to_string(),
            contents: b"x".to_vec(),
        };
        assert!(build_git_tree(
            &repo,
            &[
                f("terraform/a/b.tf"),
                f("terraform/a/c.tf"),
                f("stacks/a.yaml")
            ]
        )
        .is_ok());
        assert!(build_git_tree(&repo, &[f("terraform/a/b/c.tf")]).is_err());
    }

    #[test]
    fn a_changed_tfstate_is_detected_against_the_baseline() {
        let bare_dir = tempfile::tempdir().expect("tempdir");
        let f = |state: &[u8], tf: &[u8]| {
            vec![
                TreeFile {
                    path: "cluster.yaml".to_string(),
                    contents: b"a".to_vec(),
                },
                TreeFile {
                    path: "terraform/s/w.tf".to_string(),
                    contents: tf.to_vec(),
                },
                TreeFile {
                    path: "terraform/s/w.tfstate.age".to_string(),
                    contents: state.to_vec(),
                },
            ]
        };
        let (backup, first, _local) = seeded_backup(&bare_dir, &f(b"s1", b"m1"));
        let dummy_key = "not-a-real-key";
        let second = backup
            .sync(&f(b"s1", b"m2"), "edit module", dummy_key, None)
            .expect("sync")
            .expect("commit");
        let third = backup
            .sync(&f(b"s2", b"m2"), "edit state", dummy_key, None)
            .expect("sync")
            .expect("commit");
        assert!(!backup
            .tfstate_changed_against_baseline(Some(first), second)
            .unwrap());
        assert!(backup
            .tfstate_changed_against_baseline(Some(second), third)
            .unwrap());
        // No known good: the parent is the baseline.
        assert!(!backup
            .tfstate_changed_against_baseline(None, second)
            .unwrap());
        assert!(backup
            .tfstate_changed_against_baseline(None, third)
            .unwrap());
    }

    #[test]
    fn a_path_deeper_than_the_layout_is_an_error_not_flattened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init");
        let files = [
            TreeFile {
                path: "stacks/b.yaml".to_string(),
                contents: b"services: {}\n".to_vec(),
            },
            TreeFile {
                path: "stacks/a/b.yaml".to_string(),
                contents: b"services: {}\n".to_vec(),
            },
        ];
        assert!(build_git_tree(&repo, &files).is_err());
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

    #[test]
    fn test_sync_with_custom_author() {
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
        let oid = backup
            .sync(
                &files,
                "add cluster.yaml",
                dummy_key,
                Some(("alice", "alice@example.com")),
            )
            .expect("sync")
            .expect("commit oid");

        let remote_repo = Repository::open_bare(bare_dir.path()).expect("open bare");
        let commit = remote_repo.find_commit(oid).expect("find commit");
        assert_eq!(commit.author().name().ok(), Some("alice"));
        assert_eq!(commit.author().email().ok(), Some("alice@example.com"));
    }
}
