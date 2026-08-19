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
    /// Labels from the cluster config, replicated so the scheduler can match
    /// `node.labels.<key>` constraints without reading the config file.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Egress-kind networks this node has a host interface address for. Used
    /// the same way `drives` is: a service on one of these networks can only
    /// be placed on a node that implements it.
    #[serde(default)]
    pub egress_networks: BTreeSet<String>,
    /// `std::env::consts::ARCH` of the running binary, e.g. `x86_64` or
    /// `aarch64`. Lets the self-update orchestrator group nodes by what needs
    /// a native build.
    #[serde(default)]
    pub arch: String,
    /// Blake3 hash of the currently running binary, hex encoded. Lets the
    /// self-update orchestrator confirm a restarted node actually came back
    /// on the freshly built binary rather than just coming back.
    #[serde(default)]
    pub binary_hash: String,
}

impl NodeState {
    pub fn implements(&self, drive: &str) -> bool {
        self.drives.contains_key(drive)
    }

    pub fn implements_egress_network(&self, network: &str) -> bool {
        self.egress_networks.contains(network)
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
    /// Cluster config objects this stack brought with it, so removing the stack
    /// takes them away too.
    #[serde(default)]
    pub configs: BTreeSet<String>,
    /// Cluster secrets this stack brought with it.
    #[serde(default)]
    pub secrets: BTreeSet<String>,
}

/// The cluster config file, replicated so an edit made on one node reaches the
/// file on disk on every node.
///
/// The file is what each daemon reads at startup, and a node that was down
/// during an edit would otherwise come back with a stale one. Held as the raw
/// yaml rather than a parsed `ClusterConfig` so `cl-raft` does not depend on
/// `cl-config`, and so comments and key order survive a round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterConfigEntry {
    pub yaml: String,
    /// Bumped on every accepted edit. A node writes the file only when this
    /// moves, so a restarted daemon does not fight the operator over an
    /// unchanged file.
    pub generation: u64,
}

/// A cluster wide config object, mounted read only into the containers that
/// reference it. Stored in the clear: a config is not a secret.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub name: String,
    /// Base64, so the state machine stays valid json.
    pub content: String,
    /// Bumped on every update, so a node can tell its mounted copy is stale.
    pub revision: u64,
    /// Stack that owns it, if it came from a stack file rather than the ui.
    #[serde(default)]
    pub owner: Option<String>,
}

impl ConfigEntry {
    pub fn decode(&self) -> Result<Vec<u8>, String> {
        base64_decode(&self.content)
    }

    pub fn len(&self) -> usize {
        self.content.len() / 4 * 3
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// A cluster wide secret, mounted at `/run/secrets/<name>` like a docker secret.
///
/// The value is sealed before it reaches here, so the database, its snapshots
/// and any backup of them carry only ciphertext. A node without the cluster key
/// replicates this fine but cannot open it, and so cannot run a container that
/// needs it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretEntry {
    pub name: String,
    /// Base64 of nonce followed by ciphertext, from `cl_secret::Sealed`.
    pub sealed: String,
    pub revision: u64,
    /// Fingerprint of the key it was sealed under, so a node holding the wrong
    /// key can say so instead of reporting a decryption failure.
    pub key_fingerprint: String,
    #[serde(default)]
    pub owner: Option<String>,
}

/// Prints no ciphertext, on the principle that nothing about a secret belongs
/// in a log line.
impl fmt::Debug for SecretEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretEntry")
            .field("name", &self.name)
            .field("revision", &self.revision)
            .field("key_fingerprint", &self.key_fingerprint)
            .finish_non_exhaustive()
    }
}

/// Cap on one config or secret. Same reasoning as the stack file limit: it goes
/// through the raft log and into every snapshot.
pub const MAX_OBJECT_BYTES: usize = 1024 * 1024;

