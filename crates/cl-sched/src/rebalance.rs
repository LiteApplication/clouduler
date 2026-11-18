//! Rebalancing: relocating already-placed instances to smooth load, only on operator
//! request (`clouduler rebalance` / ui button), never on the regular sweep.
//!
//! `place::place` never moves a validly-placed instance (to avoid needless downtime and
//! volume transfers); rebalance is the explicit exception. It never decides placement:
//! it only picks a less busy node among the legal `candidates` `place` uses.

use std::collections::{BTreeMap, BTreeSet};

use cl_raft::ids::InstanceId;
use cl_raft::state::ClusterState;

use crate::place::candidates;
use crate::plan::Desired;

/// One instance rebalance decided to relocate. The caller carries out the move via
/// `Request::BeginMigration` (as the ui's manual move does), so racing edits are rejected.
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

/// A node's burden, compared lexicographically: memory pressure first (permille of the
/// node's reported total used by instances declaring a memory limit), then instance count
/// as fallback and tie-break (0 pressure for nodes without a reported total). With no
/// declared limits anywhere this collapses to count-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct NodeLoad {
    mem_permille: u64,
    count: usize,
}

fn mem_permille(used: u64, total: u64) -> u64 {
    used.saturating_mul(1000).checked_div(total).unwrap_or(0)
}

fn load_of(
    state: &ClusterState,
    node: &str,
    count: &BTreeMap<String, usize>,
    mem_used: &BTreeMap<String, u64>,
) -> NodeLoad {
    let total = state
        .nodes
        .get(node)
        .map(|n| n.memory_total_bytes)
        .unwrap_or(0);
    NodeLoad {
        mem_permille: mem_permille(mem_used.get(node).copied().unwrap_or(0), total),
        count: count.get(node).copied().unwrap_or(0),
    }
}

/// Whether moving one instance `from` -> `to` is worth it: at least a 1 point easing in
/// memory pressure when pressures differ, else more than one instance apart by count (no
/// ping-pong). The move must also not leave `to` more loaded than `from` was.
fn worth_moving(from: NodeLoad, to: NodeLoad, to_after: NodeLoad) -> bool {
    let meaningful_gap = if from.mem_permille != to.mem_permille {
        from.mem_permille.saturating_sub(to.mem_permille) >= 10
    } else {
        from.count.saturating_sub(to.count) >= 2
    };
    meaningful_gap && to_after <= from
}

