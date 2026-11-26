//! Leader-gated loop that drives one self-update run: resolves the target
//! release, waits for every node to fetch its asset, then rolls the restart
//! out one node at a time. Workers handle the parts touching their own node.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use cl_raft::state::{NodeState, Request, Role};
use cl_raft::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tracing::{info, warn};

use crate::internal_client;
use crate::peers;
use crate::plan::{
    NodePhase, UpdateCheck, UpdateRunState, CHECK_CONFIG, GITHUB_TOKEN_SECRET, RUN_CONFIG,
};
use crate::release;
use crate::reset::{self, ResetPhase, ResetPlan};

/// How often the leader checks the run's progress.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a triggered restart may take before the run is failed.
const RESTART_TIMEOUT: Duration = Duration::from_secs(180);

/// Restart order: workers, then non-leader managers, then the leader last (by
/// name within each group), so raft leadership does not move mid-rollout.
pub fn restart_order(nodes: &BTreeMap<String, NodeState>, leader: &str) -> Vec<String> {
    let mut workers: Vec<&String> = nodes
        .iter()
        .filter(|(name, n)| {
            n.availability == cl_raft::state::Availability::Active
                && n.role == Role::Worker
                && name.as_str() != leader
        })
        .map(|(name, _)| name)
        .collect();
    workers.sort();

    let mut managers: Vec<&String> = nodes
        .iter()
        .filter(|(name, n)| {
            n.availability == cl_raft::state::Availability::Active
                && n.role == Role::Manager
                && name.as_str() != leader
        })
        .map(|(name, _)| name)
        .collect();
    managers.sort();

    let mut order: Vec<String> = workers.into_iter().chain(managers).cloned().collect();
    if nodes
        .get(leader)
        .is_some_and(|n| n.availability == cl_raft::state::Availability::Active)
    {
        order.push(leader.to_string());
    }
    order
}

pub struct Orchestrator {
    pub cluster: Arc<ClusterNode>,
    pub http: reqwest::Client,
    pub cluster_config: cl_config::ClusterConfig,
    /// Live so a key delivered later enables self-update. `Arc` because
    /// `ClusterKey` is not `Clone` and is held across an `.await`.
    pub cluster_key: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    /// Live config, so the periodic check and interval change without a restart.
    pub config_rx: tokio::sync::watch::Receiver<Arc<cl_config::ClusterConfig>>,
}

impl Orchestrator {
    /// Run forever; only acts while this node is leader and a run is active.
    pub async fn run(self) {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        loop {
            ticker.tick().await;
            if !self.cluster.is_leader() {
                continue;
            }
            if let Err(e) = self.tick().await {
                warn!(error = %e, "self-update orchestration pass failed");
            }
            if let Err(e) = self.check_for_update().await {
                warn!(error = %e, "self-update release check failed");
            }
        }
    }

