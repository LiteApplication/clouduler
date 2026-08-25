//! The scheduler loop.
//!
//! Two jobs on one timer. The leader decides where everything should run and
//! writes those decisions through raft. Every node, leader or not, then makes
//! its own docker match the decisions that name it. Splitting it this way means
//! a node never waits on the leader to start a container it has already been
//! given, and the leader never has to reach into anyone else's docker.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cl_config::{ClusterConfig, StackFile};
use cl_docker::Docker;
use cl_net::Allocations;
use tokio::sync::RwLock;
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::node::ClusterNode;
use cl_raft::state::{ClusterState, DesiredState, Request};
use tracing::{debug, info, warn};

use crate::materialize::Materializer;
use crate::place;
use crate::plan::{self, Desired};
use crate::reconcile::{self, PullTracker, Reconciler, Target, DEFAULT_STOP_GRACE};
use crate::spec;

/// How often to sweep, on top of reacting to state changes. A periodic pass
/// catches drift docker never told us about, like a container someone stopped
/// by hand.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

pub struct Scheduler {
    node: String,
    cluster: Arc<ClusterNode>,
    /// A live view of the docker connection rather than a snapshot: a node
    /// started before dockerd was up, or whose dockerd is later restarted on
    /// its own, keeps working once `docker` is reconnected, without a daemon
    /// restart. `None` is a normal, temporary state, not an error.
    docker: Arc<RwLock<Option<Docker>>>,
    materializer: Materializer,
    /// Internal DNS zone, written into every container's search list.
    zone: Option<String>,
    /// The config this node booted with, used only as a fallback until the
    /// replicated copy is available (`live_config`). Which networks are
    /// egress- or public-kind is re-derived from the *live* config on every
    /// pass rather than frozen from this at construction, so a rename or
    /// kind change takes effect on the next sweep instead of needing a
    /// restart.
    boot_config: ClusterConfig,
    /// Instances this node's reconciler is currently pulling the image for,
    /// shared with the api so it can report a "pulling" instance state.
    pulling: PullTracker,
}

impl Scheduler {
    pub fn new(
        node: String,
        cluster: Arc<ClusterNode>,
        docker: Arc<RwLock<Option<Docker>>>,
        materializer: Materializer,
        boot_config: ClusterConfig,
        pulling: PullTracker,
    ) -> Self {
        Self {
            node,
            cluster,
            docker,
            materializer,
            zone: None,
            boot_config,
            pulling,
        }
    }

    /// Set the internal DNS zone containers search, so `web.blog` resolves
    /// without the cluster's domain being spelled out in every stack file.
    pub fn with_zone(mut self, zone: impl Into<String>) -> Self {
        self.zone = Some(zone.into());
        self
    }

    /// The cluster config as replicated state currently has it, falling back
    /// to the config this node booted with only for the brief window before
    /// any leader has published one, or if the replicated copy fails to
    /// parse.
    fn live_config(&self, state: &ClusterState) -> ClusterConfig {
        match &state.cluster_config {
            Some(entry) => ClusterConfig::parse(&entry.yaml).unwrap_or_else(|e| {
                warn!(error = %e, "replicated cluster config failed to parse, using the config this node booted with");
                self.boot_config.clone()
            }),
            None => self.boot_config.clone(),
        }
    }

