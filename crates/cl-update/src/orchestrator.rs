//! The leader-gated background loop that drives one self-update run: picks a
//! builder per architecture, waits for builds, then rolls the restart out one
//! node at a time. Modeled on `cl_sched::run::Scheduler`: only the leader
//! decides, every node's own worker loop carries out the parts that touch
//! its own docker and its own binary.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use cl_raft::state::{NodeState, Request, Role};
use cl_raft::ClusterNode;
use cl_secret::ClusterKey;
use tracing::{info, warn};

use crate::internal_client;
use crate::peers;
use crate::plan::{NodePhase, UpdateRunState, RUN_CONFIG};

/// How often the leader checks the run's progress. Same order of magnitude
/// as `cl_sched::run::Scheduler`'s sweep: fast enough that a stalled step
/// shows up in the ui promptly, slow enough not to spam raft with reads.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long a triggered restart may take before the run is failed. Generous:
/// covers the binary swap, a services restart, and this process rejoining
/// raft and reporting a fresh `binary_hash`.
const RESTART_TIMEOUT: Duration = Duration::from_secs(180);

/// One reachable, schedulable node per distinct arch, chosen deterministically
/// (lowest name) so a re-run of this function against unchanged input always
/// picks the same builder.
pub fn pick_builders(
    nodes: &BTreeMap<String, NodeState>,
    unreachable: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let mut builders = BTreeMap::new();
    for (name, node) in nodes {
        if unreachable.contains(name) || node.arch.is_empty() {
            continue;
        }
        builders
            .entry(node.arch.clone())
            .or_insert_with(|| name.clone());
    }
    builders
}

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

/// Once every node building for an arch reports `Built`, every other node of
/// that arch that is still `Idle` moves to `AwaitingArtifact`. Nodes doing the
/// building for their own arch skip straight from `Built` to `Ready` via the
/// worker loop, since they already hold the binary; this only concerns
/// everyone else.
pub fn advance_awaiting_artifact(run: &mut UpdateRunState, nodes: &BTreeMap<String, NodeState>) {
    for (arch, builder) in run.arch_builders.clone() {
        let built = matches!(run.per_node.get(&builder), Some(NodePhase::Built { .. }));
        if !built {
            continue;
        }
        for (name, node) in nodes {
            if node.arch != arch || name == &builder {
                continue;
            }
            if run.per_node.get(name) == Some(&NodePhase::Idle) {
                run.per_node.insert(name.clone(), NodePhase::AwaitingArtifact);
            }
        }
    }
}

pub struct Orchestrator {
    pub cluster: Arc<ClusterNode>,
    pub http: reqwest::Client,
    pub cluster_config: cl_config::ClusterConfig,
    pub cluster_key: ClusterKey,
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
        let Some(mut run) = load_run(&state) else {
            return Ok(());
        };
        if !run.in_progress() {
            return Ok(());
        }

        if run.arch_builders.is_empty() {
            let builders = pick_builders(&state.nodes, &state.unreachable);
            if builders.is_empty() {
                run.fail("no reachable node reports an architecture", now());
                return self.save(run).await;
            }
            for (arch, node) in &builders {
                run.per_node.insert(node.clone(), NodePhase::AssignedBuild);
                info!(arch, node, "assigned self-update build");
            }
            run.arch_builders = builders;
            return self.save(run).await;
        }

        advance_awaiting_artifact(&mut run, &state.nodes);

        if run.has_failed() {
            run.fail("a node reported a build or fetch failure", now());
            return self.save(run).await;
        }

        let all_ready = run
            .per_node
            .values()
            .all(|p| matches!(p, NodePhase::Ready | NodePhase::Restarting | NodePhase::Done));
        if !all_ready {
            return Ok(());
        }

        if let Some(name) = restarting_node(&run) {
            self.check_restart_progress(&mut run, &state, &name);
            return self.save(run).await;
        }

        let Some(leader_name) = self.leader_name(&state) else {
            return Ok(());
        };
        let order = restart_order(&state.nodes, &leader_name);
        for name in order {
            if run.per_node.get(&name) == Some(&NodePhase::Ready) {
                self.restart_one(&mut run, &name).await;
                return self.save(run).await;
            }
        }

