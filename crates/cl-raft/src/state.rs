//! The replicated cluster state and the commands that mutate it.
//!
//! Everything here is pure: `apply` is the only way state changes, and it never
//! performs IO. That keeps the state machine deterministic across nodes and
//! makes the invariants unit testable.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{InstanceId, VolumeId};

/// Whether a node may take new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    /// Normal, schedulable.
    Active,
    /// Keeps running what it has, takes nothing new. Set before a migration.
    Drain,
    /// Removed from the cluster, holds nothing.
    Decommissioned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Manager,
    Worker,
}

/// A node as the cluster currently sees it. Seeded from the config file, then
/// edited at runtime through the web ui.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeState {
    pub name: String,
    pub role: Role,
    pub rpc_addr: SocketAddr,
    /// Drives this node implements, mapped to its local backing directory.
    pub drives: BTreeMap<String, PathBuf>,
    pub availability: Availability,
}

impl NodeState {
    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
    }

    /// A drained or decommissioned node must not receive new containers.
    pub fn schedulable(&self) -> bool {
        self.availability == Availability::Active
    }
}

/// A deployed stack. The spec is kept as the raw yaml the user submitted so the
/// ui can round trip it without lossy reserialisation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackState {
    pub name: String,
    pub spec: String,
    /// Bumped on every redeploy, so nodes can tell a spec change from a restart.
    pub revision: u64,
    /// Contents of the files the spec's `configs` and `secrets` point at,
    /// captured at deploy time. They have to travel with the stack: the paths
    /// in the yaml only exist on whichever machine ran the deploy, and a
    /// container that moves would otherwise lose them.
    #[serde(default)]
    pub files: BTreeMap<String, Attachment>,
}

/// One file carried along with a stack.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// Base64, so the state machine stays valid json for snapshots.
    pub content: String,
    /// Secrets are redacted when the state is rendered, and never logged.
    pub secret: bool,
}

impl Attachment {
    pub fn decode(&self) -> Result<Vec<u8>, String> {
        base64_decode(&self.content)
    }

    pub fn len(&self) -> usize {
        // Base64 is 4 characters per 3 bytes, padding included.
        self.content.len() / 4 * 3
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// Never print a secret by accident.
impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field(
                "content",
                &if self.secret {
                    "<redacted>".to_string()
                } else {
                    format!("{} bytes", self.len())
                },
            )
            .field("secret", &self.secret)
            .finish()
    }
}

/// Cap on the files one stack may carry. Everything here goes through the raft
/// log and into every snapshot, so a stack is not a place to put a disk image.
pub const MAX_STACK_FILES_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredState {
    Running,
    Stopped,
}

/// Where an instance runs and what it should be doing there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub instance: InstanceId,
    pub node: String,
    pub desired: DesiredState,
    /// Pinned image digest. Set once pulled so every replica runs the same bits
    /// even when the tag is `:latest`.
    pub image_digest: Option<String>,
    /// Present while the instance is moving to another node.
    pub migration: Option<Migration>,
}

/// A move between nodes, recorded step by step so a crash mid-move is recoverable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    pub to: String,
    pub phase: MigrationPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MigrationPhase {
    /// Container is being stopped on the source node.
    Stopping,
    /// Final volume delta is being sent to the target.
    Syncing,
    /// Target is checking what it received.
    Verifying,
    /// Assignment is being switched over.
    Switching,
    /// Container is starting on the target.
    Starting,
    Done,
    Failed { reason: String },
}

impl MigrationPhase {
    /// The phase this one advances to, or None when the move is over.
    pub fn next(&self) -> Option<MigrationPhase> {
        use MigrationPhase::*;
        match self {
            Stopping => Some(Syncing),
            Syncing => Some(Verifying),
            Verifying => Some(Switching),
            Switching => Some(Starting),
            Starting => Some(Done),
            Done | Failed { .. } => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, MigrationPhase::Done | MigrationPhase::Failed { .. })
    }
}

