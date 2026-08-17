//! Placement: deciding which node each instance runs on.
//!
//! Pure over its inputs, so the whole decision surface is unit testable without
//! docker or a cluster. The output is a list of raft commands the caller
//! submits; nothing here has side effects.

use std::collections::{BTreeMap, BTreeSet};

use cl_raft::ids::InstanceId;
use cl_raft::state::{ClusterState, NodeState, Request, Role};

use crate::error::{Result, SchedError};
use crate::plan::Desired;

/// What the scheduler wants changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Place { instance: InstanceId, node: String },
    Remove { instance: InstanceId },
}

impl Decision {
    /// The raft command carrying out this decision.
    pub fn into_request(self, drives: BTreeSet<String>) -> Request {
        match self {
            Decision::Place { instance, node } => Request::Assign {
                instance,
                node,
                drives,
            },
            Decision::Remove { instance } => Request::Unassign { instance },
        }
    }
}

/// A parsed `deploy.placement` constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    pub key: String,
    pub equal: bool,
    pub value: String,
}

impl Constraint {
    /// Swarm syntax: `node.role == manager`, `node.hostname != n3`.
    pub fn parse(raw: &str) -> Result<Self> {
        let (op, equal) = if raw.contains("!=") {
            ("!=", false)
        } else if raw.contains("==") {
            ("==", true)
        } else {
            return Err(SchedError::BadConstraint(raw.to_string()));
        };

        let (key, value) = raw
            .split_once(op)
            .ok_or_else(|| SchedError::BadConstraint(raw.to_string()))?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            return Err(SchedError::BadConstraint(raw.to_string()));
        }

        Ok(Constraint {
            key: key.to_string(),
            equal,
            value: value.to_string(),
        })
    }

    fn matches(&self, node: &NodeState) -> bool {
        let actual = match self.key.as_str() {
            "node.hostname" | "node.id" => Some(node.name.clone()),
            "node.role" => Some(
                match node.role {
                    Role::Manager => "manager",
                    Role::Worker => "worker",
                }
                .to_string(),
            ),
            // An unknown key never matches, so an equality constraint on it
            // excludes every node rather than silently passing.
            _ => None,
        };

        match actual {
            Some(actual) => (actual == self.value) == self.equal,
            None => false,
        }
    }
}

/// Why a node was rejected, kept so the error can say what went wrong.
fn rejection(
    state: &ClusterState,
    node: &NodeState,
    desired: &Desired,
    constraints: &[Constraint],
) -> Option<String> {
    if !node.schedulable() {
        return Some(format!("{} is {:?}", node.name, node.availability));
    }
    if state.unreachable.contains(&node.name) {
        return Some(format!("{} is unreachable", node.name));
    }
    if let Some(pin) = &desired.pinned_node {
        if &node.name != pin {
            return Some(format!("{} is not the pinned node {pin}", node.name));
        }
    }
    for drive in &desired.drives {
        if !node.implements(drive) {
            return Some(format!("{} does not implement drive {drive}", node.name));
        }
    }
    for constraint in constraints {
        if !constraint.matches(node) {
            return Some(format!(
                "{} fails constraint {} {} {}",
                node.name,
                constraint.key,
                if constraint.equal { "==" } else { "!=" },
                constraint.value
            ));
        }
    }
    None
}

/// Nodes that could run this instance, with the reasons the others could not.
pub fn candidates<'a>(
    state: &'a ClusterState,
    desired: &Desired,
) -> Result<(Vec<&'a NodeState>, Vec<String>)> {
    let constraints: Vec<Constraint> = desired
        .constraints
        .iter()
        .map(|raw| Constraint::parse(raw))
        .collect::<Result<_>>()?;

    let mut eligible = Vec::new();
    let mut rejected = Vec::new();
    for node in state.nodes.values() {
        match rejection(state, node, desired, &constraints) {
            None => eligible.push(node),
            Some(reason) => rejected.push(reason),
        }
    }
    Ok((eligible, rejected))
}

