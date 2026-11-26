//! Runs on every node. Watches the run for its own assignment and downloads
//! its architecture's asset, staging it for the restart route. Idempotent per
//! run id: the recorded phase is read back before any step.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cl_config::ClusterConfig;
use cl_raft::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tracing::{info, warn};

use crate::plan::{NodePhase, UpdateRunState, GITHUB_TOKEN_SECRET};
use crate::{orchestrator, release, ui};

/// Same cadence as the orchestrator's poll.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often a manager without a ui retries fetching it.
const BOOTSTRAP_RETRY: Duration = Duration::from_secs(300);

pub struct Worker {
    pub node_name: String,
    pub cluster: Arc<ClusterNode>,
    /// Live so a key delivered later enables self-update. `Arc` because
    /// `ClusterKey` is not `Clone`.
    pub cluster_key: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    pub http: reqwest::Client,
    pub cluster_config: ClusterConfig,
    /// Where this node stages the binary it downloaded, until it installs it
    /// on its next triggered restart.
    pub artifact_path: PathBuf,
    /// The node's data dir; the staged ui lives under `<data_dir>/self-update/ui`.
    pub data_dir: PathBuf,
}

impl Worker {
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        let mut last_bootstrap: Option<tokio::time::Instant> = None;
        loop {
            ticker.tick().await;
            if last_bootstrap.is_none_or(|t| t.elapsed() >= BOOTSTRAP_RETRY) {
                last_bootstrap = Some(tokio::time::Instant::now());
                self.bootstrap_ui().await;
            }
            if let Err(e) = self.tick().await {
                warn!(error = %e, "self-update worker pass failed");
            }
        }
    }

    /// A manager with no ui paired to the running backend (updated by a binary that predates the
    /// split, or the staged ui was lost) fetches the one of its own release instead of waiting
    /// for the next update run.
    async fn bootstrap_ui(&self) {
        if std::env::var_os("CL_UI_DIR").is_some_and(|d| !d.is_empty()) {
            return;
        }
        let version = cl_config::version();
        if ui::serving_dir(&self.data_dir, version).is_some() {
            return;
        }
        let state = self.cluster.snapshot().await;
        let is_manager = state
            .nodes
            .get(&self.node_name)
            .is_some_and(|n| n.role == cl_raft::state::Role::Manager);
        let Some(self_update) = self.cluster_config.self_update.as_ref() else {
            return;
        };
        if !is_manager {
            return;
        }
        let token = self.unseal_github_token().await;
        match crate::ui_bootstrap::fetch_running_ui(
            &self_update.release_url,
            token.as_deref(),
            &self.data_dir,
            version,
            false,
        )
        .await
        {
            Ok(_) => info!(%version, "downloaded the web ui for the running version"),
            Err(e) => warn!(error = %e, "cannot download the web ui for the running version"),
        }
    }

    async fn tick(&self) -> Result<(), String> {
        let state = self.cluster.snapshot().await;
        let Some((mut run, revision)) = orchestrator::load_run_revision(&state) else {
            return Ok(());
        };
        if !run.in_progress() {
            return Ok(());
        }
        let Some(phase) = run.per_node.get(&self.node_name).cloned() else {
            return Ok(());
        };

        match phase {
            NodePhase::Fetching => self.do_fetch(&mut run, revision, &state).await,
            _ => Ok(()),
        }
    }

    async fn do_fetch(
        &self,
        run: &mut UpdateRunState,
        revision: u64,
        state: &cl_raft::ClusterState,
    ) -> Result<(), String> {
        let Some(node) = state.nodes.get(&self.node_name) else {
            return Err("this node is not in the roster".into());
        };
        let arch = node.arch.clone();
        let (owner, repo, asset) = match (&run.owner, &run.repo, run.assets.get(&arch)) {
            (Some(owner), Some(repo), Some(asset)) => (owner.clone(), repo.clone(), asset.clone()),
            _ => {
                return self
                    .fail_fetch(run, revision, format!("no asset published for arch {arch}"))
                    .await;
            }
        };
        let running_hash = node.binary_hash.clone();
        // Only managers serve the ui; a worker never downloads it.
        let wants_ui = node.role == cl_raft::state::Role::Manager;
        let token = self.unseal_github_token().await;

        // Staging only: nothing live changes here. Binary first, then the ui, and the node is
        // ready only when both are in place and verified.
        if let Err(reason) = self
            .fetch_binary(
                run,
                &arch,
                &owner,
                &repo,
                &asset,
                token.as_deref(),
                &running_hash,
            )
            .await
        {
            return self.fail_fetch(run, revision, reason).await;
        }
        if wants_ui {
            if let Err(reason) = self
                .fetch_ui(run, &owner, &repo, token.as_deref(), &arch, &running_hash)
                .await
            {
                return self.fail_fetch(run, revision, reason).await;
            }
        }

        run.per_node
            .insert(self.node_name.clone(), NodePhase::Ready);
        info!(node = %self.node_name, run = %run.id, "self-update download finished");
        self.save(run.clone(), revision).await
    }

    async fn fail_fetch(
        &self,
        run: &mut UpdateRunState,
        revision: u64,
        reason: String,
    ) -> Result<(), String> {
        warn!(node = %self.node_name, %reason, "self-update fetch failed");
        run.per_node
            .insert(self.node_name.clone(), NodePhase::FetchFailed { reason });
        self.save(run.clone(), revision).await
    }

    /// Stages this arch's binary unless the node already holds exactly it.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_binary(
        &self,
        run: &mut UpdateRunState,
        arch: &str,
        owner: &str,
        repo: &str,
        asset: &release::ReleaseAsset,
        token: Option<&str>,
        running_hash: &str,
    ) -> Result<(), String> {
        let recorded = run.arch_hashes.get(arch).cloned();
        if let Some(recorded) = &recorded {
            // A lost save race re-enters this step with the same run id, and an unchanged
            // backend needs no download at all: report what is already there.
            let (path, wanted) = (self.artifact_path.clone(), recorded.clone());
            let staged = tokio::task::spawn_blocking(move || staged_matches(&path, &wanted))
                .await
                .unwrap_or(false);
            if !run.force && !binary_fetch_needed(staged, running_hash, Some(recorded), run.hold) {
                return Ok(());
            }
        }

        let http = release::download_client()?;
        let binary = release::download_asset(&http, owner, repo, asset, token).await?;
        let (hash, binary) = tokio::task::spawn_blocking(move || {
            (blake3::hash(&binary).to_hex().to_string(), binary)
        })
        .await
        .map_err(|e| format!("hashing downloaded binary: {e}"))?;
        // With a manifest the recorded hash is the published one; without, every node of an
        // architecture must at least have downloaded the same bytes as the first to report.
        check_agrees(recorded.as_ref(), &hash)?;
        let artifact_path = self.artifact_path.clone();
        tokio::task::spawn_blocking(move || stage_atomically(&artifact_path, &binary))
            .await
            .map_err(|e| format!("staging downloaded binary: {e}"))?
            .map_err(|e| format!("staging downloaded binary: {e}"))?;
        run.arch_hashes.entry(arch.to_string()).or_insert(hash);
        Ok(())
    }

    /// Stages the ui of the release, downloading only a zip this node does not hold yet. When
    /// the backend binary is unchanged there is no restart to wait for, so the new ui is
    /// switched to at once.
    async fn fetch_ui(
        &self,
        run: &UpdateRunState,
        owner: &str,
        repo: &str,
        token: Option<&str>,
        arch: &str,
        running_hash: &str,
    ) -> Result<(), String> {
        let (Some(hash), Some(asset)) = (run.ui_hash.clone(), run.ui_asset.clone()) else {
            // A release from before the ui was shipped separately: the binary embeds it.
            return Ok(());
        };
        let backend_version = run
            .backend_version
            .clone()
            .ok_or("the release manifest names a ui but no backend version")?;
        let ui_only = !run.hold
            && !run.force
            && !running_hash.is_empty()
            && run.arch_hashes.get(arch).map(String::as_str) == Some(running_hash);
        // Same binary: the running version is what the ui has to be paired with.
        let paired_with = if ui_only {
            cl_config::version().to_string()
        } else {
            backend_version
        };

        let data_dir = self.data_dir.clone();
        if !run.force && ui::has(&data_dir, &hash) {
            let (dir, hash, version) = (data_dir.clone(), hash.clone(), paired_with.clone());
            tokio::task::spawn_blocking(move || ui::mark_compatible(&dir, &hash, &version))
                .await
                .map_err(|e| format!("pairing the staged ui: {e}"))?
                .map_err(|e| format!("pairing the staged ui: {e}"))?;
        } else {
            let http = release::download_client()?;
            let zip = release::download_asset(&http, owner, repo, &asset, token).await?;
            let (dir, expect, version) = (data_dir.clone(), hash.clone(), paired_with.clone());
            tokio::task::spawn_blocking(move || ui::stage_zip(&dir, &zip, &expect, &version))
                .await
                .map_err(|e| format!("staging the ui: {e}"))?
                .map_err(|e| e.to_string())?;
        }

        if ui_only {
            let (dir, hash, version) = (data_dir, hash, paired_with);
            tokio::task::spawn_blocking(move || {
                ui::activate(&dir, &hash)?;
                ui::prune_superseded(&dir, &version);
                Ok::<(), std::io::Error>(())
            })
            .await
            .map_err(|e| format!("switching the ui: {e}"))?
            .map_err(|e| format!("switching the ui: {e}"))?;
        }
        Ok(())
    }

    async fn unseal_github_token(&self) -> Option<String> {
        let state = self.cluster.snapshot().await;
        let entry = state.secrets.get(GITHUB_TOKEN_SECRET)?;
        let sealed = Sealed::from_blob(entry.sealed.clone());
        let key = self.cluster_key.borrow().clone()?;
        let plaintext = sealed.open(&key, GITHUB_TOKEN_SECRET).ok()?;
        String::from_utf8(plaintext).ok()
    }

    /// Conditional on the revision the run was read at, so this node's report
    /// cannot overwrite what the orchestrator wrote in the meantime. A lost
    /// race is an error; the next tick re-reads and downloads again.
    async fn save(&self, run: UpdateRunState, revision: u64) -> Result<(), String> {
        let response = self
            .cluster
            .submit(orchestrator::put_run_request(&run, revision)?)
            .await
            .map_err(|e| format!("submitting run state: {e}"))?;
        if let Some(error) = response.error {
            return Err(error);
        }
        Ok(())
    }
}

