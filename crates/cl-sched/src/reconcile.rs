//! The per-node reconcile loop: make docker match what the cluster decided.
//!
//! The diff is a pure function so every branch is testable without a docker
//! daemon. Applying it is the only part that touches the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use cl_docker::{ContainerSpec, ContainerState, Docker, DockerError, LABEL_INSTANCE};
use cl_raft::ids::InstanceId;
use cl_raft::state::{Assignment, DesiredState, UpdatePhase};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::error::Result;
use crate::progress::UpdateReporter;
use crate::registries::Registries;

/// Instances whose image this node's reconciler is currently pulling. Shared
/// with the api so `GET /api/instances` can report "pulling" instead of
/// guessing from the absence of a container — nothing in a docker inspect
/// distinguishes "about to be created" from "still fetching the image".
///
/// A count per instance, not a set: the api's update path and this reconciler
/// can both be pulling for one instance at once, and with a plain set the
/// first to finish would clear the mark while the other is still downloading.
/// Go through `pull_started` / `pull_finished`.
pub type PullTracker = Arc<RwLock<BTreeMap<InstanceId, usize>>>;

/// Marks one more pull in flight for `id`.
pub async fn pull_started(tracker: &PullTracker, id: &InstanceId) {
    *tracker.write().await.entry(id.clone()).or_default() += 1;
}

/// Marks one pull for `id` done; the instance stops reading as pulling only
/// when the last one is.
pub async fn pull_finished(tracker: &PullTracker, id: &InstanceId) {
    let mut pulls = tracker.write().await;
    if let Some(count) = pulls.get_mut(id) {
        *count -= 1;
        if *count == 0 {
            pulls.remove(id);
        }
    }
}

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
    /// Where redeploy and update steps get recorded. `None` records nothing.
    progress: Option<UpdateReporter>,
    /// This node's updates in flight (see `cl_raft::InstanceUpdate`), by
    /// phase, so that a `Create`/`Start` finishing one reports how it went.
    /// Any other `Create` is a plain deploy and stays out of the record.
    updates: BTreeMap<InstanceId, UpdatePhase>,
    /// Registry logins this pass may pull with. Rebuilt per pass rather than
    /// held for the reconciler's life so a login added, rotated or removed
    /// through the api takes effect on the next sweep instead of at the next
    /// daemon restart — and so no password outlives the pass that needed it.
    registries: Registries,
}

impl Reconciler {
    pub fn new(docker: Docker, pulling: PullTracker) -> Self {
        Self {
            docker,
            pulling,
            progress: None,
            updates: BTreeMap::new(),
            registries: Registries::default(),
        }
    }

    /// Record every redeploy's steps, and the end of `updates`, through
    /// `progress`.
    pub fn with_progress(
        mut self,
        progress: UpdateReporter,
        updates: BTreeMap<InstanceId, UpdatePhase>,
    ) -> Self {
        self.progress = Some(progress);
        self.updates = updates;
        self
    }

    async fn report(&self, id: &InstanceId, phase: UpdatePhase, error: Option<String>) {
        if let Some(progress) = &self.progress {
            progress.report(id, phase, error).await;
        }
    }

    /// Pull private images with these logins. Without them a private image is
    /// simply unpullable: docker answers an anonymous manifest request for
    /// one with the same 401 it gives for a repository that does not exist.
    pub fn with_registries(mut self, registries: Registries) -> Self {
        self.registries = registries;
        self
    }

