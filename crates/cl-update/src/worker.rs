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
use crate::{orchestrator, release};

/// Same cadence as the orchestrator's poll.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

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
}

impl Worker {
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                warn!(error = %e, "self-update worker pass failed");
            }
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
        let arch = state
            .nodes
            .get(&self.node_name)
            .map(|n| n.arch.clone())
            .ok_or("this node is not in the roster")?;
        let (owner, repo, asset) = match (&run.owner, &run.repo, run.assets.get(&arch)) {
            (Some(owner), Some(repo), Some(asset)) => (owner.clone(), repo.clone(), asset.clone()),
            _ => {
                run.per_node.insert(
                    self.node_name.clone(),
                    NodePhase::FetchFailed {
                        reason: format!("no asset published for arch {arch}"),
                    },
                );
                return self.save(run.clone(), revision).await;
            }
        };

        // A lost save race re-enters this step with the same run id: when the
        // staged file already is the recorded asset, report it instead of
        // downloading and rewriting a binary the restart route may be running.
        if let Some(recorded) = run.arch_hashes.get(&arch) {
            let (path, recorded) = (self.artifact_path.clone(), recorded.clone());
            let matches = tokio::task::spawn_blocking(move || staged_matches(&path, &recorded))
                .await
                .unwrap_or(false);
            if matches {
                run.per_node
                    .insert(self.node_name.clone(), NodePhase::Ready);
                return self.save(run.clone(), revision).await;
            }
        }

        let token = self.unseal_github_token().await;
        let http = release::download_client()?;
        match release::download_asset(&http, &owner, &repo, &asset, token.as_deref()).await {
            Ok(binary) => {
                let (hash, binary) = tokio::task::spawn_blocking(move || {
                    (blake3::hash(&binary).to_hex().to_string(), binary)
                })
                .await
                .map_err(|e| format!("hashing downloaded binary: {e}"))?;
                // Releases publish no checksum to verify against, so the
                // next best thing: every node of an architecture must have
                // downloaded the same bytes as the first one to report.
                if let Err(reason) = check_agrees(run.arch_hashes.get(&arch), &hash) {
                    run.per_node
                        .insert(self.node_name.clone(), NodePhase::FetchFailed { reason });
                    return self.save(run.clone(), revision).await;
                }
                let artifact_path = self.artifact_path.clone();
                tokio::task::spawn_blocking(move || stage_atomically(&artifact_path, &binary))
                    .await
                    .map_err(|e| format!("staging downloaded binary: {e}"))?
                    .map_err(|e| format!("staging downloaded binary: {e}"))?;
                run.arch_hashes.entry(arch).or_insert(hash);
                run.per_node
                    .insert(self.node_name.clone(), NodePhase::Ready);
                info!(node = %self.node_name, run = %run.id, "self-update download finished");
            }
            Err(e) => {
                run.per_node
                    .insert(self.node_name.clone(), NodePhase::FetchFailed { reason: e });
            }
        }
        self.save(run.clone(), revision).await
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
    fn a_download_that_disagrees_with_the_recorded_hash_is_rejected() {
        assert!(check_agrees(None, "a").is_ok());
        assert!(check_agrees(Some(&"a".to_string()), "a").is_ok());
        assert!(check_agrees(Some(&"a".to_string()), "b").is_err());
    }
}
