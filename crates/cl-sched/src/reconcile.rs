//! The per-node reconcile loop: make docker match what the cluster decided.
//!
//! The diff is a pure function so every branch is testable without a docker
//! daemon. Applying it is the only part that touches the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use cl_docker::{ContainerSpec, ContainerState, Docker, DockerError, LABEL_INSTANCE};
use cl_raft::ids::InstanceId;
use cl_raft::state::{Assignment, DesiredState};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::error::Result;

/// Instances whose image this node's reconciler is currently pulling. Shared
/// with the api so `GET /api/instances` can report "pulling" instead of
/// guessing from the absence of a container — nothing in a docker inspect
/// distinguishes "about to be created" from "still fetching the image".
pub type PullTracker = Arc<RwLock<BTreeSet<InstanceId>>>;

/// What has to happen to one container to bring it in line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Not there at all, create and start it.
    Create(InstanceId),
    /// There but stopped, and it should be running.
    Start(InstanceId),
    /// Running but it should not be.
    Stop(InstanceId),
    /// Not ours any more, take it away.
    Remove(InstanceId),
    /// The spec changed, so it has to be replaced rather than restarted.
    Recreate(InstanceId),
}

impl Action {
    pub fn instance(&self) -> &InstanceId {
        match self {
            Action::Create(id)
            | Action::Start(id)
            | Action::Stop(id)
            | Action::Remove(id)
            | Action::Recreate(id) => id,
        }
    }
}

/// What this node should be running, keyed by instance.
#[derive(Debug, Clone)]
pub struct Target {
    pub assignment: Assignment,
    pub spec: ContainerSpec,
    /// Fingerprint of the spec. A change means the container is replaced.
    pub revision: u64,
    /// How long to wait for a clean shutdown, from `stop_grace_period`.
    pub stop_grace: Duration,
}

/// Docker's own default is 10 seconds, which is short for a database.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(30);

/// Label carrying the spec revision, so drift is detectable without inspecting
/// every field docker may have normalised.
pub const LABEL_REVISION: &str = "clouduler.revision";

/// Work out what to do, given what the cluster wants and what docker has.
///
/// `fenced` short circuits everything: a node that has lost quorum stops what
/// it runs and starts nothing, because the rest of the cluster is about to hand
/// its work to somebody else.
pub fn diff(targets: &[Target], actual: &[ContainerState], fenced: bool) -> Vec<Action> {
    let running: BTreeMap<InstanceId, &ContainerState> = actual
        .iter()
        .filter_map(|c| {
            let id = c.labels.get(LABEL_INSTANCE)?.parse::<InstanceId>().ok()?;
            Some((id, c))
        })
        .collect();

    if fenced {
        return running
            .iter()
            .filter(|(_, c)| c.running)
            .map(|(id, _)| Action::Stop(id.clone()))
            .collect();
    }

    let mut actions = Vec::new();
    let mut wanted = BTreeSet::new();

    for target in targets {
        let id = &target.assignment.instance;
        wanted.insert(id.clone());

        let Some(current) = running.get(id) else {
            // Nothing here yet. A stopped assignment needs no container at all.
            if target.assignment.desired == DesiredState::Running {
                actions.push(Action::Create(id.clone()));
            }
            continue;
        };

        // A changed spec cannot be patched onto a live container, so replace it.
        //
        // Only for one that should be running: recreating ends in `start`, and
        // an instance is stopped either because someone asked for that or
        // because it is migrating away, and neither wants it back up. It keeps
        // its stale container until it is wanted again, and is recreated then.
        let current_revision = current
            .labels
            .get(LABEL_REVISION)
            .and_then(|r| r.parse::<u64>().ok());
        if current_revision != Some(target.revision)
            && target.assignment.desired == DesiredState::Running
        {
            actions.push(Action::Recreate(id.clone()));
            continue;
        }

        match (target.assignment.desired, current.running) {
            (DesiredState::Running, false) => actions.push(Action::Start(id.clone())),
            (DesiredState::Stopped, true) => actions.push(Action::Stop(id.clone())),
            _ => {}
        }
    }

    // Anything managed by clouduler that this node is no longer assigned.
    for id in running.keys() {
        if !wanted.contains(id) {
            actions.push(Action::Remove(id.clone()));
        }
    }

    actions
}