    /// Apply the actions. One failure does not abort the rest: a single broken
    /// container must not stop the node reconciling everything else.
    pub async fn apply(
        &self,
        actions: &[Action],
        targets: &[Target],
        actual: &[ContainerState],
    ) -> Result<()> {
        let specs: BTreeMap<&InstanceId, &Target> = targets
            .iter()
            .map(|t| (&t.assignment.instance, t))
            .collect();

        for action in actions {
            let id = action.instance();
            // A create or start finishing an update someone else began.
            // A recreate records its own steps instead, see `replace`.
            let tracked = finishes_update(action, self.updates.get(id).copied());
            let result = self.apply_one(action, specs.get(id).copied()).await;
            if let Err(e) = &result {
                warn!(instance = %id, error = %e, "reconcile step failed");
            }
            if tracked {
                // A container with a healthcheck is only started, not yet
                // known good: the sweep promotes it to `Started` once docker
                // reports it healthy (`health_gate`), or fails the update.
                let has_healthcheck = specs.get(id).is_some_and(|t| t.spec.healthcheck.is_some());
                match result {
                    Ok(()) if has_healthcheck => self.report(id, UpdatePhase::Starting, None).await,
                    Ok(()) => self.report(id, UpdatePhase::Started, None).await,
                    Err(e) => {
                        self.report(id, UpdatePhase::Failed, Some(e.to_string()))
                            .await
                    }
                }
            }
        }

        self.reconcile_egress_attachments(targets, actual).await;
        Ok(())
    }

    /// Catches a container that should be attached to an egress network but
    /// isn't yet — almost always one that was already running before the
    /// network existed to attach it to (`crates/cl-daemon/src/egress.rs` can
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
            if !state.running {
                continue;
            }
            if state.networks.contains(egress) {
                // Already attached, but the node set or this service's
                // networks may have changed since the routes were pinned, and
                // a container started before this existed never had them.
                // Idempotent (`ip route replace`).
                let name = target.assignment.instance.container_name();
                if let Err(e) = self.pin_proxy_routes(&name, &target.spec).await {
                    warn!(instance = %target.assignment.instance, error = %e, "cannot keep the proxy addresses on the overlay");
                }
                continue;
            }

