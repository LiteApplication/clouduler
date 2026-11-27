//! Rebalancing: relocating already-placed instances to smooth load, only on operator
//! request (`clouduler rebalance` / ui button), never on the regular sweep.
//!
//! `place::place` never moves a validly-placed instance (to avoid needless downtime and
//! volume transfers); rebalance is the explicit exception. It never decides placement:
//! it only picks a less busy node among the legal `candidates` `place` uses. "Busy" is judged
//! from live usage readings when the caller has them and from declared memory limits otherwise.

use std::collections::{BTreeMap, BTreeSet};

use cl_raft::ids::InstanceId;
use cl_raft::state::ClusterState;
use serde::{Deserialize, Serialize};

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

/// A reading of how much a node or an instance is really using right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Memory in use. For an instance this is what docker reports for its container.
    pub memory_used_bytes: u64,
    /// Share of the whole host's CPU, 0 to 100 (an instance using one core of a 4 core host is 25).
    pub cpu_percent: f64,
}

/// Live readings gathered by the caller just before planning. Raft holds none of this, so
/// `plan` only sees what the caller managed to collect: a node or instance missing here simply
/// has no reading and falls back to declared limits. `LiveUsage::default()` is empty and
/// makes `plan` use declared memory limits and instance counts only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveUsage {
    pub nodes: BTreeMap<String, Usage>,
    pub instances: BTreeMap<InstanceId, Usage>,
}

/// A node's burden, compared lexicographically: memory pressure first (permille of the
/// node's reported total in use), then CPU load (permille of the host), then instance count
/// as fallback and tie-break. Memory in use is the node's live reading when there is one,
/// else the sum of the declared limits of its instances; 0 pressure for nodes without a
/// reported total. CPU is 0 for nodes without a live reading. With no live readings and no
/// declared limits anywhere this collapses to count-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct NodeLoad {
    mem_permille: u64,
    cpu_permille: u64,
    count: usize,
}

/// Running per-node totals, updated as moves are simulated.
struct Tally {
    count: BTreeMap<String, usize>,
    mem_used: BTreeMap<String, u64>,
    cpu_permille: BTreeMap<String, u64>,
}

fn mem_permille(used: u64, total: u64) -> u64 {
    used.saturating_mul(1000).checked_div(total).unwrap_or(0)
}

fn cpu_permille(percent: f64) -> u64 {
    if percent.is_finite() && percent > 0.0 {
        (percent * 10.0).round() as u64
    } else {
        0
    }
}

fn total_of(state: &ClusterState, node: &str) -> u64 {
    state
        .nodes
        .get(node)
        .map(|n| n.memory_total_bytes)
        .unwrap_or(0)
}

fn load_of(state: &ClusterState, node: &str, tally: &Tally) -> NodeLoad {
    NodeLoad {
        mem_permille: mem_permille(
            tally.mem_used.get(node).copied().unwrap_or(0),
            total_of(state, node),
        ),
        cpu_permille: tally.cpu_permille.get(node).copied().unwrap_or(0),
        count: tally.count.get(node).copied().unwrap_or(0),
    }
}

/// CPU gap (permille of the host) below which two nodes count as equally busy on CPU.
const CPU_GAP_PERMILLE: u64 = 50;

/// Memory pressure (permille) a move may add to `to` beyond `from`'s when CPU is what
/// justifies the move.
const MEM_SLACK_PERMILLE: u64 = 10;

/// Whether moving one instance `from` -> `to` is worth it. When memory pressures differ,
/// only memory counts: at least a 1 point easing, and the move must not leave `to` more
/// loaded than `from` was. When they are equal, a CPU gap of at least 5 points justifies a
/// move as long as it leaves `to` with less CPU than `from` had and memory pressure no
/// more than 1 point above `from`'s. Otherwise it is count: more than one instance apart,
/// without leaving `to` more loaded than `from` was (no ping-pong).
fn worth_moving(from: NodeLoad, to: NodeLoad, to_after: NodeLoad) -> bool {
    if from.mem_permille != to.mem_permille {
        return from.mem_permille.saturating_sub(to.mem_permille) >= 10 && to_after <= from;
    }
    if from.cpu_permille.abs_diff(to.cpu_permille) >= CPU_GAP_PERMILLE {
        return from.cpu_permille > to.cpu_permille
            && to_after.cpu_permille <= from.cpu_permille
            && to_after.mem_permille <= from.mem_permille.saturating_add(MEM_SLACK_PERMILLE);
    }
    from.count.saturating_sub(to.count) >= 2 && to_after <= from
}