        if run.is_complete() {
            run.finish(now());
            self.save(run).await
        } else {
            Ok(())
        }
    }

    /// Once a node is restarting, it counts as done when it is reachable
    /// again and reports the hash its arch's build produced; it counts as
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
        let expected_hash = run
            .arch_builders
            .get(&node.arch)
            .and_then(|builder| run.per_node.get(builder))
            .and_then(|phase| match phase {
                NodePhase::Built { sha256 } => Some(sha256.clone()),
                _ => None,
            });

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
                NodePhase::RestartFailed { reason: "did not come back in time".into() },
            );
            run.fail(format!("{name} did not come back in time after restart"), now());
        }
    }

    async fn restart_one(&self, run: &mut UpdateRunState, name: &str) {
        let Some(base_url) = peers::base_url(&self.cluster_config, name) else {
            run.fail(format!("no address on record for {name}"), now());
            return;
        };
        match internal_client::trigger_restart(&self.http, &base_url, &self.cluster_key).await {
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
            .keys()
            .find(|name| cl_raft::node_id(name) == leader_id)
            .cloned()
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

fn restarting_node(run: &UpdateRunState) -> Option<String> {
    run.per_node
        .iter()
        .find(|(_, phase)| **phase == NodePhase::Restarting)
        .map(|(name, _)| name.clone())
}

/// Read the current run state out of applied cluster state, if any.
pub fn load_run(state: &cl_raft::ClusterState) -> Option<UpdateRunState> {
    let entry = state.configs.get(RUN_CONFIG)?;
    let raw = entry.decode().ok()?;
    serde_json::from_slice(&raw).ok()
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
            role,
            rpc_addr: "10.10.0.1:7373".parse::<SocketAddr>().unwrap(),
            drives: Default::default(),
            availability: cl_raft::state::Availability::Active,
            labels: Default::default(),
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: arch.to_string(),
            binary_hash: String::new(),
            node_pubkey: None,
        }
    }

    #[test]
    fn one_builder_is_picked_per_distinct_arch() {
        let nodes = BTreeMap::from([
            ("a".to_string(), node("a", Role::Manager, "x86_64")),
            ("b".to_string(), node("b", Role::Worker, "x86_64")),
            ("c".to_string(), node("c", Role::Worker, "aarch64")),
        ]);
        let builders = pick_builders(&nodes, &BTreeSet::new());
        assert_eq!(builders.len(), 2);
        assert_eq!(builders["x86_64"], "a");
        assert_eq!(builders["aarch64"], "c");
    }

    #[test]
    fn an_unreachable_node_is_never_picked_as_a_builder() {
        let nodes = BTreeMap::from([
            ("a".to_string(), node("a", Role::Manager, "x86_64")),
            ("b".to_string(), node("b", Role::Worker, "x86_64")),
        ]);
        let unreachable = BTreeSet::from(["a".to_string()]);
        let builders = pick_builders(&nodes, &unreachable);
        assert_eq!(builders["x86_64"], "b");
    }

    #[test]
    fn nodes_with_no_reported_arch_are_skipped() {
        let nodes = BTreeMap::from([("a".to_string(), node("a", Role::Manager, ""))]);
        assert!(pick_builders(&nodes, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn restart_order_puts_workers_first_then_managers_then_the_leader_last() {
        let nodes = BTreeMap::from([
            ("leader".to_string(), node("leader", Role::Manager, "x86_64")),
            ("mgr2".to_string(), node("mgr2", Role::Manager, "x86_64")),
            ("w1".to_string(), node("w1", Role::Worker, "x86_64")),
            ("w2".to_string(), node("w2", Role::Worker, "x86_64")),
        ]);
        let order = restart_order(&nodes, "leader");
        assert_eq!(order, vec!["w1", "w2", "mgr2", "leader"]);
    }

    #[test]
    fn awaiting_artifact_only_moves_idle_non_builder_nodes_of_the_built_arch() {
        let nodes = BTreeMap::from([
            ("builder".to_string(), node("builder", Role::Manager, "x86_64")),
            ("other".to_string(), node("other", Role::Worker, "x86_64")),
            ("different_arch".to_string(), node("different_arch", Role::Worker, "aarch64")),
        ]);
        let mut run = UpdateRunState::new(
            "run-1".into(),
            ["builder".to_string(), "other".to_string(), "different_arch".to_string()],
            0,
        );
        run.arch_builders.insert("x86_64".to_string(), "builder".to_string());
        run.per_node.insert(
            "builder".to_string(),
            NodePhase::Built { sha256: "abc".into() },
        );

        advance_awaiting_artifact(&mut run, &nodes);

        assert_eq!(run.per_node["other"], NodePhase::AwaitingArtifact);
        assert_eq!(run.per_node["different_arch"], NodePhase::Idle);
    }
}
