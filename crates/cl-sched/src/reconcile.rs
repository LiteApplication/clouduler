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
use cl_raft::FenceState;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

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

/// While emergency mode is on, drop every action that would touch a container
/// outside the emergency stack. Other stacks are not scheduled, but whatever
/// of theirs is already running on this node is left exactly as it is: not
/// stopped, not removed, not recreated.
pub fn restrict_to_emergency_stacks(
    actions: &mut Vec<Action>,
    emergency_stacks: &BTreeSet<String>,
) {
    if !emergency_stacks.is_empty() {
        actions.retain(|a| emergency_stacks.contains(&a.instance().stack));
    }
}

/// `x-clouduler.priority` per stack name (lower is more important). A stack
/// missing from the map counts as 0.
pub type Priorities = BTreeMap<String, i32>;

fn priority_of(priorities: &Priorities, action: &Action) -> i32 {
    priorities
        .get(&action.instance().stack)
        .copied()
        .unwrap_or(0)
}

/// Puts the most important stacks first, keeping the order within a priority.
pub fn sort_by_priority(actions: &mut [Action], priorities: &Priorities) {
    actions.sort_by_key(|a| priority_of(priorities, a));
}

/// A fence's stops grouped by priority, least important group first, so the
/// most important stacks are the last to go down.
fn fence_stages(actions: &[Action], priorities: &Priorities) -> Vec<Vec<Action>> {
    let mut stages: BTreeMap<std::cmp::Reverse<i32>, Vec<Action>> = BTreeMap::new();
    for action in actions.iter().filter(|a| matches!(a, Action::Stop(_))) {
        stages
            .entry(std::cmp::Reverse(priority_of(priorities, action)))
            .or_default()
            .push(action.clone());
    }
    stages.into_values().collect()
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

/// Run a fence's stops all at once, skipping any not yet begun once
/// `still_fenced` says the fence is gone. Other actions in `actions` are left
/// alone: a fence batch is only ever stops.
///
/// `stop` is the real thing in `Reconciler::apply`, and a stand-in in tests.
async fn stop_while_fenced<'a, F, Fut>(
    actions: &'a [Action],
    still_fenced: &(dyn Fn() -> bool + Sync),
    stop: F,
) where
    F: Fn(&'a Action) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    futures::future::join_all(actions.iter().filter(|a| matches!(a, Action::Stop(_))).map(
        |action| {
            let stop = &stop;
            async move {
                if !still_fenced() {
                    debug!(instance = %action.instance(), "fence released, not stopping");
                    return;
                }
                stop(action).await;
            }
        },
    ))
    .await;
}

/// Allowed on top of a container's stop grace before a fence's stop is
/// abandoned, for docker itself to answer.
const FENCE_STOP_SLACK: Duration = Duration::from_secs(10);

/// The built-in stop grace; the configured one is `failover.stop_grace`.
pub const DEFAULT_STOP_GRACE: Duration = cl_config::DEFAULT_STOP_GRACE;

/// Label carrying the spec revision, so drift is detectable without inspecting
/// every field docker may have normalised.
pub const LABEL_REVISION: &str = "clouduler.revision";

/// Label carrying the revision of every cluster secret the container was
/// created with, as `name=revision` pairs joined by commas (secret names
/// cannot contain `=` or `,`). Deliberately not part of `LABEL_REVISION`:
/// `diff` recreates a container whenever that changes, and overwriting a
/// secret must not restart anything unless the operator asks for it. Holds
/// names and numbers only, never a value.
pub const LABEL_SECRETS: &str = "clouduler.secrets";