/// Config and secret names become filenames under the mount directory, so they
/// have to be safe to join onto a path.
fn validate_object_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 253 {
        return Err(format!("{name:?} must be between 1 and 253 characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(format!(
            "{name:?} may only contain letters, digits, dot, dash and underscore"
        ));
    }
    if name.starts_with('.') {
        return Err(format!("{name:?} must not start with a dot"));
    }
    Ok(())
}

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
    /// The node driving the move, which is where the instance started.
    ///
    /// Recorded rather than derived from the assignment because `Switching`
    /// repoints the assignment at the target: from that phase on there is no
    /// other way to tell who owns the remaining work, and a node that crashed
    /// mid-move has to be able to recognise its own job when it comes back.
    pub from: String,
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
    /// The cluster config file, so every node can keep its own copy current.
    #[serde(default)]
    pub cluster_config: Option<ClusterConfigEntry>,
    /// Cluster wide config objects, mounted read only into containers.
    #[serde(default)]
    pub configs: BTreeMap<String, ConfigEntry>,
    /// Cluster wide secrets, sealed. Mounted at /run/secrets/<name>.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretEntry>,
    pub certificates: BTreeMap<String, CertificateState>,
    pub dns_records: BTreeMap<String, DnsRecordState>,
    /// Nodes the leader has stopped hearing from. Observed rather than
    /// configured, so it is kept apart from `availability`, which is what the
    /// operator asked for.
    #[serde(default)]
    pub unreachable: BTreeSet<String>,
    /// Overlay subnet and address assignments, as json owned by cl-net. Kept
    /// opaque here so the address book can evolve without touching the state
    /// machine, which every node has to agree on byte for byte.
    #[serde(default)]
    pub overlay: Option<String>,
    /// Incremented on every applied command. Cheap way for watchers to notice
    /// a change without diffing.
    pub version: u64,
}