/// Whether the release binary has to be downloaded. It does not when the staged file already is
/// it, or (outside a configuration reset, which installs the staged file) when the running binary
/// already is: that node is not restarted.
fn binary_fetch_needed(
    staged_matches: bool,
    running_hash: &str,
    recorded: Option<&str>,
    hold: bool,
) -> bool {
    if staged_matches {
        return false;
    }
    match recorded {
        Some(recorded) => hold || running_hash != recorded,
        None => true,
    }
}

/// Whether the file at `path` already holds the asset with blake3 `hash`.
fn staged_matches(path: &std::path::Path, hash: &str) -> bool {
    std::fs::read(path)
        .map(|bytes| blake3::hash(&bytes).to_hex().as_str() == hash)
        .unwrap_or(false)
}

/// Writes next to `path` and renames into place, so a reader (the restart
/// route executing the artifact for `--version`) sees the old file or the
/// whole new one, never a truncated or still-open one.
fn stage_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, path)
}

/// Fails if an earlier node of the same architecture recorded a different
/// hash for what should be the same asset.
fn check_agrees(recorded: Option<&String>, hash: &str) -> Result<(), String> {
    match recorded {
        Some(other) if other != hash => Err(format!(
            "downloaded binary hash {hash} differs from the {other} other nodes of this architecture got"
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_replaces_the_file_whole_and_is_recognised() {
        let dir = std::env::temp_dir().join(format!("cl-stage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact");
        stage_atomically(&path, b"one").unwrap();
        stage_atomically(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!dir.join("artifact.partial").exists());
        assert!(staged_matches(
            &path,
            blake3::hash(b"two").to_hex().as_str()
        ));
        assert!(!staged_matches(
            &path,
            blake3::hash(b"one").to_hex().as_str()
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_is_downloaded_when_the_hashes_match() {
        // staged copy is the release
        assert!(!binary_fetch_needed(true, "old", Some("new"), false));
        // the running binary is the release: no restart, no download
        assert!(!binary_fetch_needed(false, "same", Some("same"), false));
        // ...except for a reset, which installs the staged file
        assert!(binary_fetch_needed(false, "same", Some("same"), true));
        // a changed binary, or no recorded hash yet, is downloaded
        assert!(binary_fetch_needed(false, "old", Some("new"), false));
        assert!(binary_fetch_needed(false, "old", None, false));
        assert!(binary_fetch_needed(false, "", Some("new"), false));
    }

    #[test]
    fn a_download_that_disagrees_with_the_recorded_hash_is_rejected() {
        assert!(check_agrees(None, "a").is_ok());
        assert!(check_agrees(Some(&"a".to_string()), "a").is_ok());
        assert!(check_agrees(Some(&"a".to_string()), "b").is_err());
    }
}