/// Carry out a diff against the local docker daemon.
pub struct Reconciler {
    docker: Docker,
    pulling: PullTracker,
}

impl Reconciler {
    pub fn new(docker: Docker, pulling: PullTracker) -> Self {
        Self { docker, pulling }
    }

    /// Apply the actions. One failure does not abort the rest: a single broken
    /// container must not stop the node reconciling everything else.
    pub async fn apply(&self, actions: &[Action], targets: &[Target], actual: &[ContainerState]) -> Result<()> {
        let specs: BTreeMap<&InstanceId, &Target> = targets
            .iter()
            .map(|t| (&t.assignment.instance, t))
            .collect();

        for action in actions {
            let id = action.instance();
            if let Err(e) = self.apply_one(action, specs.get(id).copied()).await {
                warn!(instance = %id, error = %e, "reconcile step failed");
            }
        }

        self.reconcile_egress_attachments(targets, actual).await;
        Ok(())
    }

    /// Catches a container that should be attached to an egress network but
    /// isn't yet — almost always one that was already running before the
    /// network existed to attach it to (`crates/clouduler/src/egress.rs` can
    /// bring a new `egress`/`wireguard` network up without a restart, but
    /// that alone does nothing for a container already running without it).
    /// `Action::Create`/`Action::Recreate` above are the only other place
    /// this attachment happens, and neither fires again once a container's
    /// revision is up to date, so nothing else would ever retry it.
    ///
    /// Runs unconditionally every tick, for every running target with an
    /// egress network, rather than trying to track "did this one already get
    /// fixed": both `connect_network` and `set_default_route` are already
    /// idempotent (a repeat call is a cheap no-op), and that's simpler than
    /// keeping that answer correct across reconnects, migrations and
    /// restarts.
    async fn reconcile_egress_attachments(&self, targets: &[Target], actual: &[ContainerState]) {
        let running: BTreeMap<InstanceId, &ContainerState> = actual
            .iter()
            .filter_map(|c| {
                let id = c.labels.get(LABEL_INSTANCE)?.parse::<InstanceId>().ok()?;
                Some((id, c))
            })
            .collect();

        for target in targets {
            if target.assignment.desired != DesiredState::Running {
                continue;
            }
            let Some(egress) = &target.spec.egress_network else {
                continue;
            };
            let Some(state) = running.get(&target.assignment.instance) else {
                continue;
            };
            if !state.running || state.networks.contains(egress) {
                continue;
            }

            let id = &target.assignment.instance;
            info!(instance = %id, network = %egress, "attaching a running container to its egress network");
            if let Err(e) = self.docker.connect_network(&state.id, egress, None).await {
                warn!(instance = %id, network = %egress, error = %e, "cannot attach the egress network");
                continue;
            }
            let name = id.container_name();
            if let Err(e) = self.docker.set_default_route(&name, egress).await {
                warn!(instance = %id, network = %egress, error = %e, "cannot route this container's traffic out the egress network");
            }
        }
    }

    /// Pull the image unless it is already here. Creating a container against
    /// an image the node has never seen fails, and on a fresh node that is
    /// every image.
    async fn ensure_image(&self, id: &InstanceId, image: &str) -> Result<()> {
        if self.docker.resolve_digest(image).await?.is_some() {
            return Ok(());
        }
        info!(%image, "pulling");
        self.pulling.write().await.insert(id.clone());
        let result = self.docker.pull(image).await;
        self.pulling.write().await.remove(id);
        result?;
        Ok(())
    }