/// A command submitted through raft. Every mutation of cluster state is one of these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Replace the cluster config file on every node.
    ///
    /// `expect_generation` is the generation the writer read before editing, 0
    /// when nothing was stored yet. A mismatch means someone else edited in
    /// between and the command is refused rather than silently overwriting
    /// their change.
    PutClusterConfig {
        yaml: String,
        expect_generation: u64,
    },
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
    },
    RemoveStack {
        name: String,
    },
    /// Record which cluster objects a stack owns, so removing the stack cleans
    /// them up. Sent after the objects themselves have been written.
    SetStackObjects {
        name: String,
        configs: BTreeSet<String>,
        secrets: BTreeSet<String>,
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
    PutConfig {
        name: String,
        /// Base64 content.
        content: String,
        #[serde(default)]
        owner: Option<String>,
    },
    RemoveConfig {
        name: String,
    },
    /// The value is sealed by the caller: plaintext never travels through the
    /// raft log, so it is never written to disk unencrypted.
    PutSecret {
        name: String,
        sealed: String,
        key_fingerprint: String,
        #[serde(default)]
        owner: Option<String>,
    },
    RemoveSecret {
        name: String,
    },
    /// Overlay address book. Allocation goes through raft so two nodes cannot
    /// hand out the same address for the same stack network.
    SetOverlayAllocations {
        allocations: String,
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

            Request::PutClusterConfig {
                yaml,
                expect_generation,
            } => {
                let current = self
                    .cluster_config
                    .as_ref()
                    .map(|c| c.generation)
                    .unwrap_or(0);
                if current != *expect_generation {
                    return Err(format!(
                        "cluster config was edited concurrently (stored generation {current}, edit was made against {expect_generation})"
                    ));
                }
                self.cluster_config = Some(ClusterConfigEntry {
                    yaml: yaml.clone(),
                    generation: current + 1,
                });
                Ok(())
            }

            Request::PutConfig {
                name,
                content,
                owner,
            } => {
                validate_object_name(name)?;
                if base64_decode(content).is_err() {
                    return Err(format!("config {name} is not valid base64"));
                }
                let revision = self.configs.get(name).map(|c| c.revision + 1).unwrap_or(1);
                let entry = ConfigEntry {
                    name: name.clone(),
                    content: content.clone(),
                    revision,
                    owner: owner.clone(),
                };
                if entry.len() > MAX_OBJECT_BYTES {
                    return Err(format!(
                        "config {name} is over the {MAX_OBJECT_BYTES} byte limit"
                    ));
                }
                self.configs.insert(name.clone(), entry);
                Ok(())
            }

            Request::RemoveConfig { name } => {
                if let Some(user) = self.config_in_use(name) {
                    return Err(format!("config {name} is still used by {user}"));
                }
                self.configs.remove(name);
                Ok(())
            }

            Request::PutSecret {
                name,
                sealed,
                key_fingerprint,
                owner,
            } => {
                validate_object_name(name)?;
                if base64_decode(sealed).is_err() {
                    return Err(format!("secret {name} is not valid base64"));
                }
                let revision = self.secrets.get(name).map(|s| s.revision + 1).unwrap_or(1);
                let entry = SecretEntry {
                    name: name.clone(),
                    sealed: sealed.clone(),
                    revision,
                    key_fingerprint: key_fingerprint.clone(),
                    owner: owner.clone(),
                };
                if entry.sealed.len() / 4 * 3 > MAX_OBJECT_BYTES {
                    return Err(format!(
                        "secret {name} is over the {MAX_OBJECT_BYTES} byte limit"
                    ));
                }
                self.secrets.insert(name.clone(), entry);
                Ok(())
            }

            Request::RemoveSecret { name } => {
                if let Some(user) = self.secret_in_use(name) {
                    return Err(format!("secret {name} is still used by {user}"));
                }
                self.secrets.remove(name);
                Ok(())
            }

            Request::SetOverlayAllocations { allocations } => {
                self.overlay = Some(allocations.clone());
                Ok(())
            }

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

            Request::DeployStack { name, spec } => {
                let revision = self.stacks.get(name).map(|s| s.revision + 1).unwrap_or(1);
                let previous = self.stacks.get(name);
                let configs = previous.map(|s| s.configs.clone()).unwrap_or_default();
                let secrets = previous.map(|s| s.secrets.clone()).unwrap_or_default();
                self.stacks.insert(
                    name.clone(),
                    StackState {
                        name: name.clone(),
                        spec: spec.clone(),
                        revision,
                        configs,
                        secrets,
                    },
                );
                Ok(())
            }

            Request::RemoveStack { name } => {
                // Drop the stack first so the in-use check below sees it gone.
                let removed = self.stacks.remove(name);
                self.assignments.retain(|id, _| &id.stack != name);
                if let Some(stack) = removed {
                    for config in &stack.configs {
                        if self.config_in_use(config).is_none() {
                            self.configs.remove(config);
                        }
                    }
                    for secret in &stack.secrets {
                        if self.secret_in_use(secret).is_none() {
                            self.secrets.remove(secret);
                        }
                    }
                }
                Ok(())
            }

            Request::SetStackObjects {
                name,
                configs,
                secrets,
            } => {
                let stack = self
                    .stacks
                    .get_mut(name)
                    .ok_or_else(|| format!("unknown stack {name}"))?;
                stack.configs = configs.clone();
                stack.secrets = secrets.clone();
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
                    from: assignment.node.clone(),
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

                // Asking for the phase already on record changes nothing, so
                // it succeeds rather than reading as an illegal transition. A
                // driver that retries a submit it never saw the answer to has
                // to be able to tell "already done" from "not allowed".
                if &migration.phase == phase {
                    return Ok(());
                }

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

    /// Stacks referencing a config, so removing one in use is refused rather
    /// than quietly breaking a container on its next restart.
    fn config_in_use(&self, name: &str) -> Option<String> {
        self.stacks
            .values()
            .find(|s| s.configs.iter().any(|c| c == name))
            .map(|s| format!("stack {}", s.name))
    }

    fn secret_in_use(&self, name: &str) -> Option<String> {
        self.stacks
            .values()
            .find(|s| s.secrets.iter().any(|c| c == name))
            .map(|s| format!("stack {}", s.name))
    }

    /// A copy with every secret blanked, for anything that leaves the node.
    ///
    /// The lengths stay right so the ui can still show that a secret is set
    /// without ever handling its contents.
    pub fn redacted(&self) -> ClusterState {
        let mut copy = self.clone();
        for secret in copy.secrets.values_mut() {
            secret.sealed = String::new();
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

    #[test]
    fn the_first_cluster_config_is_stored_at_generation_one() {
        let mut state = ClusterState::default();
        let response = state.apply(&Request::PutClusterConfig {
            yaml: "cluster:\n  name: home\n".into(),
            expect_generation: 0,
        });

        assert!(response.is_ok(), "{:?}", response.error);
        let stored = state.cluster_config.expect("stored");
        assert_eq!(stored.generation, 1);
        assert!(stored.yaml.contains("home"));
    }

    #[test]
    fn a_config_edit_made_against_a_stale_generation_is_refused() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutClusterConfig {
            yaml: "first".into(),
            expect_generation: 0,
        });

        // Two admins both read generation 1 and both save. The second one has
        // not seen the first one's change, so it must not land.
        let first = state.apply(&Request::PutClusterConfig {
            yaml: "second".into(),
            expect_generation: 1,
        });
        let second = state.apply(&Request::PutClusterConfig {
            yaml: "third".into(),
            expect_generation: 1,
        });

        assert!(first.is_ok());
        assert!(second.error.is_some());
        let stored = state.cluster_config.expect("stored");
        assert_eq!(stored.yaml, "second");
        assert_eq!(stored.generation, 2);
    }

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
            labels: Default::default(),
            egress_networks: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
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
        });
        assert_eq!(state.stacks["gitlab"].revision, 1);
        state.apply(&Request::DeployStack {
            name: "gitlab".into(),
            spec: "b".into(),
        });
        assert_eq!(state.stacks["gitlab"].revision, 2);
        assert_eq!(state.stacks["gitlab"].spec, "b");
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
    fn re_recording_the_current_phase_is_a_no_op() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);

        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Syncing,
        });

        // A driver that retried a submit it never saw the answer to must be
        // told "already done", not "not allowed": the second reads as a
        // failure and would abandon a move that is in fact on track.
        let repeat = state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Syncing,
        });
        assert!(repeat.error.is_none());
        assert_eq!(
            state.assignments[&instance].migration.as_ref().unwrap().phase,
            MigrationPhase::Syncing
        );

        // Going backwards is still refused.
        let rewind = state.apply(&Request::AdvanceMigration {
            instance: instance.clone(),
            phase: MigrationPhase::Stopping,
        });
        assert!(rewind.error.unwrap().contains("cannot advance"));
    }

    #[test]
    fn a_migration_records_the_node_driving_it() {
        let mut state = cluster();
        let instance = InstanceId::new("gitlab", "web", 0);
        assign(&mut state, "n1", &["fast"]);

        state.apply(&Request::BeginMigration {
            instance: instance.clone(),
            to: "n2".into(),
            drives: drives(&["fast"]),
        });
        let migration = state.assignments[&instance].migration.clone().unwrap();
        assert_eq!(migration.from, "n1");

        // `Switching` repoints the assignment, so `from` is the only thing
        // that still says whose job this is.
        for phase in [
            MigrationPhase::Syncing,
            MigrationPhase::Verifying,
            MigrationPhase::Switching,
        ] {
            state.apply(&Request::AdvanceMigration {
                instance: instance.clone(),
                phase,
            });
        }
        assert_eq!(state.assignments[&instance].node, "n2");
        assert_eq!(
            state.assignments[&instance].migration.as_ref().unwrap().from,
            "n1"
        );
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

    #[test]
    fn configs_are_stored_and_versioned() {
        let mut state = ClusterState::default();
        assert!(state
            .apply(&Request::PutConfig {
                name: "nginx_conf".into(),
                content: base64_encode(b"server {}"),
                owner: None,
            })
            .is_ok());
        assert_eq!(state.configs["nginx_conf"].revision, 1);
        assert_eq!(state.configs["nginx_conf"].decode().unwrap(), b"server {}");

        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 80; }"),
            owner: None,
        });
        assert_eq!(state.configs["nginx_conf"].revision, 2);
    }

    #[test]
    fn secrets_are_stored_sealed_and_redacted() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutSecret {
            name: "db_password".into(),
            sealed: base64_encode(b"ciphertext-goes-here"),
            key_fingerprint: "abc123".into(),
            owner: None,
        });

        assert_eq!(state.secrets["db_password"].key_fingerprint, "abc123");
        // Nothing about the value leaves the node.
        assert!(state.redacted().secrets["db_password"].sealed.is_empty());
        // Nor reaches a log line.
        let rendered = format!("{:?}", state.secrets["db_password"]);
        assert!(!rendered.contains("ciphertext"));
    }

    /// The self-update deploy key rides the ordinary secrets map under a
    /// reserved name, with no special casing in `redacted()`. This confirms
    /// that falls out for free rather than needing its own blanking rule.
    #[test]
    fn redacted_blanks_the_self_update_deploy_key_like_any_other_secret() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutSecret {
            name: "_clouduler_self_update_deploy_key".into(),
            sealed: base64_encode(b"ssh-private-key-ciphertext"),
            key_fingerprint: "abc123".into(),
            owner: None,
        });
        assert!(state
            .redacted()
            .secrets["_clouduler_self_update_deploy_key"]
            .sealed
            .is_empty());
    }

    #[test]
    fn object_names_must_be_safe_as_filenames() {
        let mut state = ClusterState::default();
        for bad in ["../escape", "with/slash", ".hidden", ""] {
            let rejected = state.apply(&Request::PutConfig {
                name: bad.into(),
                content: base64_encode(b"x"),
                owner: None,
            });
            assert!(!rejected.is_ok(), "{bad} should be refused");
        }
        assert!(state.configs.is_empty());
    }

    #[test]
    fn objects_in_use_cannot_be_removed() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services: {}".into(),
        });
        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"x"),
            owner: Some("site".into()),
        });
        state.apply(&Request::SetStackObjects {
            name: "site".into(),
            configs: BTreeSet::from(["nginx_conf".to_string()]),
            secrets: BTreeSet::new(),
        });

        let rejected = state.apply(&Request::RemoveConfig {
            name: "nginx_conf".into(),
        });
        assert!(rejected.error.unwrap().contains("still used by stack site"));
    }

    #[test]
    fn removing_a_stack_takes_its_objects_with_it() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "services: {}".into(),
        });
        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"x"),
            owner: Some("site".into()),
        });
        state.apply(&Request::PutSecret {
            name: "db_password".into(),
            sealed: base64_encode(b"y"),
            key_fingerprint: "abc".into(),
            owner: Some("site".into()),
        });
        state.apply(&Request::SetStackObjects {
            name: "site".into(),
            configs: BTreeSet::from(["nginx_conf".to_string()]),
            secrets: BTreeSet::from(["db_password".to_string()]),
        });

        state.apply(&Request::RemoveStack {
            name: "site".into(),
        });
        assert!(state.configs.is_empty());
        assert!(state.secrets.is_empty());
    }

    #[test]
    fn redeploying_a_stack_keeps_its_object_ownership() {
        let mut state = ClusterState::default();
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "a".into(),
        });
        state.apply(&Request::SetStackObjects {
            name: "site".into(),
            configs: BTreeSet::from(["c".to_string()]),
            secrets: BTreeSet::new(),
        });
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: "b".into(),
        });
        assert_eq!(state.stacks["site"].configs.len(), 1);
    }

    #[test]
    fn an_unused_object_can_be_removed() {
        let mut state = ClusterState::default();
        state.apply(&Request::PutConfig {
            name: "orphan".into(),
            content: base64_encode(b"x"),
            owner: None,
        });
        assert!(state
            .apply(&Request::RemoveConfig {
                name: "orphan".into()
            })
            .is_ok());
        assert!(state.configs.is_empty());
    }
}