            let id = &target.assignment.instance;
            info!(instance = %id, network = %egress, "attaching a running container to its egress network");
            if let Err(e) = self.docker.connect_network(&state.id, egress, None).await {
                warn!(instance = %id, network = %egress, error = %e, "cannot attach the egress network");
                continue;
            }
            let name = id.container_name();
            if let Err(e) = self.route_egress(&name, &target.spec).await {
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
        pull_started(&self.pulling, id).await;
        let result = self
            .docker
            .pull_as(image, self.registries.for_image(image))
            .await;
        pull_finished(&self.pulling, id).await;
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
                    self.docker
                        .connect_network(&container_id, egress, None)
                        .await?;
                }
                self.docker.start(&name).await?;
                self.route_egress(&name, &target.spec).await?;
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
                if let Some(target) = target {
                    self.route_egress(&name, &target.spec).await?;
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
    ///
    /// Every step lands in the replicated update record, so a redeploy shows
    /// real progress from any node's api.
    async fn replace(&self, id: &InstanceId, target: &Target, name: &str) -> Result<()> {
        let result = self.replace_steps(id, target, name).await;
        match &result {
            Ok(()) => self.report(id, UpdatePhase::Started, None).await,
            Err(e) => {
                self.report(id, UpdatePhase::Failed, Some(e.to_string()))
                    .await
            }
        }
        result
    }

    async fn replace_steps(&self, id: &InstanceId, target: &Target, name: &str) -> Result<()> {
        self.report(id, UpdatePhase::Pulling, None).await;
        self.ensure_image(id, &target.spec.image).await?;

        self.report(id, UpdatePhase::Stopping, None).await;
        self.docker
            .stop(name, stop_timeout(Some(target)))
            .await
            .ok();
        self.docker.remove(name, true).await.ok();

        self.report(id, UpdatePhase::Starting, None).await;
        let container_id = self.docker.create(&target.spec).await?;
        if let Some(egress) = &target.spec.egress_network {
            self.docker
                .connect_network(&container_id, egress, None)
                .await?;
        }
        self.docker.start(name).await?;
        self.route_egress(name, &target.spec).await
    }

    /// Points a started container's default route at its egress network, if
    /// it has one, and keeps the proxy's node addresses on the overlay.
    async fn route_egress(&self, name: &str, spec: &ContainerSpec) -> Result<()> {
        let Some(egress) = &spec.egress_network else {
            return Ok(());
        };
        self.docker.set_default_route(name, egress).await?;
        self.pin_proxy_routes(name, spec).await
    }

    /// Without this, a route published on a network the service is a member
    /// of resolves to a node address that is not on the overlay, follows the
    /// egress default route and dies in the bridge's kill-switch.
    async fn pin_proxy_routes(&self, name: &str, spec: &ContainerSpec) -> Result<()> {
        let Some(gateway) = spec.pinned_gateway else {
            return Ok(());
        };
        if spec.pinned_routes.is_empty() {
            return Ok(());
        }
        self.docker
            .pin_routes(name, gateway, &spec.pinned_routes)
            .await?;
        Ok(())
    }
}

/// How long a container with a healthcheck gets to turn healthy, counted from
/// the update reaching `Starting`, before the update is failed.
pub const HEALTH_TIMEOUT_SECS: i64 = 5 * 60;

/// What a freshly started container's health says about the update that
/// started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthGate {
    /// No healthcheck, or it passed: the update succeeded.
    Ready,
    /// The healthcheck has not passed yet.
    Wait,
    /// It never will: docker marked the container unhealthy, or it is still
    /// not healthy after `HEALTH_TIMEOUT_SECS`.
    Failed(&'static str),
}

/// Decides an update's fate from its new container's health and how long it
/// has been `Starting`.
pub fn health_gate(health: Option<cl_docker::HealthState>, waited_secs: i64) -> HealthGate {
    use cl_docker::HealthState as H;
    match health {
        Option::None | Some(H::None) | Some(H::Healthy) => HealthGate::Ready,
        Some(H::Unhealthy) => HealthGate::Failed("the new container is unhealthy"),
        Some(H::Starting) if waited_secs >= HEALTH_TIMEOUT_SECS => {
            HealthGate::Failed("the new container did not become healthy in time")
        }
        Some(H::Starting) => HealthGate::Wait,
    }
}

/// Whether applying `action` brings up the new container of an update that
/// is at `phase`. A missing container mid-update is the one the update took
/// away (queued is excluded: that update has not touched anything yet). A
/// stopped one only counts once the old container is known to be gone,
/// otherwise this would be restarting the very container being replaced.
fn finishes_update(action: &Action, phase: Option<UpdatePhase>) -> bool {
    use UpdatePhase::*;
    matches!(
        (action, phase),
        (
            Action::Create(_),
            Some(Pulling | Stopping | Starting | Failed)
        ) | (Action::Start(_), Some(Starting | Failed))
    )
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
    #[tokio::test]
    async fn concurrent_pulls_of_one_instance_keep_it_marked_until_the_last_finishes() {
        let tracker = PullTracker::default();
        let id = InstanceId::new("s", "web", 0);
        pull_started(&tracker, &id).await;
        pull_started(&tracker, &id).await;
        pull_finished(&tracker, &id).await;
        assert!(tracker.read().await.contains_key(&id));
        pull_finished(&tracker, &id).await;
        assert!(tracker.read().await.is_empty());
        // A stray extra finish is harmless.
        pull_finished(&tracker, &id).await;
    }

    #[test]
    fn health_gate_waits_then_fails_but_passes_without_a_check() {
        use cl_docker::HealthState as H;
        assert_eq!(health_gate(None, 0), HealthGate::Ready);
        assert_eq!(health_gate(Some(H::None), 0), HealthGate::Ready);
        assert_eq!(health_gate(Some(H::Healthy), 0), HealthGate::Ready);
        assert_eq!(health_gate(Some(H::Starting), 10), HealthGate::Wait);
        assert!(matches!(
            health_gate(Some(H::Starting), HEALTH_TIMEOUT_SECS),
            HealthGate::Failed(_)
        ));
        assert!(matches!(
            health_gate(Some(H::Unhealthy), 1),
            HealthGate::Failed(_)
        ));
    }

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
            status: if running {
                RunState::Running
            } else {
                RunState::Created
            },
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
        assert_eq!(
            actions,
            vec![Action::Create(InstanceId::new("site", "web", 0))]
        );
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
        assert_eq!(
            actions,
            vec![Action::Stop(InstanceId::new("site", "web", 0))]
        );
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
