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
        let Some(mut run) = load_run(&state) else {
            return Ok(());
        };
        if !run.in_progress() {
            return Ok(());
        }

        if run.release_tag.is_none() {
            return self.resolve_release(run).await;
        }

        if run.has_failed() {
            run.fail("a node reported a fetch or restart failure", now());
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

    /// Resolve the configured release (once) into a tag and a per-arch asset
    /// map, then hand every node its assignment. Every node moves straight to
    /// `Fetching`: there is no build step left to gate on.
    async fn resolve_release(&self, mut run: UpdateRunState) -> Result<(), String> {
        let Some(self_update) = self.cluster_config.self_update.as_ref() else {
            run.fail("no self-update release is configured", now());
            return self.save(run).await;
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
        self.save(run).await
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
        let Some(key) = self.cluster_key.borrow().clone() else {
            run.fail("this node has no cluster key to sign the restart request with", now());
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
            id: uuid::Uuid::new_v4(),
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
}