/// Decide which placed instances to move. Considers only instances that are not pinned
/// or global, publish no proxy hostname (those must move as a group, not attempted), are
/// not mid-migration, and have more than one candidate node (otherwise `place` handles it).
///
/// Greedy: each pass moves one instance off the busiest node onto the least busy node that
/// can run it, stopping when no move narrows the gap (`worth_moving`), capped at one pass
/// per instance.
pub fn plan(
    state: &ClusterState,
    desired: &[Desired],
    served_drives: &BTreeSet<String>,
    everywhere_networks: &BTreeSet<String>,
) -> Vec<Move> {
    let memory_bytes: BTreeMap<&InstanceId, u64> = desired
        .iter()
        .filter_map(|d| Some((&d.id, d.memory_bytes?)))
        .collect();

    let mut count: BTreeMap<String, usize> = state
        .nodes
        .keys()
        .map(|name| (name.clone(), state.assignments_on(name).count()))
        .collect();
    let mut mem_used: BTreeMap<String, u64> = BTreeMap::new();
    for (id, assignment) in &state.assignments {
        if let Some(&bytes) = memory_bytes.get(id) {
            let used = mem_used.entry(assignment.node.clone()).or_insert(0);
            *used = used.saturating_add(bytes);
        }
    }

    let mut movable: Vec<Movable> = Vec::new();
    for item in desired {
        if item.global || item.pinned_node.is_some() || item.anchor_keys().next().is_some() {
            continue;
        }
        let Some(assignment) = state.assignments.get(&item.id) else {
            continue;
        };
        if assignment
            .migration
            .as_ref()
            .is_some_and(|m| !m.phase.is_terminal())
        {
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
            .max_by_key(|node| {
                (
                    load_of(state, node, &count, &mem_used),
                    std::cmp::Reverse(node.clone()),
                )
            })
        else {
            break;
        };
        let from_load = load_of(state, &from, &count, &mem_used);

        // Among instances on `from`, the one whose best alternative node is
        // least busy - that is the move that narrows the gap the most.
        let Some((idx, to)) = movable
            .iter()
            .enumerate()
            .filter(|(_, m)| m.current == from)
            .filter_map(|(idx, m)| {
                m.candidate_nodes
                    .iter()
                    .filter(|n| **n != from)
                    .min_by_key(|n| load_of(state, n, &count, &mem_used))
                    .map(|to| (idx, to.clone()))
            })
            .min_by_key(|(_, to)| load_of(state, to, &count, &mem_used))
        else {
            break;
        };
        let to_load = load_of(state, &to, &count, &mem_used);
        let moved_bytes = memory_bytes
            .get(&movable[idx].item.id)
            .copied()
            .unwrap_or(0);
        let to_total = state
            .nodes
            .get(&to)
            .map(|n| n.memory_total_bytes)
            .unwrap_or(0);
        let to_after = NodeLoad {
            mem_permille: mem_permille(
                mem_used
                    .get(&to)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(moved_bytes),
                to_total,
            ),
            count: to_load.count + 1,
        };

        if !worth_moving(from_load, to_load, to_after) {
            break;
        }

        let m = movable.remove(idx);
        if let Some(c) = count.get_mut(&from) {
            *c = c.saturating_sub(1);
        }
        *count.entry(to.clone()).or_insert(0) += 1;
        if moved_bytes > 0 {
            let from_used = mem_used.entry(from.clone()).or_insert(0);
            *from_used = from_used.saturating_sub(moved_bytes);
            let to_used = mem_used.entry(to.clone()).or_insert(0);
            *to_used = to_used.saturating_add(moved_bytes);
        }
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
        sized_node(name, 0)
    }

    fn sized_node(name: &str, memory_total_bytes: u64) -> NodeState {
        NodeState {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            role: Role::Manager,
            rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
            drives: Default::default(),
            availability: Availability::Active,
            labels: Default::default(),
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
            memory_total_bytes,
            gpus: Default::default(),
            images: Default::default(),
            version: Default::default(),
        }
    }

    fn desired(service: &str, index: u32) -> Desired {
        sized_desired(service, index, None)
    }

    fn sized_desired(service: &str, index: u32, memory_bytes: Option<u64>) -> Desired {
        Desired {
            id: InstanceId::new("site", service, index),
            image: "img".into(),
            drives: Default::default(),
            volumes: Default::default(),
            constraints: Vec::new(),
            pinned_node: None,
            global: false,
            egress_network: None,
            networks: Default::default(),
            bound_ports: Default::default(),
            proxy_hosts: Default::default(),
            colocate_key: None,
            memory_bytes,
            gpu_needs: Default::default(),
            near: BTreeSet::new(),
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
    fn an_instance_in_a_colocate_group_never_moves() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        for i in 0..3 {
            assign(&mut state, "web", i, "n1");
        }

        let mut items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        for item in &mut items {
            item.colocate_key = Some("colocate:app,web".to_string());
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
        assert!(moves
            .iter()
            .all(|m| m.instance != InstanceId::new("site", "web", 0)));
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

    /// Two nodes with equal instance counts, but one of them a quarter the
    /// memory of the other and already far more full. Equal counts alone
    /// would leave this untouched.
    #[test]
    fn a_node_with_less_memory_gives_up_an_instance_even_at_equal_count() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(sized_node("big", 4_000_000_000)));
        state.apply(&Request::UpsertNode(sized_node("small", 1_000_000_000)));
        assign(&mut state, "web", 0, "big");
        assign(&mut state, "web", 1, "small");

        let items = vec![
            sized_desired("web", 0, Some(1_000_000_000)),
            sized_desired("web", 1, Some(900_000_000)),
        ];

        let moves = plan(&state, &items, &no_served(), &no_everywhere());
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].from, "small");
        assert_eq!(moves[0].to, "big");
    }

    /// A move that would just relocate the overload onto the destination
    /// (because the moving instance is large relative to a small node's
    /// total) is rejected rather than taken.
    #[test]
    fn a_move_that_would_overload_the_destination_is_skipped() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(sized_node("big", 4_000_000_000)));
        state.apply(&Request::UpsertNode(sized_node("tiny", 500_000_000)));
        for i in 0..2 {
            assign(&mut state, "web", i, "big");
        }

        let items = vec![
            sized_desired("web", 0, Some(1_000_000_000)),
            sized_desired("web", 1, Some(1_000_000_000)),
        ];

        assert!(plan(&state, &items, &no_served(), &no_everywhere()).is_empty());
    }

    /// With no node reporting a memory total yet, memory pressure is 0
    /// everywhere and the plan falls back to the original count-only
    /// behavior even when instances do declare a memory limit.
    #[test]
    fn unknown_node_capacity_falls_back_to_count_based_balancing() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1")));
        state.apply(&Request::UpsertNode(node("n2")));
        for i in 0..3 {
            assign(&mut state, "web", i, "n1");
        }

        let items: Vec<_> = (0..3)
            .map(|i| sized_desired("web", i, Some(1_000_000)))
            .collect();
        let moves = plan(&state, &items, &no_served(), &no_everywhere());

        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].from, "n1");
        assert_eq!(moves[0].to, "n2");
    }
}
