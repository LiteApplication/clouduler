//! Rebalancing: actively relocating already-placed instances to smooth load
//! across nodes, run only on operator request (`clouduler rebalance` / the ui
//! button), never on the regular scheduling sweep.
//!
//! `place::place` deliberately never moves a validly-placed instance -
//! reshuffling a running cluster on every pass would mean needless downtime
//! and volume transfers for no operator-visible reason (see its own doc
//! comment). Rebalancing is the opposite: explicitly asked for, so it is
//! allowed to disturb a stable cluster to spread load more evenly. It never
//! decides placement itself; it only picks, among nodes an instance could
//! already legally run on (the same `candidates` `place` uses), a less busy
//! one to move it to.

use std::collections::{BTreeMap, BTreeSet};

use cl_raft::ids::InstanceId;
use cl_raft::state::ClusterState;

use crate::place::candidates;
use crate::plan::Desired;

/// One instance rebalance decided to relocate. Carrying out the move is the
/// caller's job (`Request::BeginMigration`, the same request the ui's manual
/// "move" button already submits), so a redeploy or a config edit racing this
/// plan is rejected exactly where a manual move would be, not silently
/// overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub instance: InstanceId,
    pub from: String,
    pub to: String,
}

/// One instance still eligible to move, and where it could go.
struct Movable<'a> {
    item: &'a Desired,
    current: String,
    candidate_nodes: BTreeSet<String>,
}