    /// Run until cancelled. `fenced` reports whether this node has lost quorum
    /// long enough to give up its containers.
    pub async fn run(self, fenced: impl Fn() -> bool + Send + 'static) {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            ticker.tick().await;

            let state = self.cluster.state().await;
            let fenced_now = fenced();

            if self.cluster.is_leader() && !fenced_now {
                if let Err(e) = self.schedule(&state).await {
                    warn!(error = %e, "scheduling pass failed");
                }
            }

            if let Err(e) = self.reconcile(&state, fenced_now).await {
                warn!(error = %e, "reconcile pass failed");
            }
        }
    }

    /// Leader side: work out where everything belongs and record it.
    async fn schedule(&self, state: &ClusterState) -> crate::Result<()> {
        let live = self.live_config(state);
        let colocate_proxy_hosts = live.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
        let desired = desired_instances(state, &live.egress_networks(), &live.port_gated_networks(), colocate_proxy_hosts);

        let outcome = place::place(state, &desired, &live.served_drives());
        for unplaceable in &outcome.unplaceable {
            warn!(instance = %unplaceable.instance, reason = %unplaceable.reason, "cannot place instance");
        }

        for decision in outcome.decisions {
            let drives = desired
                .iter()
                .find(|d| &d.id == decision.instance())
                .map(|d| d.drives.clone())
                .unwrap_or_default();

            let request = decision.clone().into_request(drives);
            let response = self.cluster.submit(request).await.map_err(Box::new)?;
            if let Some(error) = response.error {
                warn!(instance = %decision.instance(), %error, "placement rejected");
            } else {
                info!(instance = %decision.instance(), "placed");
            }
        }

        for request in volume_requests(state) {
            let response = self.cluster.submit(request.clone()).await.map_err(Box::new)?;
            if let Some(error) = response.error {
                warn!(?request, %error, "registering a volume was rejected");
            }
        }
        Ok(())
    }

    /// Node side: make docker match the assignments naming this node.
    async fn reconcile(&self, state: &ClusterState, fenced: bool) -> crate::Result<()> {
        // No docker to reconcile against yet (or right now) is not an error:
        // this pass is simply skipped, and the next one tries again once
        // `docker` is reconnected.
        let Some(docker) = self.docker.read().await.clone() else {
            return Ok(());
        };
        let reconciler = Reconciler::new(docker.clone(), self.pulling.clone());

        let targets = self.targets(state)?;
        let actual = docker.list_managed().await?;
        let actions = reconcile::diff(&targets, &actual, fenced);

        if actions.is_empty() {
            debug!("nothing to reconcile");
        } else {
            info!(actions = actions.len(), fenced, "reconciling");
        }
        reconciler.apply(&actions, &targets, &actual).await?;

        // Objects for instances that have moved on must not linger on disk.
        let keep: BTreeMap<String, ()> = targets
            .iter()
            .map(|t| (t.assignment.instance.container_name(), ()))
            .collect();
        if let Err(e) = self.materializer.clean_orphans(&keep) {
            warn!(error = %e, "could not clean orphaned objects");
        }
        Ok(())
    }

    /// Build what this node should be running, materializing each instance's
    /// configs and secrets on the way.
    fn targets(&self, state: &ClusterState) -> crate::Result<Vec<Target>> {
        let live = self.live_config(state);
        let (drive_roots, dynamic_mounts) = self.drive_roots(state, &live);

        // Decoded once per pass rather than per instance: every container on
        // this node reads out of the same address book.
        let allocations = Allocations::decode(state.overlay.as_deref());
        let egress_networks = live.egress_networks();

        let mut targets = Vec::new();

        for assignment in state.assignments_on(&self.node) {
            let instance = &assignment.instance;
            let Some(stack) = state.stacks.get(&instance.stack) else {
                continue;
            };
            let file = match StackFile::parse(&stack.spec) {
                Ok(file) => file,
                Err(e) => {
                    warn!(stack = %instance.stack, error = %e, "stored stack does not parse");
                    continue;
                }
            };
            let Some(service) = file.services.get(&instance.service) else {
                continue;
            };

            // A served-drive volume this node isn't the active server for is
            // backed by a FUSE mount the drive subsystem sets up on demand,
            // not necessarily up yet the first time an instance lands here.
            // Starting the container against an empty, not-yet-mounted
            // directory would bind it to the wrong data, so wait rather than
            // start early; the next sweep tries again.
            if !volumes_ready(service, &drive_roots, &dynamic_mounts) {
                debug!(instance = %instance, "waiting for a served drive volume to be mounted");
                continue;
            }

            // A missing secret or a node without the key is a reason not to
            // start this one container, not to abandon the whole pass.
            let objects = match self.materializer.prepare(
                state,
                instance,
                &service.configs,
                &service.secrets,
            ) {
                Ok(objects) => objects,
                Err(e) => {
                    warn!(instance = %instance, error = %e, "cannot prepare objects, skipping");
                    continue;
                }
            };

            let network = file.networks.keys().next().map(|n| overlay_name(&instance.stack, n));

            // A container on a stack network gets the address the leader
            // allocated for it, not one docker picks: the other nodes read
            // that address out of replicated state to build DNS answers and
            // proxy backends, and can only do that if it was decided cluster
            // wide. Nothing to run yet if it has not been allocated.
            let mut placement = spec::Placement {
                network: network.clone(),
                ..Default::default()
            };
            if let Some(network) = &network {
                let Some(ip) = allocations.as_ref().and_then(|a| a.instance_ip(network, instance))
                else {
                    debug!(instance = %instance, network = %network, "no overlay address yet");
                    continue;
                };
                placement.ip = Some(ip);
                placement.resolver = allocations.as_ref().and_then(|a| a.gateway(network));
                placement.zone = self.zone.clone();
            }
            placement.egress_network = service
                .networks
                .iter()
                .find(|n| egress_networks.contains(*n))
                .map(|n| cl_net::egress::egress_docker_name(n));

            // An instance on its way somewhere else must not be started here
            // again between the migration stopping it and the handover
            // finishing: the two loops would fight, and every restart would
            // dirty the volume the final sync is trying to freeze. Recorded as
            // a stopped target rather than dropped from the list, so the
            // container stays in place and a failed migration is one start
            // away from being back where it was.
            //
            // Past `Switching` the assignment names the target, so this same
            // test lets the target start it: `to` is the node reading it.
            let mut assignment = assignment.clone();
            if assignment
                .migration
                .as_ref()
                .is_some_and(|m| !m.phase.is_terminal() && m.to != self.node)
            {
                assignment.desired = DesiredState::Stopped;
            }

            let spec = spec::build(
                instance,
                service,
                spec::Materialization {
                    objects: &objects.mounts,
                    env: &objects.env,
                    drive_roots: &drive_roots,
                },
                placement,
                assignment.image_digest.as_deref(),
                stack.revision,
            );

            targets.push(Target {
                assignment,
                spec,
                revision: stack.revision,
                stop_grace: service.stop_grace_period.unwrap_or(DEFAULT_STOP_GRACE),
            });
        }

        Ok(targets)
    }

    /// Where this node resolves each drive's root for mounting purposes, and
    /// which of those roots are FUSE mountpoints the drive subsystem owns
    /// (`crate::run::volumes_ready` waits on those before starting a
    /// container).
    ///
    /// A replicated drive always resolves to this node's own declared local
    /// copy (`NodeState::drives`), exactly as before. A served drive resolves
    /// the same way only for the node currently serving it; every other node
    /// — whether or not it declared the drive at all — resolves to a
    /// per-volume directory under its data dir, which the drive subsystem
    /// (`clouduler::drives`) is responsible for FUSE-mounting from the active
    /// server before a container needing it can start.
    fn drive_roots(
        &self,
        state: &ClusterState,
        live: &ClusterConfig,
    ) -> (BTreeMap<String, PathBuf>, BTreeSet<String>) {
        let mut roots = state
            .nodes
            .get(&self.node)
            .map(|n| n.drives.clone())
            .unwrap_or_default();
        let mut dynamic = BTreeSet::new();

        let data_dir = live
            .node(&self.node)
            .map(|n| n.data_dir(live))
            .unwrap_or(live.data_dir.as_path());

        for (name, drive) in &live.drives {
            if !drive.is_served() {
                continue;
            }
            if active_server(drive, &state.unreachable) != Some(self.node.as_str()) {
                roots.insert(name.clone(), data_dir.join("volume-mounts").join(name));
                dynamic.insert(name.clone());
            }
        }

        (roots, dynamic)
    }
}

