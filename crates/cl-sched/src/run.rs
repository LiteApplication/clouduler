//! The scheduler loop.
//!
//! Two jobs on one timer. The leader decides where everything should run and
//! writes those decisions through raft. Every node, leader or not, then makes
//! its own docker match the decisions that name it. Splitting it this way means
//! a node never waits on the leader to start a container it has already been
//! given, and the leader never has to reach into anyone else's docker.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use cl_config::StackFile;
use cl_docker::Docker;
use cl_net::Allocations;
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::node::ClusterNode;
use cl_raft::state::{ClusterState, DesiredState, Request};
use tracing::{debug, info, warn};

use crate::materialize::Materializer;
use crate::place;
use crate::plan;
use crate::reconcile::{self, Reconciler, Target, DEFAULT_STOP_GRACE};
use crate::spec;

/// How often to sweep, on top of reacting to state changes. A periodic pass
/// catches drift docker never told us about, like a container someone stopped
/// by hand.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

pub struct Scheduler {
    node: String,
    cluster: Arc<ClusterNode>,
    docker: Docker,
    reconciler: Reconciler,
    materializer: Materializer,
    /// Internal DNS zone, written into every container's search list.
    zone: Option<String>,
}

impl Scheduler {
    pub fn new(
        node: String,
        cluster: Arc<ClusterNode>,
        docker: Docker,
        materializer: Materializer,
    ) -> Self {
        Self {
            node,
            cluster,
            reconciler: Reconciler::new(docker.clone()),
            docker,
            materializer,
            zone: None,
        }
    }

    /// Set the internal DNS zone containers search, so `web.blog` resolves
    /// without the cluster's domain being spelled out in every stack file.
    pub fn with_zone(mut self, zone: impl Into<String>) -> Self {
        self.zone = Some(zone.into());
        self
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
        let mut desired = Vec::new();
        for (name, stack) in &state.stacks {
            match StackFile::parse(&stack.spec) {
                Ok(file) => desired.extend(plan::expand(name, &file)),
                // A stored stack that no longer parses is the operator's
                // problem, but it must not stop every other stack scheduling.
                Err(e) => warn!(stack = %name, error = %e, "stored stack does not parse"),
            }
        }

        let decisions = match place::place(state, &desired) {
            Ok(decisions) => decisions,
            Err(e) => {
                warn!(error = %e, "placement failed");
                return Ok(());
            }
        };

        for decision in decisions {
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
        let targets = self.targets(state)?;
        let actual = self.docker.list_managed().await?;
        let actions = reconcile::diff(&targets, &actual, fenced);

        if actions.is_empty() {
            debug!("nothing to reconcile");
        } else {
            info!(actions = actions.len(), fenced, "reconciling");
        }
        self.reconciler.apply(&actions, &targets).await?;

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
        let drive_roots = state
            .nodes
            .get(&self.node)
            .map(|n| n.drives.clone())
            .unwrap_or_default();

        // Decoded once per pass rather than per instance: every container on
        // this node reads out of the same address book.
        let allocations = Allocations::decode(state.overlay.as_deref());

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
                &objects,
                &drive_roots,
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

/// Every request the leader would submit for a state, without submitting them.
/// Exists so the scheduling decision can be tested without a cluster.
pub fn plan_requests(state: &ClusterState) -> crate::Result<Vec<Request>> {
    let mut desired = Vec::new();
    for (name, stack) in &state.stacks {
        if let Ok(file) = StackFile::parse(&stack.spec) {
            desired.extend(plan::expand(name, &file));
        }
    }

    let decisions = place::place(state, &desired)?;
    Ok(decisions
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
            role: Role::Manager,
            rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
            drives: drives
                .iter()
                .map(|d| (d.to_string(), PathBuf::from(format!("/srv/{d}"))))
                .collect(),
            availability: Availability::Active,
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