/// Decide which already-placed instances to move to smooth the number of
/// instances per node.
///
/// Only ever considers an instance that:
/// - is not pinned to a specific node (`x-clouduler.node`) or a global
///   service (one copy per eligible node already, nothing to balance);
/// - publishes no proxy hostname - a hostname shared by more than one
///   instance has to move as one group (see `place`'s `host_node`
///   anchoring), which this does not attempt, so such an instance is simply
///   left alone rather than half-moved;
/// - is not already mid-migration;
/// - has more than one node in its own candidate set, the current one
///   included (otherwise there is nowhere else for it to go, or its current
///   placement is not even valid any more, which is `place`'s job to fix).
///
/// Greedy and bounded: each pass moves one instance off the currently busiest
/// node (that still has a movable instance on it) onto the least busy node
/// that can run it, stopping once no candidate move would narrow the gap by
/// more than one, so two nodes within one of each other are left alone rather
/// than trading a container back and forth forever. Capped at one pass over
/// every instance, the most a single call could ever usefully need.
pub fn plan(
    state: &ClusterState,
    desired: &[Desired],
    served_drives: &BTreeSet<String>,
    everywhere_networks: &BTreeSet<String>,
) -> Vec<Move> {
    let mut load: BTreeMap<String, usize> = state
        .nodes
        .keys()
        .map(|name| (name.clone(), state.assignments_on(name).count()))
        .collect();

    let mut movable: Vec<Movable> = Vec::new();
    for item in desired {
        if item.global || item.pinned_node.is_some() || !item.proxy_hosts.is_empty() {
            continue;
        }
        let Some(assignment) = state.assignments.get(&item.id) else {
            continue;
        };
        if assignment.migration.is_some() {
            continue;
        }
        let Ok((eligible, _)) = candidates(state, item, served_drives, everywhere_networks) else {
            continue;
        };
        let candidate_nodes: BTreeSet<String> = eligible.iter().map(|n| n.name.clone()).collect();
        if candidate_nodes.len() < 2 || !candidate_nodes.contains(&assignment.node) {
            continue;
        }
        movable.push(Movable {
            item,
            current: assignment.node.clone(),
            candidate_nodes,
        });
    }

    let mut moves = Vec::new();
    for _ in 0..desired.len() {
        // Busiest node that still has a movable instance on it. Ties break on
        // node name so a run is deterministic.
        let Some(from) = movable
            .iter()
            .map(|m| m.current.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .max_by_key(|node| (load.get(node).copied().unwrap_or(0), std::cmp::Reverse(node.clone())))
        else {
            break;
        };
        let from_load = load.get(&from).copied().unwrap_or(0);

        // Among instances on `from`, the one whose best alternative node is
        // least busy - that is the move that narrows the gap the most.
        let candidate = movable
            .iter()
            .enumerate()
            .filter(|(_, m)| m.current == from)
            .filter_map(|(idx, m)| {
                m.candidate_nodes
                    .iter()
                    .filter(|n| **n != from)
                    .min_by_key(|n| load.get(*n).copied().unwrap_or(0))
                    .map(|to| (idx, to.clone()))
            })
            .min_by_key(|(_, to)| load.get(to).copied().unwrap_or(0));

        let Some((idx, to)) = candidate else { break };
        let to_load = load.get(&to).copied().unwrap_or(0);

        // Moving must narrow the gap by more than the one container it
        // relocates, or two nodes already within one of each other would
        // trade a container back and forth every time this runs.
        if from_load.saturating_sub(to_load) < 2 {
            break;
        }

        let m = movable.remove(idx);
        *load.get_mut(&from).expect("from has a load entry, it hosts m") -= 1;
        *load.entry(to.clone()).or_insert(0) += 1;
        moves.push(Move {
            instance: m.item.id.clone(),
            from: m.current,
            to,
        });
    }

    moves
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{Availability, NodeState, Request, Role};

    fn node(name: &str) -> NodeState {
        NodeState {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            role: Role::Manager,
            rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
            drives: Default::default(),
            availability: Availability::Active,
            labels: Default::default(),
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
        }
    }

    fn desired(service: &str, index: u32) -> Desired {
        Desired {
            id: InstanceId::new("site", service, index),
            image: "img".into(),
            drives: Default::default(),
            constraints: Vec::new(),
            pinned_node: None,
            global: false,
            egress_network: None,
            networks: Default::default(),
            bound_ports: Default::default(),
            proxy_hosts: Default::default(),
        }
    }

    fn assign(state: &mut ClusterState, service: &str, index: u32, node: &str) {
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", service, index),
            node: node.to_string(),
            drives: Default::default(),
        });
    }

    fn no_served() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn no_everywhere() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn an_overloaded_node_gives_up_an_instance_to_an_empty_one() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        for i in 0..3 {
            assign(&mut state, "web", i, "n1");
        }

        let items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        let moves = plan(&state, &items, &no_served(), &no_everywhere());

        assert_eq!(moves.len(), 1, "3 vs 0 narrows to 2 vs 1, then stops");
        assert_eq!(moves[0].from, "n1");
        assert_eq!(moves[0].to, "n2");
    }

    #[test]
    fn nodes_already_within_one_of_each_other_are_left_alone() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        assign(&mut state, "web", 0, "n1");
        assign(&mut state, "web", 1, "n2");

        let items: Vec<_> = (0..2).map(|i| desired("web", i)).collect();
        assert!(plan(&state, &items, &no_served(), &no_everywhere()).is_empty());
    }

    #[test]
    fn a_pinned_instance_never_moves() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        for i in 0..3 {
            assign(&mut state, "web", i, "n1");
        }

        let mut items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        for item in &mut items {
            item.pinned_node = Some("n1".into());
        }

        assert!(plan(&state, &items, &no_served(), &no_everywhere()).is_empty());
    }

    #[test]
    fn an_instance_sharing_a_proxy_host_never_moves() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        for i in 0..3 {
            assign(&mut state, "web", i, "n1");
        }

        let mut items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        for item in &mut items {
            item.proxy_hosts = ["app.example.com".to_string()].into();
        }

        assert!(plan(&state, &items, &no_served(), &no_everywhere()).is_empty());
    }

    #[test]
    fn a_global_instance_never_moves() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        assign(&mut state, "agent", 0, "n1");
        assign(&mut state, "agent", 1, "n2");
        for i in 0..3 {
            assign(&mut state, "web", i, "n1");
        }

        let mut agent = desired("agent", 0);
        agent.global = true;
        let mut items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        items.push(agent);

        let moves = plan(&state, &items, &no_served(), &no_everywhere());
        assert!(moves.iter().all(|m| m.instance.service != "agent"));
    }

    #[test]
    fn a_mid_migration_instance_never_moves_again() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        for i in 0..3 {
            assign(&mut state, "web", i, "n1");
        }
        state.apply(&Request::BeginMigration {
            instance: InstanceId::new("site", "web", 0),
            to: "n2".to_string(),
            drives: Default::default(),
        });

        let items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        let moves = plan(&state, &items, &no_served(), &no_everywhere());
        assert!(moves.iter().all(|m| m.instance != InstanceId::new("site", "web", 0)));
    }

    #[test]
    fn repeated_runs_converge_rather_than_oscillate() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        for i in 0..5 {
            assign(&mut state, "web", i, "n1");
        }

        let items: Vec<_> = (0..5).map(|i| desired("web", i)).collect();
        let first = plan(&state, &items, &no_served(), &no_everywhere());
        assert!(!first.is_empty());
        for m in &first {
            state.apply(&Request::Assign {
                instance: m.instance.clone(),
                node: m.to.clone(),
                drives: Default::default(),
            });
        }

        // Applying the plan must leave the cluster settled: running it again
        // finds nothing left worth moving.
        assert!(plan(&state, &items, &no_served(), &no_everywhere()).is_empty());
    }
}