/// Decide which placed instances to move. Considers only instances that are not pinned
/// or global, publish no proxy hostname (those must move as a group, not attempted), are
/// not mid-migration, and have more than one candidate node (otherwise `place` handles it).
///
/// Load comes from `live` where it has a reading and from declared memory limits otherwise
/// (see `NodeLoad`). A simulated move shifts the instance's live memory and CPU, falling back
/// to its declared memory limit and then to 0.
///
/// Greedy: each pass moves one instance off the busiest node onto the least busy node that
/// can run it, stopping when no move narrows the gap (`worth_moving`), capped at one pass
/// per instance.
pub fn plan(
    state: &ClusterState,
    desired: &[Desired],
    served_drives: &BTreeSet<String>,
    everywhere_networks: &BTreeSet<String>,
    live: &LiveUsage,
) -> Vec<Move> {
    let memory_bytes: BTreeMap<&InstanceId, u64> = desired
        .iter()
        .filter_map(|d| Some((&d.id, d.memory_bytes?)))
        .collect();

    let mut tally = Tally {
        count: state
            .nodes
            .keys()
            .map(|name| (name.clone(), state.assignments_on(name).count()))
            .collect(),
        mem_used: BTreeMap::new(),
        cpu_permille: BTreeMap::new(),
    };
    for (node, usage) in &live.nodes {
        tally.mem_used.insert(node.clone(), usage.memory_used_bytes);
        tally
            .cpu_permille
            .insert(node.clone(), cpu_permille(usage.cpu_percent));
    }
    for (id, assignment) in &state.assignments {
        if live.nodes.contains_key(&assignment.node) {
            continue;
        }
        if let Some(&bytes) = memory_bytes.get(id) {
            let used = tally.mem_used.entry(assignment.node.clone()).or_insert(0);
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
                    load_of(state, node, &tally),
                    std::cmp::Reverse(node.clone()),
                )
            })
        else {
            break;
        };
        let from_load = load_of(state, &from, &tally);

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
                    .min_by_key(|n| load_of(state, n, &tally))
                    .map(|to| (idx, to.clone()))
            })
            .min_by_key(|(_, to)| load_of(state, to, &tally))
        else {
            break;
        };
        let to_load = load_of(state, &to, &tally);
        let id = &movable[idx].item.id;
        let live_instance = live.instances.get(id);
        let moved_bytes = live_instance
            .map(|u| u.memory_used_bytes)
            .or_else(|| memory_bytes.get(id).copied())
            .unwrap_or(0);
        let moved_cpu = live_instance.map_or(0, |u| cpu_permille(u.cpu_percent));
        let to_after = NodeLoad {
            mem_permille: mem_permille(
                tally
                    .mem_used
                    .get(&to)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(moved_bytes),
                total_of(state, &to),
            ),
            cpu_permille: to_load.cpu_permille.saturating_add(moved_cpu),
            count: to_load.count + 1,
        };

        if !worth_moving(from_load, to_load, to_after) {
            break;
        }

        let m = movable.remove(idx);
        if let Some(c) = tally.count.get_mut(&from) {
            *c = c.saturating_sub(1);
        }
        *tally.count.entry(to.clone()).or_insert(0) += 1;
        if moved_bytes > 0 {
            let from_used = tally.mem_used.entry(from.clone()).or_insert(0);
            *from_used = from_used.saturating_sub(moved_bytes);
            let to_used = tally.mem_used.entry(to.clone()).or_insert(0);
            *to_used = to_used.saturating_add(moved_bytes);
        }
        if moved_cpu > 0 {
            let from_cpu = tally.cpu_permille.entry(from.clone()).or_insert(0);
            *from_cpu = from_cpu.saturating_sub(moved_cpu);
            let to_cpu = tally.cpu_permille.entry(to.clone()).or_insert(0);
            *to_cpu = to_cpu.saturating_add(moved_cpu);
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
            open_all_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            ui_hash: Default::default(),
            started_at: Default::default(),
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
        let moves = plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default(),
        );

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
        assert!(plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default()
        )
        .is_empty());
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

        assert!(plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default()
        )
        .is_empty());
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
            item.colocate_key = Some("colocate:app.example.com".to_string());
        }

        assert!(plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default()
        )
        .is_empty());
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

        assert!(plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default()
        )
        .is_empty());
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

        let moves = plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default(),
        );
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
        let moves = plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default(),
        );
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
        let first = plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default(),
        );
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
        assert!(plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default()
        )
        .is_empty());
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

        let moves = plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default(),
        );
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

        assert!(plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default()
        )
        .is_empty());
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
        let moves = plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default(),
        );

        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].from, "n1");
        assert_eq!(moves[0].to, "n2");
    }

    fn live(nodes: &[(&str, u64, f64)], instances: &[(u32, u64, f64)]) -> LiveUsage {
        LiveUsage {
            nodes: nodes
                .iter()
                .map(|(n, m, c)| {
                    (
                        n.to_string(),
                        Usage {
                            memory_used_bytes: *m,
                            cpu_percent: *c,
                        },
                    )
                })
                .collect(),
            instances: instances
                .iter()
                .map(|(i, m, c)| {
                    (
                        InstanceId::new("site", "web", *i),
                        Usage {
                            memory_used_bytes: *m,
                            cpu_percent: *c,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Declared limits are tiny and equal, but one node really uses most of its memory.
    #[test]
    fn low_declared_but_high_live_memory_counts_as_busy() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(sized_node("hot", 4_000_000_000)));
        state.apply(&Request::UpsertNode(sized_node("cool", 4_000_000_000)));
        assign(&mut state, "web", 0, "hot");
        assign(&mut state, "web", 1, "hot");
        assign(&mut state, "web", 2, "cool");
        assign(&mut state, "web", 3, "cool");
        let items: Vec<_> = (0..4)
            .map(|i| sized_desired("web", i, Some(10_000_000)))
            .collect();

        assert!(plan(
            &state,
            &items,
            &no_served(),
            &no_everywhere(),
            &LiveUsage::default()
        )
        .is_empty());

        let usage = live(
            &[("hot", 3_600_000_000, 0.0), ("cool", 400_000_000, 0.0)],
            &[(0, 1_000_000_000, 0.0), (1, 1_000_000_000, 0.0)],
        );
        let moves = plan(&state, &items, &no_served(), &no_everywhere(), &usage);
        assert!(!moves.is_empty());
        assert_eq!(moves[0].from, "hot");
        assert_eq!(moves[0].to, "cool");
    }

    /// The instance's live size, not its (huge) limit, decides whether the move overloads
    /// the destination.
    #[test]
    fn live_instance_size_decides_the_shifted_load() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(sized_node("big", 4_000_000_000)));
        state.apply(&Request::UpsertNode(sized_node("small", 1_000_000_000)));
        assign(&mut state, "web", 0, "big");
        assign(&mut state, "web", 1, "big");
        let items = vec![
            sized_desired("web", 0, Some(3_000_000_000)),
            sized_desired("web", 1, Some(3_000_000_000)),
        ];

        // Declared 3 GB each would overload "small" (and 6/4 GB overcommits "big").
        let usage = live(
            &[("big", 2_400_000_000, 0.0), ("small", 0, 0.0)],
            &[(0, 100_000_000, 0.0), (1, 1_200_000_000, 0.0)],
        );
        let moves = plan(&state, &items, &no_served(), &no_everywhere(), &usage);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].instance, InstanceId::new("site", "web", 0));
        assert_eq!(moves[0].to, "small");

        // Without the live instance readings the 3 GB limit is used and the move is refused.
        let nodes_only = live(&[("big", 2_400_000_000, 0.0), ("small", 0, 0.0)], &[]);
        assert!(plan(&state, &items, &no_served(), &no_everywhere(), &nodes_only).is_empty());
    }

    /// Equal memory pressure: the busier CPU gives up an instance, even at equal count.
    #[test]
    fn cpu_breaks_a_memory_tie_before_count() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(sized_node("busy", 4_000_000_000)));
        state.apply(&Request::UpsertNode(sized_node("idle", 4_000_000_000)));
        assign(&mut state, "web", 0, "busy");
        assign(&mut state, "web", 1, "idle");
        let items = vec![desired("web", 0), desired("web", 1)];

        let usage = live(
            &[("busy", 1_000_000_000, 80.0), ("idle", 1_000_000_000, 10.0)],
            &[(0, 8_000_000, 30.0), (1, 8_000_000, 5.0)],
        );
        let moves = plan(&state, &items, &no_served(), &no_everywhere(), &usage);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].from, "busy");
        assert_eq!(moves[0].to, "idle");

        // A CPU gap under 5 points does not justify moving at equal count.
        let close = live(
            &[("busy", 1_000_000_000, 12.0), ("idle", 1_000_000_000, 10.0)],
            &[],
        );
        assert!(plan(&state, &items, &no_served(), &no_everywhere(), &close).is_empty());
    }

    /// A move whose CPU would make the destination busier than the source was is refused.
    #[test]
    fn a_move_that_would_flip_the_cpu_ranking_is_skipped() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(sized_node("a", 4_000_000_000)));
        state.apply(&Request::UpsertNode(sized_node("b", 4_000_000_000)));
        assign(&mut state, "web", 0, "a");
        let items = vec![desired("web", 0)];

        let usage = live(
            &[("a", 1_000_000_000, 60.0), ("b", 1_000_000_000, 0.0)],
            &[(0, 500_000_000, 60.0)],
        );
        assert!(plan(&state, &items, &no_served(), &no_everywhere(), &usage).is_empty());
    }

    /// A node without a live reading is measured by declared limits next to one that has one.
    #[test]
    fn missing_live_readings_fall_back_to_declared_limits() {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(sized_node("n1", 4_000_000_000)));
        state.apply(&Request::UpsertNode(sized_node("n2", 4_000_000_000)));
        for i in 0..2 {
            assign(&mut state, "web", i, "n1");
        }
        let items: Vec<_> = (0..2)
            .map(|i| sized_desired("web", i, Some(1_000_000_000)))
            .collect();

        // n2 has a reading, n1 does not: n1 is 2 GB declared (500 permille), n2 is 0.
        let usage = live(&[("n2", 0, 0.0)], &[]);
        let moves = plan(&state, &items, &no_served(), &no_everywhere(), &usage);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].from, "n1");
        assert_eq!(moves[0].to, "n2");
    }
}
