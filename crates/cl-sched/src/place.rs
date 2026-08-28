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
    pub fn instance(&self) -> &InstanceId {
        match self {
            Decision::Place { instance, .. } | Decision::Remove { instance } => instance,
        }
    }

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
        // A label the node does not carry is a real answer, not a missing one:
        // `node.labels.fast == true` must exclude a node without the label,
        // and `node.labels.fast != true` must include it. Treating an absent
        // label as an empty string gets both, where returning None would make
        // the negative form exclude every node in the cluster.
        if let Some(key) = self.key.strip_prefix("node.labels.") {
            let actual = node.labels.get(key).map(String::as_str).unwrap_or("");
            return (actual == self.value) == self.equal;
        }

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
    served_drives: &BTreeSet<String>,
    everywhere_networks: &BTreeSet<String>,
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
        // A served drive has one authoritative copy that any node can reach
        // over the internal network (mounted per volume, on demand, once the
        // instance is actually placed); only a replicated drive, which has
        // no such fallback, requires the node to already hold a local copy.
        if !served_drives.contains(drive) && !node.implements(drive) {
            return Some(format!("{} does not implement drive {drive}", node.name));
        }
    }
    // Implementing a replicated drive says the node has the directory, not
    // that it has this volume's data. Every node holds its own copy and only
    // one of them is authoritative, so being allowed to run here is a question
    // about the copy, not about the drive.
    //
    // The bar is holding a copy at the volume's current generation. Anything
    // less is a node that has never received this volume (its mount is an
    // empty directory, and an instance started on it comes up as if its data
    // had never existed) or one whose copy is behind (it missed the syncs
    // since the last handover). Either way `run::volume_requests` would then
    // make *that* copy the primary, and the sync scheduler would push it over
    // the good one everywhere else. The primary itself always clears this bar,
    // so there is always a candidate as long as it is up.
    for volume in &desired.volumes {
        if served_drives.contains(&volume.drive) {
            continue;
        }
        // No record yet means the volume is about to be created wherever this
        // lands (`run::volume_requests`), so there is nothing to disagree with.
        let Some(record) = state.volumes.get(volume) else {
            continue;
        };
        if record.replicas.get(&node.name) != Some(&record.generation) {
            return Some(format!("{} holds no current copy of volume {volume}", node.name));
        }
    }
    if let Some(net) = &desired.egress_network {
        // A nat network declares no per-node interface at all: every node
        // implements it the same way, so it never narrows candidates the
        // way an egress or wireguard interface does.
        if !everywhere_networks.contains(net) && !node.implements_egress_network(net) {
            return Some(format!("{} does not implement egress network {net}", node.name));
        }
    }
    for net in &desired.networks {
        if !everywhere_networks.contains(net) && !node.implements_network(net) {
            return Some(format!("{} does not implement network {net}", node.name));
        }
    }
    for (net, proto, port) in &desired.bound_ports {
        if !node.opens_port(net, *proto, *port) {
            return Some(format!(
                "{} does not have {port}/{proto:?} open on network {net}",
                node.name
            ));
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

/// An instance the cluster wants but that could not be placed on any node,
/// with the reason so an operator (or the ui) can say why rather than just
/// "unschedulable".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplaceable {
    pub instance: InstanceId,
    pub reason: String,
}

/// What a scheduling pass decided: placements/removals to submit, plus
/// whatever could not be placed. Unlike an error, this never aborts the rest
/// of the batch — one instance with no eligible node must not stop every
/// other instance in the cluster from scheduling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaceOutcome {
    pub decisions: Vec<Decision>,
    pub unplaceable: Vec<Unplaceable>,
}