    /// Look up the latest release once the interval has passed since the stored
    /// check. Keyed on the stored timestamp so leader changes do not cause extra
    /// GitHub requests.
    async fn check_for_update(&self) -> Result<(), String> {
        let cfg = self.config_rx.borrow().clone();
        let Some(self_update) = cfg.self_update.as_ref() else {
            return Ok(());
        };
        if !self_update.check_updates {
            return Ok(());
        }

        let state = self.cluster.snapshot().await;
        let stored = state.configs.get(CHECK_CONFIG);
        let revision = stored.map(|e| e.revision).unwrap_or(0);
        let last = stored
            .and_then(|e| e.decode().ok())
            .and_then(|raw| serde_json::from_slice::<UpdateCheck>(&raw).ok());
        if let Some(last) = &last {
            if now() - last.checked_at < self_update.check_interval.as_secs() as i64 {
                return Ok(());
            }
        }
        // A rollout resolves the release itself; do not compete with it.
        if load_run(&state).is_some_and(|run| run.in_progress()) {
            return Ok(());
        }

        let token = self.unseal_github_token().await;
        let check = match release::resolve(&self_update.release_url, token.as_deref()).await {
            Ok(resolved) => {
                info!(tag = %resolved.tag, "checked for a self-update release");
                UpdateCheck {
                    checked_at: now(),
                    tag: Some(resolved.tag),
                    error: None,
                }
            }
            Err(e) => UpdateCheck {
                checked_at: now(),
                tag: None,
                error: Some(e),
            },
        };
        let content = cl_raft::base64_encode(
            &serde_json::to_vec(&check).map_err(|e| format!("encoding the check: {e}"))?,
        );
        let response = self
            .cluster
            .submit(Request::PutConfigCas {
                name: CHECK_CONFIG.to_string(),
                content,
                owner: None,
                expect_revision: revision,
            })
            .await
            .map_err(|e| format!("storing the check: {e}"))?;
        match response.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn tick(&self) -> Result<(), String> {
        let state = self.cluster.snapshot().await;
        let Some((mut run, revision)) = load_run_revision(&state) else {
            return Ok(());
        };
        if run.hold {
            self.settle_failed_reset(&state, &run).await;
        }
        if !run.in_progress() {
            return Ok(());
        }

        if run.release_tag.is_none() {
            return self.resolve_release(run, revision).await;
        }

        if run.has_failed() {
            run.fail("a node reported a fetch or restart failure", now());
            return self.save(run, revision).await;
        }

        let all_ready = run.per_node.values().all(|p| {
            matches!(
                p,
                NodePhase::Ready | NodePhase::Restarting | NodePhase::Done
            )
        });
        if !all_ready {
            return Ok(());
        }

        // A reset takes it from here: every node holds the release, none is restarted one by one.
        if run.hold {
            self.commit_reset_go(&state, &run).await?;
            run.finish(now());
            return self.save(run, revision).await;
        }

        if let Some(name) = restarting_node(&run) {
            self.check_restart_progress(&mut run, &state, &name);
            return self.save(run, revision).await;
        }

        let Some(leader_name) = self.leader_name(&state) else {
            return Ok(());
        };
        let order = restart_order(&state.nodes, &leader_name);
        let mut waiting_for_ui = false;
        for name in order {
            if run.per_node.get(&name) == Some(&NodePhase::Ready) {
                // Same binary, so no restart: a ui-only update is done once the node reports
                // serving the new ui. Keep looking at the other nodes meanwhile.
                if let Some(node) = state.nodes.get(&name) {
                    let target = run.arch_hashes.get(&node.arch).map(String::as_str);
                    if !run.force && !needs_restart(node, target) && !ui_current(node, &run) {
                        waiting_for_ui = true;
                        continue;
                    }
                }
                self.restart_one(&mut run, &name, &state).await;
                return self.save(run, revision).await;
            }
        }
        if waiting_for_ui {
            let started = *run.restart_started_at.get_or_insert_with(now);
            if now() - started > RESTART_TIMEOUT.as_secs() as i64 {
                run.fail("a manager did not start serving the new ui in time", now());
            }
            return self.save(run, revision).await;
        }

        if run.is_complete() {
            run.finish(now());
            self.save(run, revision).await
        } else {
            Ok(())
        }
    }

    /// The reset waiting on `run` goes `Go`: every node has the release staged. Idempotent.
    async fn commit_reset_go(
        &self,
        state: &cl_raft::ClusterState,
        run: &UpdateRunState,
    ) -> Result<(), String> {
        let Some((mut plan, revision)) = reset::load_revision(state) else {
            return Ok(());
        };
        if plan.phase != ResetPhase::Preparing || plan.run_id.as_deref() != Some(run.id.as_str()) {
            return Ok(());
        }
        plan.phase = ResetPlan::go_phase(&self.cluster, now());
        info!(commit = %plan.commit, "every node holds the release, starting the configuration reset");
        self.store_reset_plan(&plan, revision).await
    }

    /// The run a reset waits on stopped without every node ready (a failed download, a
    /// cancellation): the reset fails with it, before any node was touched.
    async fn settle_failed_reset(&self, state: &cl_raft::ClusterState, run: &UpdateRunState) {
        let Some(reason) = run.failure_reason.clone() else {
            return;
        };
        let Some((mut plan, revision)) = reset::load_revision(state) else {
            return;
        };
        if plan.phase != ResetPhase::Preparing || plan.run_id.as_deref() != Some(run.id.as_str()) {
            return;
        }
        warn!(%reason, "the configuration reset's release download failed, abandoning the reset");
        plan.phase = ResetPhase::Failed { reason };
        if let Err(e) = self.store_reset_plan(&plan, revision).await {
            warn!(error = %e, "could not record the failed configuration reset");
        }
    }

    async fn store_reset_plan(&self, plan: &ResetPlan, revision: u64) -> Result<(), String> {
        let response = self
            .cluster
            .submit(plan.put_request(revision)?)
            .await
            .map_err(|e| format!("submitting the reset plan: {e}"))?;
        match response.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Resolve the release once into a tag and per-arch assets, then assign
    /// every node (straight to `Fetching`).
    async fn resolve_release(&self, mut run: UpdateRunState, revision: u64) -> Result<(), String> {
        let Some(self_update) = self.cluster_config.self_update.as_ref() else {
            run.fail("no self-update release is configured", now());
            return self.save(run, revision).await;
        };
        let token = self.unseal_github_token().await;

        let source = match run.target_tag.as_deref() {
            Some(tag) => match release::pinned_source(&self_update.release_url, tag) {
                Ok(source) => source,
                Err(e) => {
                    run.fail(e, now());
                    return self.save(run, revision).await;
                }
            },
            None => self_update.release_url.clone(),
        };
        match release::resolve(&source, token.as_deref()).await {
            Ok(resolved) => {
                run.release_tag = Some(resolved.tag.clone());
                run.owner = Some(resolved.owner);
                run.repo = Some(resolved.repo);
                run.assets = resolved.assets;
                if let Some(manifest) = &resolved.manifest {
                    // The manifest publishes every hash, so nodes verify downloads against it and
                    // a restart is confirmed by it, instead of trusting the first node to report.
                    run.arch_hashes = manifest
                        .backend
                        .arch
                        .iter()
                        .map(|(arch, entry)| (arch.clone(), entry.blake3.clone()))
                        .collect();
                    run.backend_version = Some(manifest.backend.version.clone());
                    run.ui_hash = Some(manifest.ui.blake3.clone());
                    run.ui_asset = resolved.ui.clone();
                }
                for phase in run.per_node.values_mut() {
                    if *phase == NodePhase::Idle {
                        *phase = NodePhase::Fetching;
                    }
                }
                info!(tag = %resolved.tag, "self-update resolved a release");
            }
            Err(e) => run.fail(format!("resolving the release: {e}"), now()),
        }
        self.save(run, revision).await
    }

    async fn unseal_github_token(&self) -> Option<String> {
        let state = self.cluster.snapshot().await;
        let entry = state.secrets.get(GITHUB_TOKEN_SECRET)?;
        let sealed = Sealed::from_blob(entry.sealed.clone());
        let key = self.cluster_key.borrow().clone()?;
        let plaintext = sealed.open(&key, GITHUB_TOKEN_SECRET).ok()?;
        String::from_utf8(plaintext).ok()
    }

    /// Once a node is restarting, it counts as done when it is reachable
    /// again and reports the hash its arch's download produced; it counts as
    /// failed once `RESTART_TIMEOUT` passes without that.
    fn check_restart_progress(
        &self,
        run: &mut UpdateRunState,
        state: &cl_raft::ClusterState,
        name: &str,
    ) {
        let Some(node) = state.nodes.get(name) else {
            run.fail(format!("{name} left the cluster mid-restart"), now());
            return;
        };
        let expected_hash = run.arch_hashes.get(&node.arch).cloned();

        let back = !state.unreachable.contains(name)
            && expected_hash.as_deref() == Some(node.binary_hash.as_str())
            && ui_current(node, run)
            && restarted(node, run);
        if back {
            run.per_node.insert(name.to_string(), NodePhase::Done);
            run.restart_started_at = None;
            run.restart_prev_started_at = None;
            info!(node = name, "restart confirmed");
            return;
        }

        let started = run.restart_started_at.unwrap_or_else(now);
        run.restart_started_at.get_or_insert(started);
        if now() - started > RESTART_TIMEOUT.as_secs() as i64 {
            run.per_node.insert(
                name.to_string(),
                NodePhase::RestartFailed {
                    reason: "did not come back in time".into(),
                },
            );
            run.fail(
                format!("{name} did not come back in time after restart"),
                now(),
            );
        }
    }

    async fn restart_one(
        &self,
        run: &mut UpdateRunState,
        name: &str,
        state: &cl_raft::ClusterState,
    ) {
        // Already on the target binary: a restart would prove nothing (the
        // hash cannot change) and only risks quorum, so skip it. This is also
        // what keeps `check_restart_progress` honest: it confirms a restart
        // by the hash changing, which needs the hash to differ beforehand.
        if let Some(node) = state.nodes.get(name) {
            if !run.force
                && !needs_restart(node, run.arch_hashes.get(&node.arch).map(String::as_str))
            {
                run.per_node.insert(name.to_string(), NodePhase::Done);
                run.restart_started_at = None;
                info!(
                    node = name,
                    "already running the target binary, not restarting"
                );
                return;
            }
        }
        let Some(base_url) = peers::base_url(&self.cluster_config, name) else {
            run.fail(format!("no address on record for {name}"), now());
            return;
        };
        let Some(key) = self.cluster_key.borrow().clone() else {
            run.fail(
                "this node has no cluster key to sign the restart request with",
                now(),
            );
            return;
        };
        match internal_client::trigger_restart(&self.http, &base_url, &key).await {
            Ok(()) => {
                run.per_node.insert(name.to_string(), NodePhase::Restarting);
                run.restart_started_at = Some(now());
                run.restart_prev_started_at = state.nodes.get(name).map(|n| n.started_at);
                info!(node = name, "triggered restart");
            }
            Err(e) => {
                run.per_node.insert(
                    name.to_string(),
                    NodePhase::RestartFailed { reason: e.clone() },
                );
                run.fail(format!("could not trigger restart on {name}: {e}"), now());
            }
        }
    }

    fn leader_name(&self, state: &cl_raft::ClusterState) -> Option<String> {
        let leader_id = self.cluster.quorum().leader()?;
        state
            .nodes
            .values()
            .find(|node| cl_raft::node_id(node.id) == leader_id)
            .map(|node| node.name.clone())
    }

    /// Writes only if nobody else has since, so a concurrent worker report is
    /// not overwritten with a stale copy. A lost race is an error; the next tick
    /// redoes it.
    async fn save(&self, run: UpdateRunState, revision: u64) -> Result<(), String> {
        let response = self
            .cluster
            .submit(put_run_request(&run, revision)?)
            .await
            .map_err(|e| format!("submitting run state: {e}"))?;
        if let Some(error) = response.error {
            return Err(error);
        }
        Ok(())
    }
}

/// Whether `node` has to be restarted to be running the build whose hash is
/// `expected`. With no hash recorded there is nothing to compare, so restart.
pub fn needs_restart(node: &NodeState, expected: Option<&str>) -> bool {
    expected != Some(node.binary_hash.as_str())
}

/// Whether `node` came back from the restart `run` triggered. Only a forced run needs the proof: a
/// normal one is confirmed by the binary hash changing.
fn restarted(node: &NodeState, run: &UpdateRunState) -> bool {
    !run.force
        || run
            .restart_prev_started_at
            .is_none_or(|prev| node.started_at > prev)
}

/// Whether `node` serves the ui `run` rolls out. Workers serve none, and a release without a
/// manifest ships its ui inside the binary, so for both this is always true.
pub fn ui_current(node: &NodeState, run: &UpdateRunState) -> bool {
    match (&run.ui_hash, node.role) {
        (Some(expected), Role::Manager) => node.ui_hash.as_ref() == Some(expected),
        _ => true,
    }
}

/// The request that stores `run`, conditional on the object still being at
/// `revision` (0 if there is none yet). Shared by every writer of the run:
/// orchestrator, workers and the api's start/cancel.
pub fn put_run_request(run: &UpdateRunState, revision: u64) -> Result<Request, String> {
    let content = cl_raft::base64_encode(
        &serde_json::to_vec(run).map_err(|e| format!("encoding run state: {e}"))?,
    );
    Ok(Request::PutConfigCas {
        name: RUN_CONFIG.to_string(),
        content,
        owner: None,
        expect_revision: revision,
    })
}

fn restarting_node(run: &UpdateRunState) -> Option<String> {
    run.per_node
        .iter()
        .find(|(_, phase)| **phase == NodePhase::Restarting)
        .map(|(name, _)| name.clone())
}

pub fn load_run(state: &cl_raft::ClusterState) -> Option<UpdateRunState> {
    load_run_revision(state).map(|(run, _)| run)
}

pub fn load_run_revision(state: &cl_raft::ClusterState) -> Option<(UpdateRunState, u64)> {
    let entry = state.configs.get(RUN_CONFIG)?;
    let raw = entry.decode().ok()?;
    Some((serde_json::from_slice(&raw).ok()?, entry.revision))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn node(name: &str, role: Role, arch: &str) -> NodeState {
        NodeState {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            role,
            rpc_addr: "10.10.0.1:7373".parse::<SocketAddr>().unwrap(),
            drives: Default::default(),
            availability: cl_raft::state::Availability::Active,
            labels: Default::default(),
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            open_all_ports: Default::default(),
            arch: arch.to_string(),
            binary_hash: String::new(),
            ui_hash: Default::default(),
            started_at: Default::default(),
            node_pubkey: None,
            memory_total_bytes: 0,
            gpus: Default::default(),
            images: Default::default(),
            version: Default::default(),
        }
    }

    #[test]
    fn a_forced_restart_is_confirmed_by_a_later_start_not_the_hash() {
        let mut run = UpdateRunState::new("r".into(), [], 0);
        let mut n = node("n1", Role::Worker, "x86_64");
        n.started_at = 100;
        // A normal run is confirmed by the hash alone.
        assert!(restarted(&n, &run));
        run.force = true;
        run.restart_prev_started_at = Some(100);
        assert!(!restarted(&n, &run));
        n.started_at = 160;
        assert!(restarted(&n, &run));
    }

    #[test]
    fn a_node_already_on_the_target_binary_is_not_restarted() {
        let mut n = node("n1", Role::Worker, "x86_64");
        n.binary_hash = "abc".into();
        assert!(!needs_restart(&n, Some("abc")));
        assert!(needs_restart(&n, Some("def")));
        assert!(needs_restart(&n, None));
    }

    #[test]
    fn a_manager_is_current_only_once_it_serves_the_target_ui() {
        let mut run = UpdateRunState::new("r".into(), ["m".to_string()], 0);
        let mut manager = node("m", Role::Manager, "x86_64");
        let worker = node("w", Role::Worker, "x86_64");
        // No manifest: nothing to compare.
        assert!(ui_current(&manager, &run));
        run.ui_hash = Some("new".into());
        assert!(!ui_current(&manager, &run));
        assert!(ui_current(&worker, &run));
        manager.ui_hash = Some("old".into());
        assert!(!ui_current(&manager, &run));
        manager.ui_hash = Some("new".into());
        assert!(ui_current(&manager, &run));
    }

    #[test]
    fn the_run_is_stored_conditionally_on_the_revision_it_was_read_at() {
        let run = UpdateRunState::new("r".into(), ["n1".to_string()], 0);
        match put_run_request(&run, 7).expect("request") {
            Request::PutConfigCas {
                name,
                expect_revision,
                ..
            } => {
                assert_eq!(name, RUN_CONFIG);
                assert_eq!(expect_revision, 7);
            }
            other => panic!("expected a conditional put, got {other:?}"),
        }
    }

    #[test]
    fn restart_order_puts_workers_first_then_managers_then_the_leader_last() {
        let nodes = BTreeMap::from([
            (
                "leader".to_string(),
                node("leader", Role::Manager, "x86_64"),
            ),
            ("mgr2".to_string(), node("mgr2", Role::Manager, "x86_64")),
            ("w1".to_string(), node("w1", Role::Worker, "x86_64")),
            ("w2".to_string(), node("w2", Role::Worker, "x86_64")),
        ]);
        let order = restart_order(&nodes, "leader");
        assert_eq!(order, vec!["w1", "w2", "mgr2", "leader"]);
    }
}
