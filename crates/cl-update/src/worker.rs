//! Runs on every node, leader or not. Watches the run's progress object for
//! instructions addressed to this node and carries them out: clone and build,
//! or fetch the artifact a peer built and stage it for the restart route to
//! install.
//!
//! Idempotent per run id: the phase already recorded is what a restarted
//! worker loop reads back before doing anything, so it never repeats a step
//! that already landed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cl_config::ClusterConfig;
use cl_docker::Docker;
use cl_raft::state::Request;
use cl_raft::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tracing::{info, warn};

use crate::plan::{NodePhase, UpdateRunState, DEPLOY_KEY_SECRET, RUN_CONFIG};
use crate::{build, git, internal_client, orchestrator, peers};

/// Same order of magnitude as the orchestrator's own poll: a worker has to
/// notice an assignment about as promptly as the leader notices progress.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct Worker {
    pub node_name: String,
    pub cluster: Arc<ClusterNode>,
    pub docker: Docker,
    pub cluster_key: ClusterKey,
    pub http: reqwest::Client,
    pub cluster_config: ClusterConfig,
    /// Where a build's checkout lives.
    pub checkout_dir: PathBuf,
    /// Where this node stages the binary it will install on its next
    /// triggered restart, whether it built it or fetched it from a peer.
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
        let Some(mut run) = orchestrator::load_run(&state) else {
            return Ok(());
        };
        if !run.in_progress() {
            return Ok(());
        }
        let Some(phase) = run.per_node.get(&self.node_name).cloned() else {
            return Ok(());
        };

        match phase {
            NodePhase::AssignedBuild => self.do_build(&mut run).await,
            NodePhase::Built { sha256 } if self.is_own_arch_builder(&run, &state) => {
                // Already holds the binary it just built, so there is
                // nothing to fetch: skip straight to ready.
                let _ = sha256;
                run.per_node.insert(self.node_name.clone(), NodePhase::Ready);
                self.save(run).await
            }
            NodePhase::AwaitingArtifact => self.do_fetch(&mut run, &state).await,
            _ => Ok(()),
        }
    }

    fn is_own_arch_builder(&self, run: &UpdateRunState, state: &cl_raft::ClusterState) -> bool {
        state
            .nodes
            .get(&self.node_name)
            .map(|n| run.arch_builders.get(&n.arch) == Some(&self.node_name))
            .unwrap_or(false)
    }

    async fn do_build(&self, run: &mut UpdateRunState) -> Result<(), String> {
        run.per_node.insert(self.node_name.clone(), NodePhase::Cloning);
        self.save_clone(run).await?;

        let private_key = self.unseal_deploy_key().await?;
        let self_update = self
            .cluster_config
            .self_update
            .as_ref()
            .ok_or("no self-update repo configured")?;

        let dest = self.checkout_dir.join(&run.id);
        let checkout = git::clone(&self_update.git_url, &self_update.git_branch, &private_key, &dest)?;
        run.target_commit.get_or_insert(checkout.commit);

        run.per_node.insert(self.node_name.clone(), NodePhase::Building);
        self.save_clone(run).await?;

        let container_name = format!("cl-self-update-build-{}", run.id);
        let spec = build::build_container_spec(&container_name, &checkout.path);
        let _ = self.docker.remove(&container_name, true).await;
        self.docker
            .create(&spec)
            .await
            .map_err(|e| format!("creating build container: {e}"))?;
        self.docker
            .start(&container_name)
            .await
            .map_err(|e| format!("starting build container: {e}"))?;

        match build::wait_and_collect(&self.docker, &container_name).await {
            Ok((binary, hash)) => {
                std::fs::write(&self.artifact_path, &binary)
                    .map_err(|e| format!("staging built binary: {e}"))?;
                run.per_node
                    .insert(self.node_name.clone(), NodePhase::Built { sha256: hash });
                info!(node = %self.node_name, run = %run.id, "self-update build finished");
            }
            Err(e) => {
                run.per_node.insert(
                    self.node_name.clone(),
                    NodePhase::BuildFailed { reason: e },
                );
            }
        }
        let _ = self.docker.remove(&container_name, true).await;
        self.save(run.clone()).await
    }

    async fn do_fetch(&self, run: &mut UpdateRunState, state: &cl_raft::ClusterState) -> Result<(), String> {
        run.per_node.insert(self.node_name.clone(), NodePhase::Fetching);
        self.save_clone(run).await?;

        let arch = state
            .nodes
            .get(&self.node_name)
            .map(|n| n.arch.clone())
            .ok_or("this node is not in the roster")?;
        let builder = run
            .arch_builders
            .get(&arch)
            .ok_or_else(|| format!("no builder recorded for arch {arch}"))?;
        let base_url = peers::base_url(&self.cluster_config, builder)
            .ok_or_else(|| format!("no address on record for {builder}"))?;

        match internal_client::fetch_artifact(&self.http, &base_url, &arch, &self.cluster_key).await {
            Ok(binary) => {
                std::fs::write(&self.artifact_path, &binary)
                    .map_err(|e| format!("staging fetched binary: {e}"))?;
                run.per_node.insert(self.node_name.clone(), NodePhase::Ready);
            }
            Err(e) => {
                run.per_node.insert(
                    self.node_name.clone(),
                    NodePhase::BuildFailed { reason: e },
                );
            }
        }
        self.save(run.clone()).await
    }

    async fn unseal_deploy_key(&self) -> Result<String, String> {
        let state = self.cluster.state().await;
        let entry = state
            .secrets
            .get(DEPLOY_KEY_SECRET)
            .ok_or("no deploy key has been generated")?;
        let sealed = Sealed::from_blob(entry.sealed.clone());
        let plaintext = sealed
            .open(&self.cluster_key, DEPLOY_KEY_SECRET)
            .map_err(|e| format!("opening the deploy key: {e}"))?;
        String::from_utf8(plaintext).map_err(|e| format!("deploy key is not valid utf8: {e}"))
    }

    async fn save_clone(&self, run: &UpdateRunState) -> Result<(), String> {
        self.save(run.clone()).await
    }

    async fn save(&self, run: UpdateRunState) -> Result<(), String> {
        let content = cl_raft::base64_encode(
            &serde_json::to_vec(&run).map_err(|e| format!("encoding run state: {e}"))?,
        );
        let response = self
            .cluster
            .submit(Request::PutConfig {
                name: RUN_CONFIG.to_string(),
                content,
                owner: None,
            })
            .await
            .map_err(|e| format!("submitting run state: {e}"))?;
        if let Some(error) = response.error {
            return Err(error);
        }
        Ok(())
    }
}
