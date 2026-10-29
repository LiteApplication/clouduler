//! The leader-gated background loop that drives one self-update run: resolves
//! the target release, waits for every node to fetch its architecture's
//! asset, then rolls the restart out one node at a time. Modeled on
//! `cl_sched::run::Scheduler`: only the leader decides, every node's own
//! worker loop carries out the parts that touch its own filesystem and its
//! own binary.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use cl_raft::state::{NodeState, Request, Role};
use cl_raft::ClusterNode;
use cl_secret::{ClusterKey, Sealed};
use tracing::{info, warn};

use crate::internal_client;
use crate::peers;
use crate::plan::{NodePhase, UpdateRunState, GITHUB_TOKEN_SECRET, RUN_CONFIG};
use crate::release;

/// How often the leader checks the run's progress. Same order of magnitude
/// as `cl_sched::run::Scheduler`'s sweep: fast enough that a stalled step
/// shows up in the ui promptly, slow enough not to spam raft with reads.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a triggered restart may take before the run is failed. Generous:
/// covers the binary swap, a services restart, and this process rejoining
/// raft and reporting a fresh `binary_hash`.
const RESTART_TIMEOUT: Duration = Duration::from_secs(180);

/// Restart order: workers first, then non-leader managers, then the leader
/// last, each group sorted by name for determinism. Keeping the leader
/// stable until the very end means the raft leadership does not move mid
/// rollout, and every node's restart is driven by the same orchestrator loop.
pub fn restart_order(nodes: &BTreeMap<String, NodeState>, leader: &str) -> Vec<String> {
    let mut workers: Vec<&String> = nodes
        .iter()
        .filter(|(name, n)| n.role == Role::Worker && name.as_str() != leader)
        .map(|(name, _)| name)
        .collect();
    workers.sort();

    let mut managers: Vec<&String> = nodes
        .iter()
        .filter(|(name, n)| n.role == Role::Manager && name.as_str() != leader)
        .map(|(name, _)| name)
        .collect();
    managers.sort();

    let mut order: Vec<String> = workers.into_iter().chain(managers).cloned().collect();
    if nodes.contains_key(leader) {
        order.push(leader.to_string());
    }
    order
}

pub struct Orchestrator {
    pub cluster: Arc<ClusterNode>,
    pub http: reqwest::Client,
    pub cluster_config: cl_config::ClusterConfig,
    /// Live rather than a snapshot: a node started with no key still gets
    /// self-update working once one is delivered to it automatically. `Arc`
    /// because `ClusterKey` is deliberately not `Clone`, and this needs to be
    /// held across an `.await` (the restart request), which a `watch::Ref`
    /// guard cannot safely do.
    pub cluster_key: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
}

impl Orchestrator {
    /// Run forever. Only does anything while this node is the raft leader and
    /// a run is in progress; a follower or an idle cluster just polls.
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
        }
    }

    async fn tick(&self) -> Result<(), String> {
        let state = self.cluster.state().await;
        let Some((mut run, revision)) = load_run_revision(&state) else {
            return Ok(());
        };
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

        if let Some(name) = restarting_node(&run) {
            self.check_restart_progress(&mut run, &state, &name);
            return self.save(run, revision).await;
        }

        let Some(leader_name) = self.leader_name(&state) else {
            return Ok(());
        };
        let order = restart_order(&state.nodes, &leader_name);
        for name in order {
            if run.per_node.get(&name) == Some(&NodePhase::Ready) {
                self.restart_one(&mut run, &name, &state).await;
                return self.save(run, revision).await;
            }
        }

        if run.is_complete() {
            run.finish(now());
            self.save(run, revision).await
        } else {
            Ok(())
        }
    }

    /// Resolve the configured release (once) into a tag and a per-arch asset
    /// map, then hand every node its assignment. Every node moves straight to
    /// `Fetching`: there is no build step left to gate on.
    async fn resolve_release(&self, mut run: UpdateRunState, revision: u64) -> Result<(), String> {
        let Some(self_update) = self.cluster_config.self_update.as_ref() else {
            run.fail("no self-update release is configured", now());
            return self.save(run, revision).await;
        };
        let token = self.unseal_github_token().await;

        match release::resolve(&self_update.release_url, token.as_deref()).await {
            Ok(resolved) => {
                run.release_tag = Some(resolved.tag.clone());
                run.owner = Some(resolved.owner);
                run.repo = Some(resolved.repo);
                run.assets = resolved.assets;
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
        let state = self.cluster.state().await;
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
            && expected_hash.as_deref() == Some(node.binary_hash.as_str());
        if back {
            run.per_node.insert(name.to_string(), NodePhase::Done);
            run.restart_started_at = None;
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
            if !needs_restart(node, run.arch_hashes.get(&node.arch).map(String::as_str)) {
                run.per_node.insert(name.to_string(), NodePhase::Done);
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

    /// Writes the run back only if nobody else has since: a worker's
    /// `Ready` report landing between this tick's read and write would
    /// otherwise be overwritten with the stale copy (and the reverse). A lost
    /// race is an error; the next tick re-reads and redoes it.
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

/// Read the current run state out of applied cluster state, if any.
pub fn load_run(state: &cl_raft::ClusterState) -> Option<UpdateRunState> {
    load_run_revision(state).map(|(run, _)| run)
}

/// `load_run`, plus the revision it was read at, for `put_run_request`.
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
            arch: arch.to_string(),
            binary_hash: String::new(),
            node_pubkey: None,
            memory_total_bytes: 0,
            version: Default::default(),
        }
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