/// Replication bookkeeping for one volume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeState {
    pub id: VolumeId,
    /// Bumped by whichever node last wrote the volume. Lets a node tell whether
    /// its copy is behind without hashing the whole tree.
    pub generation: u64,
    /// Node holding the authoritative copy.
    pub primary: String,
    /// Generation each node has caught up to.
    pub replicas: BTreeMap<String, u64>,
}

impl VolumeState {
    /// Nodes whose copy is current.
    pub fn in_sync(&self) -> BTreeSet<&str> {
        self.replicas
            .iter()
            .filter(|(_, gen)| **gen == self.generation)
            .map(|(node, _)| node.as_str())
            .collect()
    }
}

/// A certificate clouduler holds, distributed to every node so any of them can
/// terminate TLS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CertificateState {
    pub fqdn: String,
    pub chain_pem: String,
    pub key_pem: String,
    /// Unix seconds.
    pub not_after: i64,
}

/// A DNS record clouduler created, tracked so ownership survives leader change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsRecordState {
    pub fqdn: String,
    pub record_type: String,
    pub content: String,
    pub zone: String,
    /// Cloudflare's id, so the record can be updated or deleted later.
    pub provider_id: Option<String>,
}

/// Everything the cluster agrees on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterState {
    pub nodes: BTreeMap<String, NodeState>,
    pub stacks: BTreeMap<String, StackState>,
    pub assignments: BTreeMap<InstanceId, Assignment>,
    pub volumes: BTreeMap<VolumeId, VolumeState>,
    pub certificates: BTreeMap<String, CertificateState>,
    pub dns_records: BTreeMap<String, DnsRecordState>,
    /// Nodes the leader has stopped hearing from. Observed rather than
    /// configured, so it is kept apart from `availability`, which is what the
    /// operator asked for.
    #[serde(default)]
    pub unreachable: BTreeSet<String>,
    /// Incremented on every applied command. Cheap way for watchers to notice
    /// a change without diffing.
    pub version: u64,
}

/// A command submitted through raft. Every mutation of cluster state is one of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    UpsertNode(NodeState),
    SetAvailability {
        node: String,
        availability: Availability,
    },
    /// Submitted by the leader once a node has been silent for longer than
    /// `failover.node_timeout`, or once it answers again.
    SetReachable {
        node: String,
        reachable: bool,
    },
    RemoveNode {
        node: String,
    },
    DeployStack {
        name: String,
        spec: String,
        /// Contents of the files the spec references, bundled by the client.
        #[serde(default)]
        files: BTreeMap<String, Attachment>,
    },
    RemoveStack {
        name: String,
    },
    /// Place an instance. `drives` lists what the instance needs, so the state
    /// machine can refuse a node that does not implement all of them.
    Assign {
        instance: InstanceId,
        node: String,
        drives: BTreeSet<String>,
    },
    Unassign {
        instance: InstanceId,
    },
    SetDesiredState {
        instance: InstanceId,
        desired: DesiredState,
    },
    SetImageDigest {
        instance: InstanceId,
        digest: String,
    },
    BeginMigration {
        instance: InstanceId,
        to: String,
        drives: BTreeSet<String>,
    },
    AdvanceMigration {
        instance: InstanceId,
        phase: MigrationPhase,
    },
    UpsertVolume {
        id: VolumeId,
        primary: String,
    },
    /// Records that `node` has replicated the volume up to `generation`.
    SetVolumeReplica {
        id: VolumeId,
        node: String,
        generation: u64,
    },
    /// Bumps the generation, marking every other replica stale.
    TouchVolume {
        id: VolumeId,
        primary: String,
    },
    StoreCertificate(CertificateState),
    RemoveCertificate {
        fqdn: String,
    },
    UpsertDnsRecord(DnsRecordState),
    RemoveDnsRecord {
        key: String,
    },
    /// Written on leader election so the log always has a fresh entry to commit.
    NoOp,
}

/// Result of applying a command. Failures are values, not errors: a rejected
/// command is still a committed log entry on every node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub version: u64,
    pub error: Option<String>,
}

impl Response {
    fn ok(version: u64) -> Self {
        Self {
            version,
            error: None,
        }
    }

