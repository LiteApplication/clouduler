//! Runs on every node, leader or not. Watches the run's progress object for
//! its own assignment and downloads its architecture's release asset
//! straight from GitHub, staging it for the restart route to install.
//!
//! Idempotent per run id: the phase already recorded is what a restarted
//! worker loop reads back before doing anything, so it never repeats a step
//! that already landed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cl_config::ClusterConfig;
use cl_raft::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tracing::{info, warn};

use crate::plan::{NodePhase, UpdateRunState, GITHUB_TOKEN_SECRET};
use crate::{orchestrator, release};

/// Same order of magnitude as the orchestrator's own poll: a worker has to
/// notice an assignment about as promptly as the leader notices progress.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct Worker {
    pub node_name: String,
    pub cluster: Arc<ClusterNode>,
    /// Live rather than a snapshot: a node started with no key still gets
    /// self-update working once one is delivered to it automatically. `Arc`
    /// because `ClusterKey` is deliberately not `Clone`.
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
        let state = self.cluster.state().await;
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

        let token = self.unseal_github_token().await;
        match release::download_asset(&self.http, &owner, &repo, &asset, token.as_deref()).await {
            Ok(binary) => {
                let hash = blake3::hash(&binary).to_hex().to_string();
                // Releases publish no checksum to verify against, so the
                // next best thing: every node of an architecture must have
                // downloaded the same bytes as the first one to report.
                if let Err(reason) = check_agrees(run.arch_hashes.get(&arch), &hash) {
                    run.per_node
                        .insert(self.node_name.clone(), NodePhase::FetchFailed { reason });
                    return self.save(run.clone(), revision).await;
                }
                std::fs::write(&self.artifact_path, &binary)
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
        let state = self.cluster.state().await;
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
    fn a_download_that_disagrees_with_the_recorded_hash_is_rejected() {
        assert!(check_agrees(None, "a").is_ok());
        assert!(check_agrees(Some(&"a".to_string()), "a").is_ok());
        assert!(check_agrees(Some(&"a".to_string()), "b").is_err());
    }
}