/// The node currently serving a `Served` drive: the first of `served_by` that
/// is not reported unreachable. Mirrors `cl_drive::registry::DriveRegistry`'s
/// `active_server`, computed independently here since placement/mount
/// resolution only need the answer, not the rest of that crate.
fn active_server<'a>(drive: &'a cl_config::Drive, unreachable: &BTreeSet<String>) -> Option<&'a str> {
    drive.served_by.iter().map(String::as_str).find(|n| !unreachable.contains(*n))
}

/// Whether `path` is itself a mount point (its device differs from its
/// parent's), the cheap way to tell a live FUSE mount from an ordinary, not
/// yet mounted local directory without depending on `cl-drive`.
fn is_mounted(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    let Ok(parent_meta) = std::fs::metadata(parent) else {
        return false;
    };
    meta.dev() != parent_meta.dev()
}

/// Whether every served-drive volume this service mounts is actually mounted
/// on this node yet. A replicated-drive volume, or a served drive this node
/// is the active server for, is always ready — only a `dynamic` (FUSE, drive
/// subsystem owned) mount can be not-yet-up.
fn volumes_ready(
    service: &cl_config::Service,
    drive_roots: &BTreeMap<String, PathBuf>,
    dynamic: &BTreeSet<String>,
) -> bool {
    service.volumes.iter().all(|mount| {
        let (Some(drive), Some(volume)) = (mount.drive.as_ref(), mount.volume.as_ref()) else {
            return true;
        };
        if !dynamic.contains(drive) {
            return true;
        }
        let Some(root) = drive_roots.get(drive) else {
            return true;
        };
        is_mounted(&root.join(volume))
    })
}