    fn err(version: u64, msg: impl Into<String>) -> Self {
        Self {
            version,
            error: Some(msg.into()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

impl ClusterState {
    /// Apply one command. Must stay deterministic, so no clocks, no randomness
    /// and no IO.
    pub fn apply(&mut self, req: &Request) -> Response {
        let result = self.dispatch(req);
        match result {
            Ok(()) => {
                self.version += 1;
                Response::ok(self.version)
            }
            // A rejected command leaves state untouched, so the version stays put.
            Err(msg) => Response::err(self.version, msg),
        }
    }

    fn dispatch(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::NoOp => Ok(()),

            Request::UpsertNode(node) => {
                self.nodes.insert(node.name.clone(), node.clone());
                Ok(())
            }

            Request::SetAvailability { node, availability } => {
                let entry = self
                    .nodes
                    .get_mut(node)
                    .ok_or_else(|| format!("unknown node {node}"))?;
                entry.availability = *availability;
                Ok(())
            }

            Request::SetReachable { node, reachable } => {
                if !self.nodes.contains_key(node) {
                    return Err(format!("unknown node {node}"));
                }
                if *reachable {
                    self.unreachable.remove(node);
                } else {
                    self.unreachable.insert(node.clone());
                }
                Ok(())
            }

            Request::RemoveNode { node } => {
                if let Some(inst) = self.assignments.values().find(|a| &a.node == node) {
                    return Err(format!("node {node} still runs {}", inst.instance));
                }
                self.nodes.remove(node);
                self.unreachable.remove(node);
                for volume in self.volumes.values_mut() {
                    volume.replicas.remove(node);
                }
                Ok(())
            }

            Request::DeployStack { name, spec, files } => {
                let total: usize = files.values().map(|f| f.len()).sum();
                if total > MAX_STACK_FILES_BYTES {
                    return Err(format!(
                        "stack {name} carries {total} bytes of files, over the {MAX_STACK_FILES_BYTES} byte limit"
                    ));
                }
                for (path, file) in files {
                    if file.decode().is_err() {
                        return Err(format!("attachment {path} is not valid base64"));
                    }
                }

                let revision = self.stacks.get(name).map(|s| s.revision + 1).unwrap_or(1);
                self.stacks.insert(
                    name.clone(),
                    StackState {
                        name: name.clone(),
                        spec: spec.clone(),
                        revision,
                        files: files.clone(),
                    },
                );
                Ok(())
            }

            Request::RemoveStack { name } => {
                self.stacks.remove(name);
                self.assignments.retain(|id, _| &id.stack != name);
                Ok(())
            }

            Request::Assign {
                instance,
                node,
                drives,
            } => {
                self.check_placement(node, drives)?;
                self.assignments.insert(
                    instance.clone(),
                    Assignment {
                        instance: instance.clone(),
                        node: node.clone(),
                        desired: DesiredState::Running,
                        image_digest: None,
                        migration: None,
                    },
                );
                Ok(())
            }

            Request::Unassign { instance } => {
                self.assignments.remove(instance);
                Ok(())
            }

            Request::SetDesiredState { instance, desired } => {
                self.assignment_mut(instance)?.desired = *desired;
                Ok(())
            }

            Request::SetImageDigest { instance, digest } => {
                self.assignment_mut(instance)?.image_digest = Some(digest.clone());
                Ok(())
            }

            Request::BeginMigration {
                instance,
                to,
                drives,
            } => {
                self.check_placement(to, drives)?;
                let assignment = self.assignment_mut(instance)?;
                if assignment.node == *to {
                    return Err(format!("{instance} already runs on {to}"));
                }
                if let Some(active) = &assignment.migration {
                    if !active.phase.is_terminal() {
                        return Err(format!("{instance} is already migrating to {}", active.to));
                    }
                }
                assignment.migration = Some(Migration {
                    to: to.clone(),
                    phase: MigrationPhase::Stopping,
                });
                Ok(())
            }

            Request::AdvanceMigration { instance, phase } => {
                let assignment = self.assignment_mut(instance)?;
                let migration = assignment
                    .migration
                    .as_mut()
                    .ok_or_else(|| format!("{instance} is not migrating"))?;

                // Failure can interrupt at any point, but forward progress must
                // follow the phase order so a stale message cannot rewind a move.
                let allowed = matches!(phase, MigrationPhase::Failed { .. })
                    || migration.phase.next().as_ref() == Some(phase);
                if !allowed {
                    return Err(format!(
                        "cannot advance {instance} from {:?} to {phase:?}",
                        migration.phase
                    ));
                }

                migration.phase = phase.clone();
                match phase {
                    // Switching over is the point of no return: the target owns
                    // the instance from here.
                    MigrationPhase::Switching => assignment.node = migration.to.clone(),
                    MigrationPhase::Done => assignment.migration = None,
                    _ => {}
                }
                Ok(())
            }

            Request::UpsertVolume { id, primary } => {
                if !self.node_implements(primary, &id.drive) {
                    return Err(format!("node {primary} does not implement drive {}", id.drive));
                }
                self.volumes.entry(id.clone()).or_insert_with(|| VolumeState {
                    id: id.clone(),
                    generation: 1,
                    primary: primary.clone(),
                    replicas: BTreeMap::from([(primary.clone(), 1)]),
                });
                Ok(())
            }

            Request::SetVolumeReplica {
                id,
                node,
                generation,
            } => {
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                if *generation > volume.generation {
                    return Err(format!(
                        "replica generation {generation} is ahead of {} for {id}",
                        volume.generation
                    ));
                }
                let entry = volume.replicas.entry(node.clone()).or_insert(0);
                // Sync results can arrive out of order, so never move a replica back.
                *entry = (*entry).max(*generation);
                Ok(())
            }

            Request::TouchVolume { id, primary } => {
                if !self.node_implements(primary, &id.drive) {
                    return Err(format!("node {primary} does not implement drive {}", id.drive));
                }
                let volume = self
                    .volumes
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown volume {id}"))?;
                volume.generation += 1;
                volume.primary = primary.clone();
                volume.replicas.insert(primary.clone(), volume.generation);
                Ok(())
            }

            Request::StoreCertificate(cert) => {
                self.certificates.insert(cert.fqdn.clone(), cert.clone());
                Ok(())
            }

            Request::RemoveCertificate { fqdn } => {
                self.certificates.remove(fqdn);
                Ok(())
            }

            Request::UpsertDnsRecord(record) => {
                let key = dns_key(&record.fqdn, &record.record_type, &record.content);
                self.dns_records.insert(key, record.clone());
                Ok(())
            }

            Request::RemoveDnsRecord { key } => {
                self.dns_records.remove(key);
                Ok(())
            }
        }
    }

    fn assignment_mut(&mut self, instance: &InstanceId) -> Result<&mut Assignment, String> {
        self.assignments
            .get_mut(instance)
            .ok_or_else(|| format!("{instance} is not assigned"))
    }

    fn node_implements(&self, node: &str, drive: &str) -> bool {
        self.nodes.get(node).is_some_and(|n| n.implements(drive))
    }

    /// Whether a node may take new work: the operator has not held it back and
    /// the cluster can still hear from it.
    pub fn can_schedule(&self, node: &str) -> bool {
        self.nodes.get(node).is_some_and(|n| n.schedulable()) && !self.unreachable.contains(node)
    }

    /// The core scheduling invariant: an instance only lands on a node that is
    /// taking work and implements every drive its volumes live on.
    fn check_placement(&self, node: &str, drives: &BTreeSet<String>) -> Result<(), String> {
        let target = self
            .nodes
            .get(node)
            .ok_or_else(|| format!("unknown node {node}"))?;
        if !target.schedulable() {
            return Err(format!("node {node} is {:?}", target.availability));
        }
        if self.unreachable.contains(node) {
            return Err(format!("node {node} is unreachable"));
        }
        for drive in drives {
            if !target.implements(drive) {
                return Err(format!("node {node} does not implement drive {drive}"));
            }
        }
        Ok(())
    }

    /// A copy with every secret blanked, for anything that leaves the node.
    ///
    /// The lengths stay right so the ui can still show that a secret is set
    /// without ever handling its contents.
    pub fn redacted(&self) -> ClusterState {
        let mut copy = self.clone();
        for stack in copy.stacks.values_mut() {
            for file in stack.files.values_mut() {
                if file.secret {
                    file.content = String::new();
                }
            }
        }
        for cert in copy.certificates.values_mut() {
            cert.key_pem = String::new();
        }
        copy
    }

    /// Instances a node should be running right now.
    pub fn assignments_on<'a>(&'a self, node: &'a str) -> impl Iterator<Item = &'a Assignment> {
        self.assignments.values().filter(move |a| a.node == node)
    }
}

/// Base64 helpers, kept here so the encoding used for attachments is defined in
/// exactly one place.
pub fn base64_encode(raw: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(raw)
}

pub fn base64_decode(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| e.to_string())
}

/// Records are keyed by the triple that makes them unique in a zone.
pub fn dns_key(fqdn: &str, record_type: &str, content: &str) -> String {
    format!("{fqdn}|{record_type}|{content}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, drives: &[&str]) -> NodeState {
        NodeState {
            name: name.to_string(),
            role: Role::Manager,
            rpc_addr: "10.10.0.1:7373".parse().unwrap(),
            drives: drives
                .iter()
                .map(|d| (d.to_string(), PathBuf::from(format!("/srv/{d}"))))
                .collect(),
            availability: Availability::Active,
        }
    }

    fn cluster() -> ClusterState {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1", &["fast", "bulk"])));
        state.apply(&Request::UpsertNode(node("n2", &["fast"])));
        state.apply(&Request::UpsertNode(node("n3", &[])));
        state
    }

    fn drives(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn assign(state: &mut ClusterState, node: &str, needs: &[&str]) -> Response {
        state.apply(&Request::Assign {
            instance: InstanceId::new("gitlab", "web", 0),
            node: node.to_string(),
            drives: drives(needs),
        })
    }

    #[test]
    fn version_advances_only_on_accepted_commands() {
        let mut state = cluster();
        let before = state.version;
        let rejected = assign(&mut state, "nowhere", &[]);
        assert!(!rejected.is_ok());
        assert_eq!(state.version, before);

        let accepted = assign(&mut state, "n1", &[]);
        assert!(accepted.is_ok());
        assert_eq!(state.version, before + 1);
    }

    #[test]
    fn placement_requires_every_drive() {
        let mut state = cluster();
        assert!(assign(&mut state, "n1", &["fast", "bulk"]).is_ok());

        let mut state = cluster();
        let rejected = assign(&mut state, "n2", &["fast", "bulk"]);
        assert!(rejected
            .error
            .unwrap()
            .contains("n2 does not implement drive bulk"));
    }

    #[test]
    fn drained_nodes_take_no_new_work() {
        let mut state = cluster();
        state.apply(&Request::SetAvailability {
            node: "n1".into(),
            availability: Availability::Drain,
        });
        let rejected = assign(&mut state, "n1", &[]);
        assert!(rejected.error.unwrap().contains("Drain"));
    }

    #[test]
    fn an_unreachable_node_takes_no_new_work() {
        let mut state = cluster();
        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: false,
        });
        assert!(!state.can_schedule("n1"));

        let rejected = assign(&mut state, "n1", &[]);
        assert!(rejected.error.unwrap().contains("n1 is unreachable"));

        // The node coming back makes it a candidate again.
        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: true,
        });
        assert!(state.can_schedule("n1"));
        assert!(assign(&mut state, "n1", &[]).is_ok());
    }

    #[test]
    fn reachability_is_only_tracked_for_known_nodes() {
        let mut state = cluster();
        let rejected = state.apply(&Request::SetReachable {
            node: "ghost".into(),
            reachable: false,
        });
        assert!(rejected.error.unwrap().contains("unknown node ghost"));
    }

    #[test]
    fn removing_a_node_clears_its_unreachable_flag() {
        let mut state = cluster();
        state.apply(&Request::SetReachable {
            node: "n1".into(),
            reachable: false,
        });
        state.apply(&Request::RemoveNode { node: "n1".into() });
        assert!(state.unreachable.is_empty());
    }

    #[test]
    fn nodes_running_work_cannot_be_removed() {
        let mut state = cluster();
        assign(&mut state, "n1", &[]);
        let rejected = state.apply(&Request::RemoveNode { node: "n1".into() });
        assert!(rejected.error.unwrap().contains("still runs gitlab.web.0"));

        state.apply(&Request::Unassign {
            instance: InstanceId::new("gitlab", "web", 0),
        });
        assert!(state.apply(&Request::RemoveNode { node: "n1".into() }).is_ok());
        assert!(!state.nodes.contains_key("n1"));
    }

    #[test]
    fn redeploying_a_stack_bumps_the_revision() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "a".into(),
            files: BTreeMap::new(),
        });
        assert_eq!(state.stacks["gitlab"].revision, 1);
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "b".into(),
            files: BTreeMap::new(),
        });
        assert_eq!(state.stacks["gitlab"].revision, 2);
        assert_eq!(state.stacks["gitlab"].spec, "b");
    }

    fn attachment(content: &str, secret: bool) -> Attachment {
        Attachment {
            content: base64_encode(content.as_bytes()),
            secret,
        }
    }

    #[test]
    fn stack_files_are_stored_with_the_spec() {
        let mut state = ClusterState::default();
        let files = BTreeMap::from([
            ("nginx_conf".to_string(), attachment("server {}", false)),
            ("db_password".to_string(), attachment("hunter2", true)),
        ]);

        assert!(state
            .apply(&Request::DeployStack {
                name: "site".into(),
                spec: "services: {}".into(),
                files: files.clone(),
            })
            .is_ok());

        let stored = &state.stacks["site"];
        assert_eq!(stored.files.len(), 2);
        assert_eq!(stored.files["nginx_conf"].decode().unwrap(), b"server {}");
    }

    #[test]
    fn oversized_stack_files_are_refused() {
        let mut state = ClusterState::default();
        let big = "x".repeat(MAX_STACK_FILES_BYTES + 1024);
        let rejected = state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services: {}".into(),
            files: BTreeMap::from([("big".to_string(), attachment(&big, false))]),
        });
        assert!(rejected.error.unwrap().contains("over the"));
        assert!(state.stacks.is_empty());
    }

    #[test]
    fn malformed_attachments_are_refused() {
        let mut state = ClusterState::default();
        let rejected = state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services: {}".into(),
            files: BTreeMap::from([(
                "bad".to_string(),
                Attachment {
                    content: "not base64!!".into(),
                    secret: false,
                },
            )]),
        });
        assert!(rejected.error.unwrap().contains("not valid base64"));
    }

    #[test]
    fn redaction_blanks_secrets_but_keeps_configs() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services: {}".into(),
            files: BTreeMap::from([
                ("conf".to_string(), attachment("public", false)),
                ("pass".to_string(), attachment("hunter2", true)),
            ]),
        });
        state.apply(&Request::StoreCertificate(CertificateState {
            fqdn: "a.example.com".into(),
            chain_pem: "chain".into(),
            key_pem: "private".into(),
            not_after: 0,
        }));

        let safe = state.redacted();
        assert_eq!(safe.stacks["site"].files["conf"].decode().unwrap(), b"public");
        assert!(safe.stacks["site"].files["pass"].content.is_empty());
        assert!(safe.certificates["a.example.com"].key_pem.is_empty());
        // The original is untouched.
        assert!(!state.stacks["site"].files["pass"].content.is_empty());
    }

    #[test]
    fn secret_contents_never_appear_in_debug_output() {
        let secret = attachment("hunter2", true);
        let rendered = format!("{secret:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("hunter2"));
        // The base64 of the secret must not leak either.
        assert!(!rendered.contains(&base64_encode(b"hunter2")));
    }

    #[test]
    fn removing_a_stack_drops_its_assignments() {
        let mut state = cluster();
        assign(&mut state, "n1", &[]);
        state.apply(&Request::RemoveStack {
            name: "gitlab".into(),
        });
        assert!(state.assignments.is_empty());
    }

    #[test]
    fn migration_walks_the_phases_in_order() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);

        assert!(state
            .apply(&Request::BeginMigration {
                instance: instance.clone(),
                to: "n2".into(),
                drives: drives(&["fast"]),
            })
            .is_ok());

        // Skipping ahead is refused, so a stale message cannot rewind a move.
        let skipped = state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Starting,
        });
        assert!(skipped.error.unwrap().contains("cannot advance"));

        for phase in [
            MigrationPhase::Syncing,
            MigrationPhase::Verifying,
            MigrationPhase::Switching,
        ] {
            assert!(state
                .apply(&Request::AdvanceMigration {
                    instance: instance.clone(),
                    phase,
                })
                .is_ok());
        }
        // Ownership changes at the switch, before the container starts.
        assert_eq!(state.assignments[&instance].node, "n2");

        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Starting,
        });
        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Done,
        });
        assert!(state.assignments[&instance].migration.is_none());
    }

    #[test]
    fn migration_target_must_implement_the_drives() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["bulk"]);
        let rejected = state.apply(&Request::BeginMigration {
            instance,
            to: "n2".into(),
            drives: drives(&["bulk"]),
        });
        assert!(rejected.error.unwrap().contains("does not implement drive bulk"));
    }

    #[test]
    fn migration_can_fail_from_any_phase() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);
        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        assert!(state
            .apply(&Request::AdvanceMigration {
                instance: instance.clone(),
                phase: MigrationPhase::Failed {
                    reason: "sync timed out".into()
                },
            })
            .is_ok());
        // The instance stays on the source node when the move fails early.
        assert_eq!(state.assignments[&instance].node, "n1");
    }

    #[test]
    fn one_migration_at_a_time() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);
        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        let rejected = state.apply(&Request::BeginMigration {
            instance,
            to: "n3".into(),
            drives: drives(&[]),
        });
        assert!(rejected.error.unwrap().contains("already migrating"));
    }

    #[test]
    fn volume_generation_marks_replicas_stale() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: "n2".into(),
            generation: 1,
        });
        assert_eq!(state.volumes[&id].in_sync(), ["n1", "n2"].into());

        state.apply(&Request::TouchVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        assert_eq!(state.volumes[&id].in_sync(), ["n1"].into());
        assert_eq!(state.volumes[&id].generation, 2);
    }

    #[test]
    fn replica_progress_never_goes_backwards() {
        let mut state = cluster();
        let id = VolumeId::new("fast", "data");
        state.apply(&Request::UpsertVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::TouchVolume {
            id: id.clone(),
            primary: "n1".into(),
        });
        state.apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: "n2".into(),
            generation: 2,
        });
        // A late report from an older sync must not undo the newer one.
        state.apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: "n2".into(),
            generation: 1,
        });
        assert_eq!(state.volumes[&id].replicas["n2"], 2);
    }

    #[test]
    fn volumes_only_live_on_implementing_nodes() {
        let mut state = cluster();
        let rejected = state.apply(&Request::UpsertVolume {
            id: VolumeId::new("bulk", "data"),
            primary: "n2".into(),
        });
        assert!(rejected.error.unwrap().contains("does not implement drive bulk"));
    }

    #[test]
    fn assignments_on_filters_by_node() {
        let mut state = cluster();
        assign(&mut state, "n1", &[]);
        state.apply(&Request::Assign {
            instance: InstanceId::new("gitlab", "web", 1),
            node: "n2".into(),
            drives: drives(&["fast"]),
        });
        assert_eq!(state.assignments_on("n1").count(), 1);
        assert_eq!(state.assignments_on("n2").count(), 1);
        assert_eq!(state.assignments_on("n3").count(), 0);
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = cluster();
        assign(&mut state, "n1", &["fast"]);
        let encoded = serde_json::to_vec(&state).unwrap();
        let decoded: ClusterState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(state, decoded);
    }
}