/// `name=revision` pairs for `LABEL_SECRETS`, sorted by name.
pub fn encode_secret_revisions(revisions: &BTreeMap<String, u64>) -> String {
    revisions
        .iter()
        .map(|(name, revision)| format!("{name}={revision}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Inverse of `encode_secret_revisions`. Malformed pairs are skipped.
pub fn decode_secret_revisions(label: &str) -> BTreeMap<String, u64> {
    label
        .split(',')
        .filter_map(|pair| {
            let (name, revision) = pair.split_once('=')?;
            Some((name.to_string(), revision.parse().ok()?))
        })
        .collect()
}

/// Work out what to do, given what the cluster wants and what docker has.
///
/// `Closed` short circuits everything: a node that has lost quorum stops what
/// it runs and starts nothing, because the rest of the cluster is about to hand
/// its work to somebody else. `Hold` changes nothing at all, since this node
/// cannot yet tell whether its view of what it owns is still current.
pub fn diff(targets: &[Target], actual: &[ContainerState], gate: FenceState) -> Vec<Action> {
    let running: BTreeMap<InstanceId, &ContainerState> = actual
        .iter()
        .filter_map(|c| {
            let id = c.labels.get(LABEL_INSTANCE)?.parse::<InstanceId>().ok()?;
            Some((id, c))
        })
        .collect();

    if gate == FenceState::Hold {
        return Vec::new();
    }

    if gate == FenceState::Closed {
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
    /// Stop grace for a container with no target to read one from.
    stop_grace: Duration,
}

impl Reconciler {
    pub fn new(docker: Docker, pulling: PullTracker) -> Self {
        Self {
            docker,
            pulling,
            progress: None,
            updates: BTreeMap::new(),
            registries: Registries::default(),
            stop_grace: DEFAULT_STOP_GRACE,
        }
    }

    /// The cluster's `failover.stop_grace`, for stopping a container that has
    /// no target (one being removed).
    pub fn with_stop_grace(mut self, stop_grace: Duration) -> Self {
        self.stop_grace = stop_grace;
        self
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
    ///
    /// `fencing` is set when `actions` is a fence's batch of stops, with a
    /// check for whether the fence still stands. Those run concurrently, each
    /// with its own grace period, so the fence takes as long as the slowest
    /// container rather than all of them added up, and a release part way
    /// through drops the stops not yet begun.
    pub async fn apply(
        &self,
        actions: &[Action],
        targets: &[Target],
        actual: &[ContainerState],
        fencing: Option<&(dyn Fn() -> bool + Sync)>,
        priorities: &Priorities,
    ) -> Result<()> {
        let specs: BTreeMap<&InstanceId, &Target> = targets
            .iter()
            .map(|t| (&t.assignment.instance, t))
            .collect();

        if let Some(still_fenced) = fencing {
            // One stage per priority, so a stack that matters more outlives
            // the ones it depends on being gone.
            for stage in fence_stages(actions, priorities) {
                stop_while_fenced(&stage, still_fenced, |action| async {
                    let id = action.instance();
                    let target = specs.get(id).copied();
                    // One hung docker call must not hold the whole pass, and with
                    // it the start of everything else once the fence lifts.
                    let deadline = stop_timeout(target, self.stop_grace) + FENCE_STOP_SLACK;
                    match tokio::time::timeout(deadline, self.apply_one(action, target)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => warn!(instance = %id, error = %e, "reconcile step failed"),
                        Err(_) => warn!(instance = %id, ?deadline, "stop timed out, moving on"),
                    }
                })
                .await;
            }
            self.reconcile_egress_attachments(targets, actual).await;
            self.reconcile_container_attachments(targets, actual).await;
            return Ok(());
        }

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
        self.reconcile_container_attachments(targets, actual).await;
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
                self.connect_container_networks(&container_id, &target.spec)
                    .await?;
                self.docker.start(&name).await?;
                self.route_egress(&name, &target.spec).await?;
                self.route_container_networks(&name, &target.spec).await?;
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
                    self.route_container_networks(&name, &target.spec).await?;
                }
            }
            Action::Stop(id) => {
                info!(instance = %id, "stopping");
                self.docker
                    .stop(&name, stop_timeout(target, self.stop_grace))
                    .await?;
            }
            Action::Remove(id) => {
                info!(instance = %id, "removing");
                self.docker
                    .stop(&name, stop_timeout(target, self.stop_grace))
                    .await
                    .ok();
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
            .stop(name, stop_timeout(Some(target), self.stop_grace))
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
        self.connect_container_networks(&container_id, &target.spec)
            .await?;
        self.docker.start(name).await?;
        self.route_egress(name, &target.spec).await?;
        self.route_container_networks(name, &target.spec).await
    }

    /// Attach a created (not yet started) container to every container-kind
    /// network it uses, with its pinned address and, for a router, the
    /// interface name its image expects.
    async fn connect_container_networks(
        &self,
        container: &str,
        spec: &ContainerSpec,
    ) -> Result<()> {
        for att in &spec.container_networks {
            // Already attached at create time.
            if spec.network.as_deref() == Some(att.docker_network.as_str()) {
                continue;
            }
            self.docker
                .connect_network_named(
                    container,
                    &att.docker_network,
                    Some(att.ip),
                    att.ifname.as_deref(),
                )
                .await?;
        }
        Ok(())
    }

    /// Put the routes of every container-kind attachment into the container's
    /// namespace (`ip route replace`, so safe to repeat: this is also what
    /// swaps a client's `prohibit` for the real route once the router
    /// exists), then keep the proxy addresses on the overlay when a
    /// `0.0.0.0/0` network took the default route.
    async fn route_container_networks(&self, name: &str, spec: &ContainerSpec) -> Result<()> {
        if spec.container_networks.is_empty() {
            return Ok(());
        }
        for att in &spec.container_networks {
            self.docker.apply_netns_routes(name, &att.routes).await?;
        }
        if spec.container_on_primary() && !spec.container_networks.iter().any(|a| a.default_route) {
            // Docker gave the segment's own gateway as the default route; a
            // client of a narrower range must have none but the router's.
            // Fails when there is none, which is fine.
            let _ = self
                .docker
                .apply_netns_routes(name, &[vec!["del".into(), "default".into()]])
                .await;
        }
        if spec.egress_network.is_none() {
            self.pin_proxy_routes(name, spec).await?;
        }
        Ok(())
    }

    /// Per-tick catch-up for container-kind networks, the same idea as
    /// `reconcile_egress_attachments`: a client started before its router
    /// existed gets the real route once it appears, a router that moved or a
    /// node set that changed gets its routes refreshed, and a container
    /// running without an attachment gets connected. Everything is idempotent.
    async fn reconcile_container_attachments(&self, targets: &[Target], actual: &[ContainerState]) {
        let running: BTreeMap<InstanceId, &ContainerState> = actual
            .iter()
            .filter_map(|c| {
                let id = c.labels.get(LABEL_INSTANCE)?.parse::<InstanceId>().ok()?;
                Some((id, c))
            })
            .collect();

        for target in targets {
            if target.assignment.desired != DesiredState::Running
                || target.spec.container_networks.is_empty()
            {
                continue;
            }
            let id = &target.assignment.instance;
            let Some(state) = running.get(id) else {
                continue;
            };
            if !state.running {
                continue;
            }
            let mut attached = true;
            for att in &target.spec.container_networks {
                if state.networks.contains(&att.docker_network) {
                    continue;
                }
                info!(instance = %id, network = %att.docker_network, "attaching a running container to its container network");
                if let Err(e) = self
                    .docker
                    .connect_network_named(
                        &state.id,
                        &att.docker_network,
                        Some(att.ip),
                        att.ifname.as_deref(),
                    )
                    .await
                {
                    warn!(instance = %id, network = %att.docker_network, error = %e, "cannot attach the container network");
                    attached = false;
                }
            }
            if !attached {
                continue;
            }
            let name = id.container_name();
            if let Err(e) = self.route_container_networks(&name, &target.spec).await {
                warn!(instance = %id, error = %e, "cannot apply the container network routes");
            }
        }
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

fn stop_timeout(target: Option<&Target>, fallback: Duration) -> Duration {
    target.map(|t| t.stop_grace).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    #[test]
    fn important_stacks_start_first_and_stop_last() {
        let id = |stack: &str| InstanceId::new(stack, "web", 0);
        let priorities: Priorities = [("db".into(), -1), ("app".into(), 5)].into();
        let mut actions = vec![
            Action::Create(id("app")),
            Action::Create(id("misc")),
            Action::Create(id("db")),
        ];
        sort_by_priority(&mut actions, &priorities);
        let order: Vec<_> = actions
            .iter()
            .map(|a| a.instance().stack.as_str())
            .collect();
        assert_eq!(order, ["db", "misc", "app"]);

        let stops = vec![
            Action::Stop(id("db")),
            Action::Stop(id("app")),
            Action::Stop(id("misc")),
            Action::Start(id("misc")),
        ];
        let stages: Vec<Vec<_>> = fence_stages(&stops, &priorities)
            .iter()
            .map(|s| s.iter().map(|a| a.instance().stack.clone()).collect())
            .collect();
        assert_eq!(stages, [vec!["app"], vec!["misc"], vec!["db"]]);
    }

    #[test]
    fn emergency_mode_only_acts_on_the_emergency_stacks_containers() {
        let mut actions = vec![
            Action::Remove(InstanceId::new("other", "db", 0)),
            Action::Stop(InstanceId::new("other", "web", 0)),
            Action::Recreate(InstanceId::new("site", "web", 0)),
            Action::Create(InstanceId::new("site", "db", 0)),
        ];
        restrict_to_emergency_stacks(&mut actions, &BTreeSet::new());
        assert_eq!(actions.len(), 4, "no emergency: nothing is filtered");
        restrict_to_emergency_stacks(&mut actions, &["site".to_string()].into());
        assert_eq!(actions.len(), 2);
        assert!(actions.iter().all(|a| a.instance().stack == "site"));
    }

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
            ..Default::default()
        }
    }

    #[test]
    fn a_missing_container_is_created() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actions = diff(&targets, &[], FenceState::Open);
        assert_eq!(
            actions,
            vec![Action::Create(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_stopped_assignment_creates_nothing() {
        let targets = vec![target("web", 0, DesiredState::Stopped, 1)];
        assert!(diff(&targets, &[], FenceState::Open).is_empty());
    }

    #[test]
    fn a_stopped_container_that_should_run_is_started() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actual = vec![container("web", 0, false, 1)];
        assert_eq!(
            diff(&targets, &actual, FenceState::Open),
            vec![Action::Start(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_running_container_that_should_not_run_is_stopped() {
        let targets = vec![target("web", 0, DesiredState::Stopped, 1)];
        let actual = vec![container("web", 0, true, 1)];
        assert_eq!(
            diff(&targets, &actual, FenceState::Open),
            vec![Action::Stop(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_correct_container_needs_nothing() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let actual = vec![container("web", 0, true, 1)];
        assert!(diff(&targets, &actual, FenceState::Open).is_empty());
    }

    #[test]
    fn a_changed_spec_forces_a_recreate() {
        let targets = vec![target("web", 0, DesiredState::Running, 2)];
        let actual = vec![container("web", 0, true, 1)];
        assert_eq!(
            diff(&targets, &actual, FenceState::Open),
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
            diff(&targets, &actual, FenceState::Open),
            vec![Action::Stop(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_container_without_a_revision_label_is_recreated() {
        let targets = vec![target("web", 0, DesiredState::Running, 1)];
        let mut actual = vec![container("web", 0, true, 1)];
        actual[0].labels.remove(LABEL_REVISION);
        assert_eq!(
            diff(&targets, &actual, FenceState::Open),
            vec![Action::Recreate(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn an_unassigned_container_is_removed() {
        let actual = vec![container("old", 0, true, 1)];
        assert_eq!(
            diff(&[], &actual, FenceState::Open),
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

        let actions = diff(&targets, &actual, FenceState::Closed);
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
            diff(&targets, &actual, FenceState::Open),
            vec![Action::Create(InstanceId::new("site", "web", 0))]
        );
    }

    #[test]
    fn a_hold_changes_nothing() {
        // A running container that is wanted, one that is not, a missing one
        // and a stale one: a fence on hold touches none of them.
        let targets = vec![
            target("web", 0, DesiredState::Running, 1),
            target("db", 0, DesiredState::Running, 1),
        ];
        let actual = vec![
            container("web", 0, true, 1),
            container("old", 0, true, 1),
            container("cache", 0, false, 1),
        ];
        assert!(diff(&targets, &actual, FenceState::Hold).is_empty());
    }

    #[tokio::test]
    async fn fence_stops_run_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let actions: Vec<Action> = (0..4)
            .map(|i| Action::Stop(InstanceId::new("site", "web", i)))
            .collect();
        let done = AtomicUsize::new(0);
        let started = std::time::Instant::now();

        stop_while_fenced(&actions, &|| true, |_| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            done.fetch_add(1, Ordering::Relaxed);
        })
        .await;

        assert_eq!(done.load(Ordering::Relaxed), 4);
        // Run one after another these would take 400ms.
        assert!(started.elapsed() < Duration::from_millis(300));
    }

    #[tokio::test]
    async fn fence_stops_not_yet_begun_are_dropped_once_released() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let actions: Vec<Action> = (0..3)
            .map(|i| Action::Stop(InstanceId::new("site", "web", i)))
            .collect();
        let checks = AtomicUsize::new(0);
        let stopped = AtomicUsize::new(0);

        // Released after the first stop has been allowed.
        stop_while_fenced(
            &actions,
            &|| checks.fetch_add(1, Ordering::Relaxed) < 1,
            |_| async {
                stopped.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await;

        assert_eq!(stopped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_released_fence_stops_nothing() {
        let actions = vec![Action::Stop(InstanceId::new("site", "web", 0))];
        stop_while_fenced(&actions, &|| false, |_| async {
            panic!("stopped under a released fence")
        })
        .await;
    }

    #[test]
    fn secret_revisions_round_trip_through_the_label() {
        let revisions = BTreeMap::from([("db".to_string(), 3), ("api.key".to_string(), 12)]);
        let label = encode_secret_revisions(&revisions);
        assert_eq!(label, "api.key=12,db=3");
        assert_eq!(decode_secret_revisions(&label), revisions);
        assert!(decode_secret_revisions("").is_empty());
        assert_eq!(
            decode_secret_revisions("db=3,junk,x=notanumber"),
            BTreeMap::from([("db".to_string(), 3)])
        );
    }
}