/// Nodes that could run this instance, with the reasons the others could not.
///
/// Note what this deliberately does *not* do: prefer the node currently
/// holding a replicated volume's primary. Co-locating the instance with the
/// authoritative copy is required (otherwise the sync overwrites the running
/// container's data), but there are two ways to get there and dragging the
/// instance to the primary is the expensive one - it means stopping a healthy
/// service and restarting it on a copy that is at best equally fresh. Moving
/// the *primary* to the instance costs one raft entry and no downtime, so that
/// is what `run::volume_requests` does. Placement's only job here is to refuse
/// nodes whose copy is not current, which is what keeps that promotion safe.
pub fn candidates<'a>(
    state: &'a ClusterState,
    desired: &Desired,
    served_drives: &BTreeSet<String>,
    everywhere_networks: &BTreeSet<String>,
) -> Result<(Vec<&'a NodeState>, Vec<String>)> {
    let constraints: Vec<Constraint> = desired
        .constraints
        .iter()
        .map(|raw| Constraint::parse(raw))
        .collect::<Result<_>>()?;

    let mut eligible = Vec::new();
    let mut rejected = Vec::new();
    for node in state.nodes.values() {
        match rejection(state, node, desired, &constraints, served_drives, everywhere_networks) {
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
///
/// An instance with no eligible node is reported in `unplaceable` rather than
/// aborting the pass: one bad service must not stop every other instance in
/// the cluster from scheduling.
pub fn place(
    state: &ClusterState,
    desired: &[Desired],
    served_drives: &BTreeSet<String>,
    everywhere_networks: &BTreeSet<String>,
) -> PlaceOutcome {
    let mut decisions = Vec::new();
    let mut unplaceable = Vec::new();
    let mut wanted: BTreeSet<InstanceId> = BTreeSet::new();
    // Track what we hand out in this pass so spreading sees its own decisions.
    let mut load: BTreeMap<String, usize> = state
        .nodes
        .keys()
        .map(|name| (name.clone(), state.assignments_on(name).count()))
        .collect();

    // Anchor every proxied hostname to whichever node already (validly)
    // hosts it, so an instance sharing that hostname joins it instead of
    // spreading to a different node. Seeded from existing assignments before
    // any new placement decisions are made, so a running cluster stays put.
    // "Validly" is checked with the same `candidates` an instance is judged
    // by everywhere else - not just node liveness - so a node that is up but
    // no longer implements what the instance needs (a network removed from
    // it, say) can't self-anchor a sole hostname owner in place forever, nor
    // anchor its co-located siblings to a node none of them can actually use.
    let mut host_node: BTreeMap<&str, &str> = BTreeMap::new();
    for item in desired {
        if let Some(current) = state.assignments.get(&item.id) {
            let validly_placed = candidates(state, item, served_drives, everywhere_networks)
                .is_ok_and(|(eligible, _)| eligible.iter().any(|n| n.name == current.node));
            if validly_placed {
                for host in &item.proxy_hosts {
                    host_node.entry(host).or_insert(&current.node);
                }
            }
        }
    }

    for item in desired {
        let (eligible, rejected) = match candidates(state, item, served_drives, everywhere_networks) {
            Ok(v) => v,
            Err(e) => {
                wanted.insert(item.id.clone());
                unplaceable.push(Unplaceable {
                    instance: item.id.clone(),
                    reason: e.to_string(),
                });
                continue;
            }
        };

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

        // An instance sharing a proxied hostname with an already-anchored
        // instance can only go where that hostname already lands - a
        // hostname can't point at two nodes at once. Hostnames anchored to
        // different nodes (one instance's routes span more than one, or two
        // of its hostnames disagree) leave nothing eligible: a real conflict
        // the operator has to resolve, not a node to guess between.
        let anchors: BTreeSet<&str> = item.proxy_hosts.iter().filter_map(|h| host_node.get(h.as_str())).copied().collect();
        let eligible: Vec<&NodeState> = if anchors.is_empty() {
            eligible
        } else if anchors.len() == 1 {
            let anchor = *anchors.iter().next().unwrap();
            eligible.into_iter().filter(|n| n.name == anchor).collect()
        } else {
            Vec::new()
        };

        // Leave a valid existing placement alone.
        if let Some(current) = state.assignments.get(&item.id) {
            if eligible.iter().any(|n| n.name == current.node) {
                for host in &item.proxy_hosts {
                    host_node.entry(host).or_insert(&current.node);
                }
                continue;
            }
        }

        let target = eligible
            .iter()
            .min_by_key(|node| (load.get(&node.name).copied().unwrap_or(0), node.name.clone()));

        let Some(target) = target else {
            unplaceable.push(Unplaceable {
                instance: item.id.clone(),
                reason: if !anchors.is_empty() {
                    format!(
                        "must share a node with hostname(s) {} already placed on {}",
                        item.proxy_hosts.iter().cloned().collect::<Vec<_>>().join(", "),
                        anchors.into_iter().collect::<Vec<_>>().join(", ")
                    )
                } else if rejected.is_empty() {
                    "no nodes in the cluster".to_string()
                } else {
                    rejected.join("; ")
                },
            });
            continue;
        };

        *load.entry(target.name.clone()).or_insert(0) += 1;
        for host in &item.proxy_hosts {
            host_node.entry(host).or_insert(&target.name);
        }
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

    PlaceOutcome { decisions, unplaceable }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::ids::VolumeId;
    use cl_raft::state::{Availability, Request};
    use std::path::PathBuf;

    fn node(name: &str, role: Role, drives: &[&str]) -> NodeState {
        NodeState {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            role,
            rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
            drives: drives
                .iter()
                .map(|d| (d.to_string(), PathBuf::from(format!("/srv/{d}"))))
                .collect(),
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

    fn labelled(name: &str, labels: &[(&str, &str)]) -> NodeState {
        let mut node = node(name, Role::Worker, &[]);
        node.labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        node
    }

    fn constraint(raw: &str) -> Constraint {
        Constraint::parse(raw).expect("parses")
    }

    fn no_served() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn no_everywhere() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn a_label_constraint_matches_the_nodes_carrying_it() {
        let fast = labelled("n1", &[("fast", "true")]);
        let slow = labelled("n2", &[]);
        let c = constraint("node.labels.fast == true");
        assert!(c.matches(&fast));
        assert!(!c.matches(&slow));
    }

    /// The trap: a node without the label has to *pass* the negative form.
    /// Treating an absent label as "no answer" would make `!=` exclude every
    /// node in the cluster, which is silently the opposite of what was asked.
    #[test]
    fn a_negated_label_constraint_includes_nodes_without_the_label() {
        let tagged = labelled("n1", &[("mail", "true")]);
        let untagged = labelled("n2", &[]);
        let c = constraint("node.labels.mail != true");
        assert!(!c.matches(&tagged));
        assert!(c.matches(&untagged));
    }

    #[test]
    fn a_label_with_a_different_value_does_not_match() {
        let node = labelled("n1", &[("tier", "cold")]);
        assert!(!constraint("node.labels.tier == hot").matches(&node));
        assert!(constraint("node.labels.tier == cold").matches(&node));
    }

    /// Dots are legal inside a label key, so only the first three segments are
    /// the prefix.
    #[test]
    fn a_dotted_label_key_is_matched_whole() {
        let node = labelled("n1", &[("storage.kind", "nvme")]);
        assert!(constraint("node.labels.storage.kind == nvme").matches(&node));
    }

    /// A key that is not a label and not one of the built-ins still matches
    /// nothing, rather than being read as an empty label.
    #[test]
    fn an_unknown_non_label_key_still_matches_nothing() {
        let node = labelled("n1", &[]);
        assert!(!constraint("node.platform.os == linux").matches(&node));
        assert!(!constraint("node.platform.os != linux").matches(&node));
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
            volumes: BTreeSet::new(),
            constraints: Vec::new(),
            pinned_node: None,
            global: false,
            egress_network: None,
            networks: BTreeSet::new(),
            bound_ports: BTreeSet::new(),
            proxy_hosts: BTreeSet::new(),
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

        let (eligible, rejected) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].name, "n1");
        assert!(rejected.iter().any(|r| r.contains("n2 does not implement drive bulk")));
    }

    /// `fast/data`, primary n1, with n2 holding a synced replica.
    fn with_replicated_volume(replicated_to: Option<&str>) -> (ClusterState, Desired) {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        if let Some(node) = replicated_to {
            state.apply(&Request::SetVolumeReplica {
                id: id.clone(),
                node: node.into(),
                generation: 1,
            });
        }

        let mut item = desired("db", 0);
        item.drives = ["fast".to_string()].into();
        item.volumes = [id].into();
        (state, item)
    }

    /// A node whose copy is current can run the instance: the primary follows
    /// it afterwards (`run::volume_requests`), which is far cheaper than
    /// stopping a healthy service to move it onto the primary instead.
    #[test]
    fn a_node_holding_a_current_copy_is_a_candidate() {
        let (state, item) = with_replicated_volume(Some("n2"));

        let (eligible, _) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert_eq!(eligible.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["n1", "n2"]);
    }

    /// And a healthy placement on such a node is left alone rather than
    /// dragged back onto the primary — moving the primary is the cheap half
    /// of restoring the invariant, moving the container is not.
    #[test]
    fn an_instance_running_off_its_primary_is_not_moved_back() {
        let (mut state, item) = with_replicated_volume(Some("n2"));
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "db", 0),
            node: "n2".into(),
            drives: ["fast".to_string()].into(),
        });

        let outcome = place(&state, &[item], &no_served(), &no_everywhere());
        assert_eq!(placed(&outcome.decisions, "db", 0), None, "should stay on n2");
    }

    /// Running on a node that never received a copy is not failover: the
    /// volume comes up empty, and that empty copy then becomes the primary and
    /// is synced over everyone else's.
    #[test]
    fn a_node_holding_no_copy_is_never_a_candidate() {
        let (mut state, item) = with_replicated_volume(None);
        state.unreachable.insert("n1".into());

        let (eligible, rejected) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert!(eligible.is_empty(), "eligible: {:?}", eligible.iter().map(|n| &n.name).collect::<Vec<_>>());
        assert!(
            rejected.iter().any(|r| r.contains("n2 holds no current copy of volume fast/data")),
            "rejected: {rejected:?}"
        );
    }

    /// A copy that has fallen behind is no better than no copy: it is missing
    /// every write since the last handover, and promoting it would sync those
    /// away everywhere else.
    #[test]
    fn a_node_whose_copy_is_behind_is_never_a_candidate() {
        let (mut state, item) = with_replicated_volume(Some("n2"));
        // A handover bumps the generation; n2 has not caught up with it yet.
        state.apply(&Request::TouchVolume {
            id: VolumeId::new("fast", "data"),
            primary: "n1".into(),
        });

        let (eligible, rejected) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert_eq!(eligible.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["n1"]);
        assert!(
            rejected.iter().any(|r| r.contains("n2 holds no current copy of volume fast/data")),
            "rejected: {rejected:?}"
        );
    }

    /// A served drive's volumes have one copy the whole cluster reaches over
    /// the network, so its primary says where the data is served from, not
    /// where the instance has to run.
    #[test]
    fn a_served_volumes_primary_does_not_narrow_placement() {
        let (state, item) = with_replicated_volume(None);

        let served = ["fast".to_string()].into();
        let (eligible, _) = candidates(&state, &item, &served, &no_everywhere()).expect("candidates");
        assert!(eligible.iter().any(|n| n.name == "n2"), "n2 should still be eligible");
    }

    /// A served drive has one authoritative copy any node can reach over the
    /// internal network, so a node that never declared it locally is still
    /// eligible — unlike a replicated drive, which has no such fallback.
    #[test]
    fn a_served_drive_does_not_require_the_node_to_implement_it() {
        let state = cluster();
        let mut item = desired("db", 0);
        item.drives = ["bulk".to_string()].into();
        let served = ["bulk".to_string()].into();

        let (eligible, _) = candidates(&state, &item, &served, &no_everywhere()).expect("candidates");
        let names: BTreeSet<_> = eligible.iter().map(|n| n.name.clone()).collect();
        assert_eq!(names, ["n1".to_string(), "n2".to_string(), "n3".to_string()].into());
    }

    #[test]
    fn networks_narrow_the_candidates() {
        let mut state = cluster();
        state.nodes.get_mut("n1").unwrap().networks = ["wan".to_string()].into();
        let mut item = desired("db", 0);
        item.networks = ["wan".to_string()].into();

        let (eligible, rejected) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].name, "n1");
        assert!(rejected.iter().any(|r| r.contains("n2 does not implement network wan")));
    }

    /// A nat network takes no per-node config at all, so it never narrows
    /// candidates the way an ordinary network (or an egress/wireguard
    /// interface) does: every node implements it identically.
    #[test]
    fn a_nat_network_does_not_narrow_the_candidates() {
        let state = cluster();
        let mut item = desired("db", 0);
        item.networks = ["home".to_string()].into();
        item.egress_network = Some("home".to_string());
        let everywhere = ["home".to_string()].into();

        let (eligible, _) = candidates(&state, &item, &no_served(), &everywhere).expect("candidates");
        let names: BTreeSet<_> = eligible.iter().map(|n| n.name.clone()).collect();
        assert_eq!(names, ["n1".to_string(), "n2".to_string(), "n3".to_string()].into());
    }

    #[test]
    fn bound_ports_narrow_the_candidates() {
        let mut state = cluster();
        state.nodes.get_mut("n1").unwrap().open_ports =
            [("wan".to_string(), [(cl_raft::state::PortProtocol::Tcp, 25)].into())].into();
        let mut item = desired("mail", 0);
        item.bound_ports = [("wan".to_string(), cl_raft::state::PortProtocol::Tcp, 25)].into();

        let (eligible, rejected) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].name, "n1");
        assert!(rejected.iter().any(|r| r.contains("n2 does not have 25/Tcp open on network wan")));
    }

    #[test]
    fn role_constraints_are_honoured() {
        let state = cluster();
        let mut item = desired("web", 0);
        item.constraints = vec!["node.role == worker".into()];

        let (eligible, _) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].name, "n3");
    }

    #[test]
    fn an_unknown_constraint_key_matches_nothing() {
        let state = cluster();
        let mut item = desired("web", 0);
        item.constraints = vec!["node.labels.zone == eu".into()];

        let (eligible, _) = candidates(&state, &item, &no_served(), &no_everywhere()).expect("candidates");
        assert!(eligible.is_empty());
    }

    /// A served drive must not pin every replica to the node that happens to
    /// serve it - any node can reach it over the internal network, so
    /// replicas spread across the cluster exactly like a driveless service.
    #[test]
    fn served_drive_replicas_spread_across_nodes_not_onto_the_server() {
        let state = cluster();
        let served = ["fast".to_string()].into();
        let mut items: Vec<_> = (0..3).map(|i| desired("db", i)).collect();
        for item in &mut items {
            item.drives = ["fast".to_string()].into();
        }

        let decisions = place(&state, &items, &served, &no_everywhere()).decisions;
        let nodes: BTreeSet<_> = decisions
            .iter()
            .filter_map(|d| match d {
                Decision::Place { node, .. } => Some(node.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            nodes.len(),
            3,
            "three replicas of a served-drive service should still land on three different nodes, not just n1 (the drive's server)"
        );
    }

    #[test]
    fn a_pin_overrides_the_spread() {
        let state = cluster();
        let mut item = desired("web", 0);
        item.pinned_node = Some("n3".into());

        let decisions = place(&state, &[item], &no_served(), &no_everywhere()).decisions;
        assert_eq!(placed(&decisions, "web", 0).as_deref(), Some("n3"));
    }

    #[test]
    fn replicas_spread_across_nodes() {
        let state = cluster();
        let items: Vec<_> = (0..3).map(|i| desired("web", i)).collect();
        let decisions = place(&state, &items, &no_served(), &no_everywhere()).decisions;

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
        let decisions = place(&state, &[desired("web", 0)], &no_served(), &no_everywhere()).decisions;
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

        let decisions = place(&state, &[desired("web", 0)], &no_served(), &no_everywhere()).decisions;
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

        let decisions = place(&state, &[desired("web", 0)], &no_served(), &no_everywhere()).decisions;
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
        let (eligible, rejected) = candidates(&state, &desired("web", 0), &no_served(), &no_everywhere()).expect("candidates");
        assert!(!eligible.iter().any(|n| n.name == "n1"));
        assert!(rejected.iter().any(|r| r.contains("n1 is unreachable")));

        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: true,
        });
        let (eligible, _) = candidates(&state, &desired("web", 0), &no_served(), &no_everywhere()).expect("candidates");
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

        let decisions = place(&state, &[], &no_served(), &no_everywhere()).decisions;
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

        let decisions = place(&state, &[item], &no_served(), &no_everywhere()).decisions;
        assert_eq!(decisions.len(), 3);
    }

    #[test]
    fn a_global_service_skips_nodes_missing_the_drive() {
        let state = cluster();
        let mut item = desired("agent", 0);
        item.global = true;
        item.drives = ["fast".to_string()].into();

        let decisions = place(&state, &[item], &no_served(), &no_everywhere()).decisions;
        assert_eq!(decisions.len(), 2, "only n1 and n2 implement fast");
    }

    #[test]
    fn an_unplaceable_instance_reports_why() {
        let state = cluster();
        let mut item = desired("db", 0);
        item.drives = ["missing".to_string()].into();

        let outcome = place(&state, &[item], &no_served(), &no_everywhere());
        assert!(outcome.decisions.is_empty());
        let unplaceable = &outcome.unplaceable[0];
        assert_eq!(unplaceable.instance, InstanceId::new("site", "db", 0));
        assert!(unplaceable.reason.contains("does not implement drive missing"), "{}", unplaceable.reason);
    }

    /// One instance with nowhere to go must not stop everything else in the
    /// same pass from placing: `place` used to abort the whole batch on the
    /// first unplaceable instance.
    #[test]
    fn an_unplaceable_instance_does_not_block_the_rest_of_the_batch() {
        let state = cluster();
        let mut broken = desired("db", 0);
        broken.drives = ["missing".to_string()].into();
        let fine = desired("web", 0);

        let outcome = place(&state, &[broken, fine], &no_served(), &no_everywhere());
        assert_eq!(outcome.unplaceable.len(), 1);
        assert_eq!(outcome.unplaceable[0].instance, InstanceId::new("site", "db", 0));
        assert!(placed(&outcome.decisions, "web", 0).is_some());
    }

    /// Two services publishing the same proxied hostname must land on the
    /// same node - a hostname can only point at one place once DNS answers
    /// with the owning node's address alone.
    #[test]
    fn instances_sharing_a_proxy_host_are_colocated() {
        let state = cluster();
        let mut main = desired("web", 0);
        main.proxy_hosts = ["app.example.com".to_string()].into();
        let mut admin = desired("admin", 0);
        admin.proxy_hosts = ["app.example.com".to_string()].into();

        let decisions = place(&state, &[main, admin], &no_served(), &no_everywhere()).decisions;
        let main_node = placed(&decisions, "web", 0).expect("web placed");
        let admin_node = placed(&decisions, "admin", 0).expect("admin placed");
        assert_eq!(main_node, admin_node);
    }

    /// A new instance sharing a hostname with one already running joins that
    /// node rather than spreading elsewhere, so a live cluster does not get
    /// reshuffled just because a second service started publishing the same
    /// hostname.
    #[test]
    fn a_new_instance_joins_an_already_placed_hostname() {
        let mut state = cluster();
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "web", 0),
            node: "n3".into(),
            drives: BTreeSet::new(),
        });

        let mut main = desired("web", 0);
        main.proxy_hosts = ["app.example.com".to_string()].into();
        let mut admin = desired("admin", 0);
        admin.proxy_hosts = ["app.example.com".to_string()].into();

        let decisions = place(&state, &[main, admin], &no_served(), &no_everywhere()).decisions;
        assert_eq!(placed(&decisions, "admin", 0).as_deref(), Some("n3"));
    }

    /// If the node already hosting a proxied hostname goes down, the whole
    /// co-located group must relocate together rather than being anchored
    /// forever to a node that will never answer again.
    #[test]
    fn a_colocated_group_relocates_when_its_anchor_node_goes_down() {
        let mut state = cluster();
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "web", 0),
            node: "n3".into(),
            drives: BTreeSet::new(),
        });
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "admin", 0),
            node: "n3".into(),
            drives: BTreeSet::new(),
        });
        state.apply(&Request::SetReachable {
            node: "n3".into(),
            reachable: false,
        });

        let mut main = desired("web", 0);
        main.proxy_hosts = ["app.example.com".to_string()].into();
        let mut admin = desired("admin", 0);
        admin.proxy_hosts = ["app.example.com".to_string()].into();

        let outcome = place(&state, &[main, admin], &no_served(), &no_everywhere());
        assert!(
            outcome.unplaceable.is_empty(),
            "should relocate rather than get stuck: {:?}",
            outcome.unplaceable
        );
        let main_node = placed(&outcome.decisions, "web", 0).expect("web placed");
        let admin_node = placed(&outcome.decisions, "admin", 0).expect("admin placed");
        assert_eq!(main_node, admin_node);
        assert_ne!(main_node, "n3");
    }

    /// The sole publisher of a hostname must not self-anchor to its own
    /// stale placement when that node is up but no longer implements what
    /// the instance needs (a network dropped from it, say) - otherwise it is
    /// permanently stuck pointing at a node it can never actually use.
    #[test]
    fn a_node_that_lost_a_needed_network_does_not_self_anchor_the_lone_hostname_owner() {
        let mut state = cluster();
        state.nodes.get_mut("n1").unwrap().networks = ["public".to_string()].into();
        state.apply(&Request::Assign {
            instance: InstanceId::new("site", "web", 0),
            node: "n3".into(),
            drives: BTreeSet::new(),
        });

        let mut item = desired("web", 0);
        item.proxy_hosts = ["share.example.com".to_string()].into();
        item.networks = ["public".to_string()].into();

        let outcome = place(&state, &[item], &no_served(), &no_everywhere());
        assert!(
            outcome.unplaceable.is_empty(),
            "should relocate to a node with the network rather than get stuck: {:?}",
            outcome.unplaceable
        );
        assert_eq!(placed(&outcome.decisions, "web", 0).as_deref(), Some("n1"));
    }

    /// A constraint that rules out the node the hostname is already anchored
    /// to is a real conflict, reported rather than silently placed elsewhere
    /// (which would split the hostname across two nodes).
    #[test]
    fn a_conflicting_constraint_makes_the_colocated_instance_unplaceable() {
        let state = cluster();
        let mut main = desired("web", 0);
        main.proxy_hosts = ["app.example.com".to_string()].into();
        main.constraints = vec!["node.hostname == n1".into()];
        let mut admin = desired("admin", 0);
        admin.proxy_hosts = ["app.example.com".to_string()].into();
        admin.constraints = vec!["node.hostname == n2".into()];

        let outcome = place(&state, &[main, admin], &no_served(), &no_everywhere());
        assert!(placed(&outcome.decisions, "web", 0).is_some());
        assert_eq!(outcome.unplaceable.len(), 1);
        assert_eq!(outcome.unplaceable[0].instance, InstanceId::new("site", "admin", 0));
    }
}