/// Decide where everything goes.
///
/// Instances already placed on a still-valid node stay put: reshuffling a
/// running cluster because a better node appeared would mean needless downtime
/// and volume transfers. Moves are the operator's call, through the ui.
pub fn place(state: &ClusterState, desired: &[Desired]) -> Result<Vec<Decision>> {
    let mut decisions = Vec::new();
    let mut wanted: BTreeSet<InstanceId> = BTreeSet::new();
    // Track what we hand out in this pass so spreading sees its own decisions.
    let mut load: BTreeMap<String, usize> = state
        .nodes
        .keys()
        .map(|name| (name.clone(), state.assignments_on(name).count()))
        .collect();

    for item in desired {
        let (eligible, rejected) = candidates(state, item)?;

        if item.global {
            // One copy per eligible node, numbered by position so the ids are
            // stable as long as the node set is.
            for (index, node) in eligible.iter().enumerate() {
                let id = InstanceId::new(&item.id.stack, &item.id.service, index as u32);
                wanted.insert(id.clone());
                if state.assignments.get(&id).map(|a| a.node.as_str()) != Some(&node.name) {
                    decisions.push(Decision::Place {
                        instance: id,
                        node: node.name.clone(),
                    });
                }
            }
            continue;
        }

        wanted.insert(item.id.clone());

        // Leave a valid existing placement alone.
        if let Some(current) = state.assignments.get(&item.id) {
            if eligible.iter().any(|n| n.name == current.node) {
                continue;
            }
        }

        let target = eligible
            .iter()
            .min_by_key(|node| (load.get(&node.name).copied().unwrap_or(0), node.name.clone()))
            .ok_or_else(|| SchedError::Unplaceable {
                instance: item.id.to_string(),
                reason: if rejected.is_empty() {
                    "no nodes in the cluster".to_string()
                } else {
                    rejected.join("; ")
                },
            })?;

        *load.entry(target.name.clone()).or_insert(0) += 1;
        decisions.push(Decision::Place {
            instance: item.id.clone(),
            node: target.name.clone(),
        });
    }

    // Anything assigned but no longer wanted is removed.
    for instance in state.assignments.keys() {
        if !wanted.contains(instance) {
            decisions.push(Decision::Remove {
                instance: instance.clone(),
            });
        }
    }

    Ok(decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{Availability, Request};
    use std::path::PathBuf;

    fn node(name: &str, role: Role, drives: &[&str]) -> NodeState {
        NodeState {
            name: name.to_string(),
            role,
            rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
            drives: drives
                .iter()
                .map(|d| (d.to_string(), PathBuf::from(format!("/srv/{d}"))))
                .collect(),
            availability: Availability::Active,
        }
    }

    fn cluster() -> ClusterState {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1", Role::Manager, &["fast", "bulk"])));
        state.apply(&Request::UpsertNode(node("n2", Role::Manager, &["fast"])));
        state.apply(&Request::UpsertNode(node("n3", Role::Worker, &[])));
        state
    }

    fn desired(service: &str, index: u32) -> Desired {
        Desired {
            id: InstanceId::new("site", service, index),
            image: "nginx".into(),
            drives: BTreeSet::new(),
            constraints: Vec::new(),
            pinned_node: None,
            global: false,
        }
    }

    fn placed(decisions: &[Decision], service: &str, index: u32) -> Option<String> {
        decisions.iter().find_map(|d| match d {
            Decision::Place { instance, node }
                if instance.service == service && instance.index == index =>
            {
                Some(node.clone())
            }
            _ => None,
        })
    }

    #[test]
    fn constraints_parse_both_operators() {
        let c = Constraint::parse("node.role == manager").expect("parse");
        assert_eq!(c.key, "node.role");
        assert!(c.equal);
        assert_eq!(c.value, "manager");

        let c = Constraint::parse("node.hostname != n3").expect("parse");
        assert!(!c.equal);

        assert!(Constraint::parse("node.role manager").is_err());
        assert!(Constraint::parse("== manager").is_err());
    }

    #[test]
    fn drives_narrow_the_candidates() {
        let state = cluster();
        let mut item = desired("db", 0);
        item.drives = ["bulk".to_string()].into();

        let (eligible, rejected) = candidates(&state, &item).expect("candidates");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].name, "n1");
        assert!(rejected.iter().any(|r| r.contains("n2 does not implement drive bulk")));
    }

    #[test]
    fn role_constraints_are_honoured() {
        let state = cluster();
        let mut item = desired("web", 0);
        item.constraints = vec!["node.role == worker".into()];

        let (eligible, _) = candidates(&state, &item).expect("candidates");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].name, "n3");
    }

    #[test]
    fn an_unknown_constraint_key_matches_nothing() {
        let state = cluster();
        let mut item = desired("web", 0);
        item.constraints = vec!["node.labels.zone == eu".into()];

        let (eligible, _) = candidates(&state, &item).expect("candidates");
        assert!(eligible.is_empty());
    }

    #[test]
    fn a_pin_overrides_the_spread() {
        let state = cluster();
        let mut item = desired("web", 0);
        item.pinned_node = Some("n3".into());

        let decisions = place(&state, &[item]).expect("place");
        assert_eq!(placed(&decisions, "web", 0).as_deref(), Some("n3"));
    }

    #[test]
    fn replicas_spread_across_nodes() {
        let state = cluster();
        let items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        let decisions = place(&state, &items).expect("place");

        let nodes: BTreeSet<_> = decisions
            .iter()
            .filter_map(|d| match d {
                Decision::Place { node, .. } => Some(node.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(nodes.len(), 3, "three replicas should land on three nodes");
    }

    #[test]
    fn existing_placements_are_left_alone() {
        let mut state = cluster();
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "web", 0),
            node: "n3".into(),
            drives: BTreeSet::new(),
        });

        // n3 is the emptiest node anyway, but the point is that no decision is
        // emitted at all for an instance that is already validly placed.
        let decisions = place(&state, &[desired("web", 0)]).expect("place");
        assert!(decisions.is_empty());
    }

    #[test]
    fn a_drained_node_gives_its_instances_up() {
        let mut state = cluster();
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "web", 0),
            node: "n3".into(),
            drives: BTreeSet::new(),
        });
        state.apply(&Request::SetAvailability {
            node: "n3".into(),
            availability: Availability::Drain,
        });

        let decisions = place(&state, &[desired("web", 0)]).expect("place");
        let target = placed(&decisions, "web", 0).expect("replacement node");
        assert_ne!(target, "n3");
    }

    #[test]
    fn an_unreachable_node_gives_its_instances_up() {
        let mut state = cluster();
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "web", 0),
            node: "n3".into(),
            drives: BTreeSet::new(),
        });
        state.apply(&Request::SetReachable {
            node: "n3".into(),
            reachable: false,
        });

        let decisions = place(&state, &[desired("web", 0)]).expect("place");
        let target = placed(&decisions, "web", 0).expect("replacement node");
        assert_ne!(target, "n3");
    }

    #[test]
    fn a_recovered_node_becomes_a_candidate_again() {
        let mut state = cluster();
        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: false,
        });
        let (eligible, rejected) = candidates(&state, &desired("web", 0)).expect("candidates");
        assert!(!eligible.iter().any(|n| n.name == "n1"));
        assert!(rejected.iter().any(|r| r.contains("n1 is unreachable")));

        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: true,
        });
        let (eligible, _) = candidates(&state, &desired("web", 0)).expect("candidates");
        assert!(eligible.iter().any(|n| n.name == "n1"));
    }

    #[test]
    fn instances_no_longer_wanted_are_removed() {
        let mut state = cluster();
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "old", 0),
            node: "n1".into(),
            drives: BTreeSet::new(),
        });

        let decisions = place(&state, &[]).expect("place");
        assert_eq!(
            decisions,
            vec![Decision::Remove {
                instance: InstanceId::new("site", "old", 0)
            }]
        );
    }

    #[test]
    fn global_services_land_on_every_eligible_node() {
        let state = cluster();
        let mut item = desired("agent", 0);
        item.global = true;

        let decisions = place(&state, &[item]).expect("place");
        assert_eq!(decisions.len(), 3);
    }

    #[test]
    fn a_global_service_skips_nodes_missing_the_drive() {
        let state = cluster();
        let mut item = desired("agent", 0);
        item.global = true;
        item.drives = ["fast".to_string()].into();

        let decisions = place(&state, &[item]).expect("place");
        assert_eq!(decisions.len(), 2, "only n1 and n2 implement fast");
    }

    #[test]
    fn an_unplaceable_instance_reports_why() {
        let state = cluster();
        let mut item = desired("db", 0);
        item.drives = ["missing".to_string()].into();

        let err = place(&state, &[item]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("site.db.0"), "{msg}");
        assert!(msg.contains("does not implement drive missing"), "{msg}");
    }
}