    async fn apply_one(&self, action: &Action, target: Option<&Target>) -> Result<()> {
        let name = action.instance().container_name();

        match action {
            Action::Create(id) => {
                let Some(target) = target else {
                    warn!(instance = %id, "no spec to create from");
                    return Ok(());
                };
                self.ensure_image(id, &target.spec.image).await?;
                info!(instance = %id, "creating");
                let container_id = self.docker.create(&target.spec).await?;
                if let Some(egress) = &target.spec.egress_network {
                    self.docker.connect_network(&container_id, egress, None).await?;
                }
                self.docker.start(&name).await?;
                if let Some(egress) = &target.spec.egress_network {
                    self.docker.set_default_route(&name, egress).await?;
                }
            }
            Action::Start(id) => {
                info!(instance = %id, "starting");
                if let Err(e) = self.docker.start(&name).await {
                    if let (true, Some(target)) = (is_stale_network_error(&e), target) {
                        warn!(instance = %id, error = %e, "container's network no longer exists, recreating");
                        return self.replace(id, target, &name).await;
                    }
                    return Err(e.into());
                }
                // A fresh start means a fresh pid, so a route pinned to the
                // old one is gone too: has to be redone every time, not just
                // on create.
                if let Some(egress) = target.and_then(|t| t.spec.egress_network.as_ref()) {
                    self.docker.set_default_route(&name, egress).await?;
                }
            }
            Action::Stop(id) => {
                info!(instance = %id, "stopping");
                self.docker.stop(&name, stop_timeout(target)).await?;
            }
            Action::Remove(id) => {
                info!(instance = %id, "removing");
                self.docker.stop(&name, stop_timeout(target)).await.ok();
                self.docker.remove(&name, true).await?;
            }
            Action::Recreate(id) => {
                let Some(target) = target else {
                    warn!(instance = %id, "no spec to recreate from");
                    return Ok(());
                };
                info!(instance = %id, "recreating after a spec change");
                self.replace(id, target, &name).await?;
            }
        }
        Ok(())
    }

    /// Stops (if running), force-removes, then creates and starts a fresh
    /// container from `target`'s current spec. Shared by `Action::Recreate`
    /// (the spec itself changed) and `Action::Start`'s stale-network
    /// recovery: both need the old container gone before docker will let a
    /// new one attach cleanly, and `create` picks up whatever the network
    /// currently is rather than whatever it was when this container was
    /// last built.
    async fn replace(&self, id: &InstanceId, target: &Target, name: &str) -> Result<()> {
        self.ensure_image(id, &target.spec.image).await?;
        self.docker.stop(name, stop_timeout(Some(target))).await.ok();
        self.docker.remove(name, true).await.ok();
        let container_id = self.docker.create(&target.spec).await?;
        if let Some(egress) = &target.spec.egress_network {
            self.docker.connect_network(&container_id, egress, None).await?;
        }
        self.docker.start(name).await?;
        if let Some(egress) = &target.spec.egress_network {
            self.docker.set_default_route(name, egress).await?;
        }
        Ok(())
    }
}

/// Whether a docker error is the container's network having been torn down
/// and rebuilt under it — the overlay bridge still exists by the same name,
/// but with a fresh docker network id, because it was removed and remade
/// (e.g. `overlay.rs` rebuilding it fresh after this node's daemon
/// restarted) while this container was still attached to the old one.
/// Docker refuses to start it and otherwise nothing ever notices: the
/// container's spec hasn't changed, so `diff()` keeps deciding `Start`
/// forever instead of `Recreate`, and the same failure repeats every sweep
/// until something forces a fresh container. Dockerd has no distinct error
/// code for this (it comes back as a generic 500), only this message —
/// same tradeoff `DockerError::is_not_found` already makes for 404s.
fn is_stale_network_error(e: &DockerError) -> bool {
    e.to_string().contains("Could not attach to network")
}