/// `UpsertVolume` for every volume a placed instance mounts that the cluster
/// does not know about yet.
///
/// Nothing else creates a volume record, and without one there is nothing for
/// the sync scheduler to replicate, nothing for `/api/volumes` to report and
/// nothing for a migration to hand over: the mount would work on the node that
/// happens to hold the directory and the data would exist nowhere else.
///
/// Only unknown volumes are submitted. A volume already on record has a
/// primary that migration moves around, and re-asserting the assignment's node
/// every sweep would either fight that or write a raft entry per volume per
/// ten seconds forever.
pub fn volume_requests(state: &ClusterState) -> Vec<Request> {
    let mut requests = Vec::new();
    // Every replica of a service mounts the same volume, so the same id comes
    // up once per instance.
    let mut seen = BTreeSet::new();
    for assignment in state.assignments.values() {
        let instance = &assignment.instance;
        let Some(stack) = state.stacks.get(&instance.stack) else {
            continue;
        };
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        let Some(service) = file.services.get(&instance.service) else {
            continue;
        };
        for mount in &service.volumes {
            let (Some(drive), Some(volume)) = (mount.drive.as_ref(), mount.volume.as_ref()) else {
                continue;
            };
            let id = VolumeId::new(drive, volume);
            if state.volumes.contains_key(&id) || !seen.insert(id.clone()) {
                continue;
            }
            requests.push(Request::UpsertVolume {
                id,
                primary: assignment.node.clone(),
            });
        }
    }
    requests
}

/// Docker network name for a stack's declared network.
pub fn overlay_name(stack: &str, network: &str) -> String {
    format!("{stack}_{network}")
}

/// Drives every instance of a stack needs, for the placement call.
pub fn drives_for(file: &StackFile) -> BTreeSet<String> {
    file.drives_used()
}

/// Instances a stack should have, used by the api to report progress.
pub fn instances_of(state: &ClusterState, stack: &str) -> Vec<InstanceId> {
    state
        .assignments
        .keys()
        .filter(|id| id.stack == stack)
        .cloned()
        .collect()
}

/// Every instance every deployed stack wants, expanded from its spec against
/// the cluster's egress/public network sets (see `ClusterConfig::egress_networks`
/// / `port_gated_networks`). Shared by the scheduler and by anything else (the
/// api's "why is this instance unplaced" reporting) that needs the same
/// expansion without duplicating it.
pub fn desired_instances(
    state: &ClusterState,
    egress_networks: &BTreeSet<String>,
    port_gated_networks: &BTreeSet<String>,
    colocate_proxy_hosts: bool,
) -> Vec<Desired> {
    let mut desired = Vec::new();
    for (name, stack) in &state.stacks {
        match StackFile::parse(&stack.spec) {
            Ok(file) => desired.extend(plan::expand(
                name,
                &file,
                egress_networks,
                port_gated_networks,
                colocate_proxy_hosts,
            )),
            // A stored stack that no longer parses is the operator's
            // problem, but it must not stop every other stack scheduling.
            Err(e) => warn!(stack = %name, error = %e, "stored stack does not parse"),
        }
    }
    desired
}

/// Drives whose mode is `Served`, read from the replicated cluster config.
/// Empty (falling back to strict replicated-drive eligibility) if no config
/// has been published yet or it fails to parse.
fn served_drives(state: &ClusterState) -> BTreeSet<String> {
    state
        .cluster_config
        .as_ref()
        .and_then(|entry| ClusterConfig::parse(&entry.yaml).ok())
        .map(|cfg| cfg.served_drives())
        .unwrap_or_default()
}

/// Every request the leader would submit for a state, without submitting them.
/// Exists so the scheduling decision can be tested without a cluster.
pub fn plan_requests(state: &ClusterState) -> crate::Result<Vec<Request>> {
    let desired = desired_instances(state, &BTreeSet::new(), &BTreeSet::new(), false);

    let outcome = place::place(state, &desired, &served_drives(state));
    Ok(outcome
        .decisions
        .into_iter()
        .map(|decision| {
            let drives = desired
                .iter()
                .find(|d| &d.id == decision.instance())
                .map(|d| d.drives.clone())
                .unwrap_or_default();
            decision.into_request(drives)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{Availability, NodeState, Role};
    use std::path::PathBuf;

    fn node(name: &str, drives: &[&str]) -> NodeState {
        NodeState {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            role: Role::Manager,
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

    const STACK: &str = r#"
services:
  web:
    image: nginx
    deploy:
      replicas: 2
"#;

    fn state() -> ClusterState {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1", &["fast"])));
        state.apply(&Request::UpsertNode(node("n2", &["fast"])));
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: STACK.into(),
        });
        state
    }

    #[test]
    fn a_deployed_stack_produces_placements() {
        let requests = plan_requests(&state()).expect("plan");
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|r| matches!(r, Request::Assign { .. })));
    }

    #[test]
    fn planning_is_idempotent_once_applied() {
        let mut state = state();
        for request in plan_requests(&state).expect("plan") {
            assert!(state.apply(&request).is_ok());
        }
        // Re-running against a settled cluster must decide nothing new,
        // otherwise the loop would churn containers every sweep.
        assert!(plan_requests(&state).expect("plan").is_empty());
    }

    #[test]
    fn removing_a_stack_unassigns_its_instances() {
        let mut state = state();
        for request in plan_requests(&state).expect("plan") {
            state.apply(&request);
        }
        state.apply(&Request::RemoveStack {
            name: "site".into(),
        });
        assert!(state.assignments.is_empty());
        assert!(plan_requests(&state).expect("plan").is_empty());
    }

    #[test]
    fn a_stack_that_no_longer_parses_is_skipped() {
        let mut state = state();
        state.apply(&Request::DeployStack {
            name: "broken".into(),
            spec: "this is not: [valid yaml".into(),
        });
        // The good stack still schedules.
        let requests = plan_requests(&state).expect("plan");
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn overlay_names_are_namespaced_by_stack() {
        assert_eq!(overlay_name("site", "internal"), "site_internal");
    }

    const STATEFUL: &str = r#"
services:
  db:
    image: postgres
    volumes:
      - fast/data:/var/lib/postgresql
"#;

    fn stateful_state() -> ClusterState {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1", &["fast"])));
        state.apply(&Request::UpsertNode(node("n2", &["fast"])));
        state.apply(&Request::DeployStack {
            name: "app".into(),
            spec: STATEFUL.into(),
        });
        for request in plan_requests(&state).expect("plan") {
            assert!(state.apply(&request).is_ok());
        }
        state
    }

    #[test]
    fn a_placed_volume_is_registered_once() {
        let mut state = stateful_state();
        let requests = volume_requests(&state);
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0],
            Request::UpsertVolume { id, .. } if id == &VolumeId::new("fast", "data")
        ));

        for request in requests {
            assert!(state.apply(&request).is_ok());
        }
        // A registered volume has a primary that migration moves; re-asserting
        // it every sweep would fight that.
        assert!(volume_requests(&state).is_empty());
    }

    #[test]
    fn a_stack_with_no_volumes_registers_nothing() {
        assert!(volume_requests(&state()).is_empty());
    }
}