fn stop_timeout(target: Option<&Target>) -> Duration {
    target.map(|t| t.stop_grace).unwrap_or(DEFAULT_STOP_GRACE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_docker::{RunState, LABEL_MANAGED, MANAGED_VALUE};
    use cl_raft::state::Assignment;
    use std::collections::HashMap;

    #[test]
    fn recognises_the_stale_network_error_dockerd_reports_on_start() {
        let e = DockerError::NotFound(
            "failed to set up container networking: Could not attach to network \
             032b9da4a7c728461a048d7b4da2011c2527a8ec9647b994876b041da23b408b: \
             rpc error: code = NotFound desc = network ... not found"
                .to_string(),
        );
        assert!(is_stale_network_error(&e));
    }

    #[test]
    fn an_unrelated_docker_error_is_not_mistaken_for_a_stale_network() {
        let e = DockerError::NotFound("no such image".to_string());
        assert!(!is_stale_network_error(&e));
    }

    fn target(service: &str, index: u32, desired: DesiredState, revision: u64) -> Target {
        let instance = InstanceId::new("site", service, index);
        Target {
            assignment: Assignment {
                instance: instance.clone(),
                node: "n1".into(),
                desired,
                image_digest: None,
                migration: None,
            },
            spec: ContainerSpec {
                name: instance.container_name(),
                image: "nginx".into(),
                stack: "site".into(),
                service: service.into(),
                instance: instance.to_string(),
                ..Default::default()
            },
            revision,
            stop_grace: DEFAULT_STOP_GRACE,
        }
    }

    fn container(service: &str, index: u32, running: bool, revision: u64) -> ContainerState {
        let instance = InstanceId::new("site", service, index);
        let mut labels = HashMap::new();
        labels.insert(LABEL_INSTANCE.to_string(), instance.to_string());
        labels.insert(LABEL_MANAGED.to_string(), MANAGED_VALUE.to_string());
        labels.insert(LABEL_REVISION.to_string(), revision.to_string());

        ContainerState {
            id: format!("id-{service}-{index}"),
            image: "nginx".into(),
            image_digest: None,
            running,
            status: if running { RunState::Running } else { RunState::Created },
            health: None,
            exit_code: None,
            labels,
            started_at: None,
            networks: BTreeSet::new(),
        }
    }

    #[test]
    fn a_missing_container_is_created() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actions = diff(&targets, &[], false);
        assert_eq!(actions, vec![Action::Create(InstanceId::new("site", "web", 0))]);
    }

    #[test]
    fn a_stopped_assignment_creates_nothing() {
        let targets = vec![target("web", 0, DesiredState::Stopped, 1)];
        assert!(diff(&targets, &[], false).is_empty());
    }

    #[test]
    fn a_stopped_container_that_should_run_is_started() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actual = vec![container("web", 0, false, 1)];
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Start(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_running_container_that_should_not_run_is_stopped() {
        let targets = vec![target("web", 0, DesiredState::Stopped, 1)];
        let actual = vec![container("web", 0, true, 1)];
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Stop(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_correct_container_needs_nothing() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actual = vec![container("web", 0, true, 1)];
        assert!(diff(&targets, &actual, false).is_empty());
    }

    #[test]
    fn a_changed_spec_forces_a_recreate() {
        let targets = vec![target("web", 0, DesiredState::Running, 2)];
        let actual = vec![container("web", 0, true, 1)];
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Recreate(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_stale_spec_does_not_restart_a_stopped_instance() {
        // An instance migrating away is recorded as stopped. Recreating ends
        // in `start`, which would put it back up on the node it is leaving.
        let targets = vec![target("web", 0, DesiredState::Stopped, 2)];
        let actual = vec![container("web", 0, true, 1)];
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Stop(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_container_without_a_revision_label_is_recreated() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let mut actual = vec![container("web", 0, true, 1)];
        actual[0].labels.remove(LABEL_REVISION);
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Recreate(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn an_unassigned_container_is_removed() {
        let actual = vec![container("old", 0, true, 1)];
        assert_eq!(
            diff(&[], &actual, false),
            vec![Action::Remove(InstanceId::new("site", "old", 0))]
        );
    }

    #[test]
    fn fencing_stops_everything_and_starts_nothing() {
        let targets = vec![
            target("web", 0, DesiredState::Running, 1),
            target("db", 0, DesiredState::Running, 1),
        ];
        let actual = vec![container("web", 0, true, 1), container("db", 0, false, 1)];

        let actions = diff(&targets, &actual, true);
        // Only the running one needs stopping, and nothing gets created.
        assert_eq!(actions, vec![Action::Stop(InstanceId::new("site", "web", 0))]);
    }

    #[test]
    fn containers_with_unparseable_labels_are_ignored() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let mut actual = vec![container("web", 0, true, 1)];
        actual[0]
            .labels
            .insert(LABEL_INSTANCE.to_string(), "nonsense".to_string());

        // The bad container is invisible to us, so the real one gets created.
        assert_eq!(
            diff(&targets, &actual, false),
            vec![Action::Create(InstanceId::new("site", "web", 0))]
        );
    }
}
