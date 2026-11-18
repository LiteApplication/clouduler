//! The scheduler loop: two jobs on one timer. The leader decides placement and writes it
//! through raft; every node then makes its own docker match the decisions naming it, so a
//! node never waits on the leader to start what it was given.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cl_config::{ClusterConfig, NetworkKind, StackFile};
use cl_docker::Docker;
use cl_net::Allocations;
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::node::ClusterNode;
use cl_raft::state::{
    base64_encode, ClusterState, DesiredState, NodeState, Request, UpdatePhase,
    CRASH_MOVE_RETENTION_SECS,
};
use cl_raft::FenceState;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::materialize::Materializer;
use crate::place;
use crate::plan::{self, Desired};

use crate::progress::{unix_now, UpdateReporter};
use crate::reconcile::{self, Action, PullTracker, Reconciler, Target};
use crate::spec;

/// Sweep interval on top of reacting to state changes; catches drift like a hand-stopped container.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Restarts within `CRASH_LOOP_WINDOW` before this node asks the leader to move the instance.
/// Three catches a genuine loop but lets one transient failure recover.
const CRASH_LOOP_THRESHOLD: usize = 3;

/// How long a write-back waits for its own commit to show up in this node's
/// applied state before leaving the rest to the next sweep.
const CONFIG_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// The window `CRASH_LOOP_THRESHOLD` is counted over.
const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(120);

/// First in-place restart delay once an instance has crash-looped everywhere (see
/// `try_reschedule_away`), doubled per loop up to `CRASH_BACKOFF_MAX`; otherwise a container
/// that dies on start busy-loops docker calls, pulls and logs.
const CRASH_BACKOFF_BASE: Duration = Duration::from_secs(30);

/// Cap on the in-place restart delay.
const CRASH_BACKOFF_MAX: Duration = Duration::from_secs(10 * 60);

/// A backoff is forgotten once the instance stays up this long after it expired.
const CRASH_BACKOFF_FORGET_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long a finished update stays in the replicated record.
const FINISHED_UPDATE_TTL_SECS: i64 = 15 * 60;

/// How long an update may sit in one unfinished phase before it is presumed abandoned.
const STALLED_UPDATE_TTL_SECS: i64 = 60 * 60;

/// How long an update may sit `Queued`: a rolling batch queues everything first, but 15
/// minutes exceeds one instance's turn (`UPDATE_FINISH_TIMEOUT` in the api bounds earlier ones).
const QUEUED_UPDATE_TTL_SECS: i64 = 15 * 60;

pub struct Scheduler {
    node: String,
    cluster: Arc<ClusterNode>,
    /// A live view of the docker connection, so a late or restarted dockerd works without a
    /// daemon restart. `None` is a normal temporary state.
    docker: cl_docker::DockerHandle,
    materializer: Materializer,
    /// Internal DNS zone, written into every container's search list.
    zone: Option<String>,
    /// Boot config, a fallback until the replicated copy is available (`live_config`). Egress and
    /// public network sets are re-derived from the live config each pass.
    boot_config: ClusterConfig,
    /// Instances this node's reconciler is currently pulling the image for,
    /// shared with the api so it can report a "pulling" instance state.
    pulling: PullTracker,
    /// Recent `Action::Start` timestamps per instance; node-local, in memory, to notice a
    /// container this node keeps failing to keep running. See `note_restart_and_check_crash_loop`.
    restarts: Arc<RwLock<BTreeMap<InstanceId, VecDeque<Instant>>>>,
    /// In-place restart delays for instances that crash-looped on every node; node-local, losing
    /// it just restarts the backoff.
    backoffs: Arc<RwLock<BTreeMap<InstanceId, RestartBackoff>>>,
    /// Unix seconds this scheduler was built; an older in-flight update was driven by a dead process.
    booted_at: i64,
    /// Rung when the fence releases so a node regaining quorum starts containers now.
    wake: Arc<tokio::sync::Notify>,
    image_gc: crate::images::ImageGc,
}

impl Scheduler {
    pub fn new(
        node: String,
        cluster: Arc<ClusterNode>,
        docker: cl_docker::DockerHandle,
        materializer: Materializer,
        boot_config: ClusterConfig,
        pulling: PullTracker,
    ) -> Self {
        let image_gc = crate::images::ImageGc::new(materializer.data_dir());
        Self {
            node,
            cluster,
            docker,
            materializer,
            image_gc,
            zone: None,
            boot_config,
            pulling,
            restarts: Arc::new(RwLock::new(BTreeMap::new())),
            backoffs: Arc::new(RwLock::new(BTreeMap::new())),
            booted_at: unix_now(),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn with_wake(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.wake = wake;
        self
    }

    /// Set the internal DNS zone containers search, so `web.blog` resolves without the domain.
    pub fn with_zone(mut self, zone: impl Into<String>) -> Self {
        self.zone = Some(zone.into());
        self
    }

    /// The replicated cluster config, falling back to the boot config before a leader has
    /// published one or if it fails to parse.
    fn live_config(&self, state: &ClusterState) -> ClusterConfig {
        match &state.cluster_config {
            Some(entry) => ClusterConfig::parse(&entry.yaml).unwrap_or_else(|e| {
                warn!(error = %e, "replicated cluster config failed to parse, using the config this node booted with");
                self.boot_config.clone()
            }),
            None => self.boot_config.clone(),
        }
    }

    /// Run until cancelled. `fence` says whether this node lost quorum long enough to give up
    /// its containers; `None` (undetermined, e.g. first sweep after restart) or `Hold` makes a
    /// pass do nothing, since guessing "fenced" would stop every container only to restart them.
    pub async fn run(self, fence: impl Fn() -> Option<FenceState> + Send + Sync + 'static) {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = self.wake.notified() => {}
            }

            let Some(gate) = fence() else {
                continue;
            };
            if gate == FenceState::Hold {
                continue;
            }
            let state = self.cluster.state().await;

            if self.cluster.is_leader() && gate == FenceState::Open {
                if let Err(e) = self.schedule(&state).await {
                    warn!(error = %e, "scheduling pass failed");
                }
                self.expire_orphaned_updates(&state).await;
            }

            let still_fenced = || fence() == Some(FenceState::Closed);
            if let Err(e) = self.reconcile(&state, gate, &still_fenced).await {
                warn!(error = %e, "reconcile pass failed");
            }
        }
    }

    async fn schedule(&self, state: &ClusterState) -> crate::Result<()> {
        let live = self.live_config(state);
        let colocate_proxy_hosts = live.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
        let desired = desired_instances(
            state,
            &live.egress_networks(),
            &live.port_gated_networks(),
            colocate_proxy_hosts,
        );
        let desired = with_container_routers(desired, state, &live);

        let outcome = place::place(
            state,
            &desired,
            &live.served_drives(),
            &live.everywhere_networks(),
        );
        for unplaceable in &outcome.unplaceable {
            warn!(instance = %unplaceable.instance, reason = %unplaceable.reason, "cannot place instance");
        }

        for decision in outcome.decisions {
            // `submit` forwards to the leader, so a pass outliving this node's leadership would keep
            // writing decisions from a stale view.
            if !self.cluster.is_leader() {
                warn!("lost leadership mid-pass, abandoning the scheduling pass");
                return Ok(());
            }
            let drives = desired
                .iter()
                .find(|d| &d.id == decision.instance())
                .map(|d| d.drives.clone())
                .unwrap_or_default();

            let Some(request) = decision.clone().into_request_for(state, drives) else {
                continue;
            };
            let response = self.cluster.submit(request).await.map_err(Box::new)?;
            if let Some(error) = response.error {
                warn!(instance = %decision.instance(), %error, "placement rejected");
            } else {
                info!(instance = %decision.instance(), "placed");
            }
        }

        // Re-read: `state` predates the placements above, and `volume_requests` decides a volume's
        // primary from assignments. A stale copy lags one sweep behind and hands the primary back
        // and forth with placement forever, restarting containers each time.
        let state = &self.cluster.state().await;
        for request in volume_requests(state, &live) {
            if !self.cluster.is_leader() {
                warn!("lost leadership mid-pass, abandoning the scheduling pass");
                return Ok(());
            }
            let response = self
                .cluster
                .submit(request.clone())
                .await
                .map_err(Box::new)?;
            if let Some(error) = response.error {
                warn!(?request, %error, "registering a volume was rejected");
            } else {
                match &request {
                    Request::UpsertVolume { id, primary } => {
                        info!(%id, %primary, "registered volume")
                    }
                    Request::TouchVolume { id, primary } => {
                        info!(%id, %primary, "volume primary follows the instance that mounts it")
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn reconcile(
        &self,
        state: &ClusterState,
        gate: FenceState,
        still_fenced: &(dyn Fn() -> bool + Sync),
    ) -> crate::Result<()> {
        let fenced = gate == FenceState::Closed;
        // No docker yet is not an error: skip, the next pass retries once `docker` reconnects.
        let Some(docker) = self.docker.get().await else {
            return Ok(());
        };
        // Opened fresh each pass (see `Reconciler::registries`); with no cluster key, pulls anonymously.
        let registries = match self.materializer.key() {
            Some(key) => crate::registries::Registries::load(state, &key),
            None => crate::registries::Registries::default(),
        };
        let updates = state
            .instance_updates
            .iter()
            .filter(|(_, u)| u.node == self.node)
            .map(|(id, u)| (id.clone(), u.phase))
            .collect();
        let reconciler = Reconciler::new(docker.clone(), self.pulling.clone())
            .with_registries(registries)
            .with_stop_grace(self.live_config(state).failover.stop_grace)
            .with_progress(
                UpdateReporter::new(self.cluster.clone(), self.node.clone()),
                updates,
            );

        let (targets, config_edits, refused) = self.targets(state)?;
        self.report_refused(state, &refused).await;
        let actual = docker.list_managed().await?;
        let mut actions = reconcile::diff(&targets, &actual, gate);
        // A standalone action is replacing this instance's container: leave it alone until done.
        let holds = docker.action_holds().await?;
        if !holds.is_empty() {
            actions.retain(|a| !holds.contains(&a.instance().to_string()));
        }
        reconcile::restrict_to_emergency_stacks(&mut actions, &state.emergency_stacks);

        let priorities = stack_priorities(state);
        reconcile::sort_by_priority(&mut actions, &priorities);

        // `depends_on`: hold back only the first `Create` until dependencies are up, so an existing
        // container is never stopped by a dependency going down.
        actions.retain(|action| {
            let Action::Create(id) = action else {
                return true;
            };
            match crate::deps::unmet_dependency(state, id) {
                Some(dep) => {
                    debug!(instance = %id, %dep, "waiting for a dependency before creating");
                    false
                }
                None => true,
            }
        });

        // A crash-looped-everywhere instance restarts in place on a growing delay. Dropped before
        // the crash-loop bookkeeping so a deferred start is not counted.
        {
            let now = Instant::now();
            let keep: BTreeSet<&InstanceId> =
                targets.iter().map(|t| &t.assignment.instance).collect();
            let mut backoffs = self.backoffs.write().await;
            backoffs.retain(|id, b| keep.contains(id) && !b.is_forgettable(now));
            actions.retain(|action| match action {
                Action::Start(id) => backoffs.get(id).is_none_or(|b| now >= b.next_start),
                _ => true,
            });
        }

        if actions.is_empty() {
            debug!("nothing to reconcile");
        } else {
            info!(actions = actions.len(), fenced, "reconciling");
        }

        // A repeatedly restarted instance is not fixed by restarting: ask the leader to move it.
        // The restart still happens either way, so a refused move does not leave it down.
        {
            let keep: BTreeSet<&InstanceId> =
                targets.iter().map(|t| &t.assignment.instance).collect();
            self.restarts
                .write()
                .await
                .retain(|id, _| keep.contains(id));
        }
        for action in &actions {
            let Action::Start(id) = action else { continue };
            if self.note_restart_and_check_crash_loop(id).await
                && !self.try_reschedule_away(state, id).await
            {
                // Nowhere better to go: restart in place, with backoff.
                let mut backoffs = self.backoffs.write().await;
                backoffs
                    .entry(id.clone())
                    .or_insert_with(|| RestartBackoff::new(Instant::now()))
                    .record_loop(Instant::now());
            }
        }

        // A fence's stops are the only batch that can be cut short by a release.
        let fencing = fenced.then_some(still_fenced);
        reconciler
            .apply(&actions, &targets, &actual, fencing, &priorities)
            .await?;
        self.report_container_status(state, &actual).await;
        self.expire_updates(state, &actual).await;
        if !fenced {
            self.clean_images(state, &docker, &targets, &actual).await;
        }

        let keep: BTreeMap<String, ()> = targets
            .iter()
            .map(|t| (t.assignment.instance.container_name(), ()))
            .collect();
        if let Err(e) = self.materializer.clean_orphans(&keep) {
            warn!(error = %e, "could not clean orphaned objects");
        }

        // A writable config a container changed: feed it back through raft (`submit` forwards to the leader).
        let mut committed: Vec<(String, u64)> = Vec::new();
        for edit in config_edits {
            // Compare-and-set on the revision edited from, so a second writer does not overwrite a newer change.
            let request = Request::PutConfigCas {
                name: edit.name.clone(),
                content: base64_encode(&edit.content),
                owner: edit.owner,
                expect_revision: edit.base_revision,
            };
            match self.cluster.submit(request).await {
                Ok(response) => {
                    if let Some(error) = response.error {
                        warn!(config = %edit.name, %error, "container's edit to a config was rejected");
                    } else {
                        info!(config = %edit.name, "wrote a container's config edit back to the cluster");
                        committed.push((edit.name.clone(), edit.base_revision + 1));
                    }
                }
                Err(e) => {
                    warn!(config = %edit.name, error = %e, "could not submit a container's config edit")
                }
            }
        }

        self.settle_config_edits(&committed).await;

        Ok(())
    }

    /// Finish a write-back: the edit is committed once `submit` returns, but this node's applied
    /// state may lag. Wait for the new revision, then re-materialize every local instance
    /// mounting the config. Other nodes do so on their own next sweep.
    async fn settle_config_edits(&self, committed: &[(String, u64)]) {
        if committed.is_empty() {
            return;
        }
        let deadline = Instant::now() + CONFIG_SETTLE_TIMEOUT;
        let state = loop {
            let state = self.cluster.state().await;
            let applied = committed.iter().all(|(name, revision)| {
                state
                    .configs
                    .get(name)
                    .is_some_and(|c| c.revision >= *revision)
            });
            if applied {
                break state;
            }
            if Instant::now() >= deadline {
                warn!("a config write-back is not yet applied locally; its copies update on the next sweep");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        for assignment in state.assignments_on(&self.node) {
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
            if !service
                .configs
                .iter()
                .any(|c| committed.iter().any(|(name, _)| *name == c.source))
            {
                continue;
            }
            if let Err(e) =
                self.materializer
                    .prepare(&state, instance, &service.configs, &service.secrets)
            {
                warn!(instance = %instance, error = %e, "could not refresh a config copy after a write-back");
            }
        }
    }

    /// Show the operator why an instance was not started via a failed update record (the UI's
    /// one per-instance error channel); skipped if already recorded, so a steady refusal costs one write.
    async fn report_refused(&self, state: &ClusterState, refused: &[(InstanceId, String)]) {
        let reporter = UpdateReporter::new(self.cluster.clone(), self.node.clone());
        for (instance, error) in refused {
            let already = state.instance_updates.get(instance).is_some_and(|u| {
                u.node == self.node
                    && u.phase == UpdatePhase::Failed
                    && u.error.as_deref() == Some(error.as_str())
            });
            if !already {
                reporter
                    .report(instance, UpdatePhase::Failed, Some(error.clone()))
                    .await;
            }
        }
    }

    /// Self-report this node's running/stopped state per instance so waiters (backup, restore,
    /// folder import) can poll `container_status` instead of sleeping. Skipped when unchanged.
    async fn report_container_status(
        &self,
        state: &ClusterState,
        actual: &[cl_docker::ContainerState],
    ) {
        let running: BTreeMap<InstanceId, bool> = actual
            .iter()
            .filter_map(|c| {
                Some((
                    c.labels
                        .get(cl_docker::LABEL_INSTANCE)?
                        .parse::<InstanceId>()
                        .ok()?,
                    c.running,
                ))
            })
            .collect();

        let label_revisions = |label: &str| -> BTreeMap<InstanceId, BTreeMap<String, u64>> {
            actual
                .iter()
                .filter_map(|c| {
                    let id = c.labels.get(cl_docker::LABEL_INSTANCE)?.parse().ok()?;
                    let value = c.labels.get(label)?;
                    Some((id, reconcile::decode_secret_revisions(value)))
                })
                .collect()
        };
        let revisions = label_revisions(reconcile::LABEL_SECRETS);
        let config_revs = label_revisions(reconcile::LABEL_CONFIGS);

        type Revisions = BTreeMap<String, u64>;
        let report: BTreeMap<InstanceId, (bool, Revisions, Revisions)> = state
            .assignments_on(&self.node)
            .map(|a| {
                (
                    a.instance.clone(),
                    (
                        running.get(&a.instance).copied().unwrap_or(false),
                        revisions.get(&a.instance).cloned().unwrap_or_default(),
                        config_revs.get(&a.instance).cloned().unwrap_or_default(),
                    ),
                )
            })
            .collect();

        let current: BTreeMap<InstanceId, (bool, Revisions, Revisions)> = state
            .container_status
            .iter()
            .filter(|(_, status)| status.node == self.node)
            .map(|(id, status)| {
                (
                    id.clone(),
                    (
                        status.running,
                        status.secret_revisions.clone(),
                        status.config_revisions.clone(),
                    ),
                )
            })
            .collect();

        if report == current {
            return;
        }
        if let Err(e) = self
            .cluster
            .submit(Request::ReportContainerStatus {
                node: self.node.clone(),
                running: report
                    .iter()
                    .map(|(id, (r, _, _))| (id.clone(), *r))
                    .collect(),
                secret_revisions: report
                    .iter()
                    .map(|(id, (_, revs, _))| (id.clone(), revs.clone()))
                    .collect(),
                config_revisions: report
                    .into_iter()
                    .map(|(id, (_, _, revs))| (id, revs))
                    .collect(),
            })
            .await
        {
            warn!(error = %e, "could not report container status");
        }
    }

    /// Remove old images (see `crate::images`) after a successful update here, at daemon start
    /// and daily. Backgrounded; never fails the pass.
    async fn clean_images(
        &self,
        state: &ClusterState,
        docker: &Docker,
        targets: &[Target],
        actual: &[cl_docker::ContainerState],
    ) {
        // With the wanted set not fully known, cleanup could mistake a service's image for unwanted.
        if state.in_emergency() {
            return;
        }
        let mut busy = !self.pulling.read().await.is_empty();
        let mut succeeded = Vec::new();
        for (instance, update) in &state.instance_updates {
            if update.node != self.node {
                continue;
            }
            match update.phase {
                UpdatePhase::Started => succeeded.push((instance.to_string(), update.at)),
                UpdatePhase::Failed => {}
                _ => busy = true,
            }
        }

        let mut wanted = crate::images::Wanted::default();
        let mut known = true;
        for assignment in state.assignments_on(&self.node) {
            let instance = &assignment.instance;
            let Some(stack) = state.stacks.get(&instance.stack) else {
                continue;
            };
            let Ok(file) = StackFile::parse(&stack.spec) else {
                // Cannot tell what the stack wants, so do not guess.
                known = false;
                break;
            };
            if let Some(service) = file.services.get(&instance.service) {
                wanted.add(&service.image);
            }
        }
        if !known || !self.image_gc.due(&succeeded, busy) {
            return;
        }
        for target in targets {
            wanted.add(&target.spec.image);
        }
        let managed: BTreeSet<String> = actual
            .iter()
            .map(|c| crate::images::repo_of(&c.image))
            .collect();

        let gc = self.image_gc.clone();
        let docker = docker.clone();
        let now = unix_now();
        tokio::spawn(async move {
            gc.sweep(&docker, &wanted, &managed, now).await;
        });
    }

    /// Tidy this node's `instance_updates`: drop finished ones after a while and fail ones nothing
    /// drives any more (a restart mid-update). Also the safety net for a new container that came
    /// up unrecorded: at `Starting` the old one is gone, so any running one is the new one.
    async fn expire_updates(&self, state: &ClusterState, actual: &[cl_docker::ContainerState]) {
        let now = unix_now();
        let running: BTreeMap<InstanceId, Option<cl_docker::HealthState>> = actual
            .iter()
            .filter(|c| c.running)
            .filter_map(|c| {
                Some((
                    c.labels.get(cl_docker::LABEL_INSTANCE)?.parse().ok()?,
                    c.health,
                ))
            })
            .collect();
        for (instance, update) in &state.instance_updates {
            if update.node != self.node {
                continue;
            }
            if update.phase == UpdatePhase::Starting {
                if let Some(health) = running.get(instance) {
                    // Up is not the same as working: with a healthcheck the
                    // update is only a success once docker says healthy.
                    let reporter = UpdateReporter::new(self.cluster.clone(), self.node.clone());
                    match reconcile::health_gate(*health, now - update.at) {
                        reconcile::HealthGate::Ready => {
                            reporter.report(instance, UpdatePhase::Started, None).await;
                        }
                        reconcile::HealthGate::Wait => {}
                        reconcile::HealthGate::Failed(why) => {
                            reporter
                                .report(instance, UpdatePhase::Failed, Some(why.to_string()))
                                .await;
                        }
                    }
                    continue;
                }
            }
            let request = match expire_verdict(update, now, self.booted_at) {
                Some(None) => Request::ClearInstanceUpdate {
                    instance: instance.clone(),
                },
                Some(Some(error)) => Request::SetInstanceUpdate {
                    instance: instance.clone(),
                    node: self.node.clone(),
                    phase: UpdatePhase::Failed,
                    error: Some(error.to_string()),
                    at: now,
                },
                None => continue,
            };
            if let Err(e) = self.cluster.submit(request).await {
                warn!(%instance, error = %e, "could not expire an update record");
            }
        }
    }

    /// Leader only: settle update records no live process drives (owner gone, moved, or a
    /// `Queued` record whose api node restarted); otherwise they stay non-terminal for an hour
    /// and `apply_selection` refuses another update on the instance.
    async fn expire_orphaned_updates(&self, state: &ClusterState) {
        let now = unix_now();
        for (instance, update) in &state.instance_updates {
            if update.node == self.node {
                continue;
            }
            let request = match orphan_verdict(state, instance, update, now) {
                Some(None) => Request::ClearInstanceUpdate {
                    instance: instance.clone(),
                },
                Some(Some(why)) => Request::SetInstanceUpdate {
                    instance: instance.clone(),
                    node: update.node.clone(),
                    phase: UpdatePhase::Failed,
                    error: Some(why.to_string()),
                    at: now,
                },
                None => continue,
            };
            if let Err(e) = self.cluster.submit(request).await {
                warn!(%instance, error = %e, "could not expire an orphaned update record");
            }
        }
    }

    /// Records this sweep's restart of `id` and reports whether it has now
    /// happened `CRASH_LOOP_THRESHOLD` times within `CRASH_LOOP_WINDOW`.
    async fn note_restart_and_check_crash_loop(&self, id: &InstanceId) -> bool {
        let now = Instant::now();
        let mut restarts = self.restarts.write().await;
        let deque = restarts.entry(id.clone()).or_default();
        record_restart(deque, now)
    }

    /// Ask the leader to move a crash-looping instance via `BeginMigration` (as a manual move
    /// or `clouduler rebalance` does, so racing redeploys are rejected there).
    ///
    /// Skipped (restart in place) for pinned, global or anchored instances (same exclusions as
    /// `rebalance::plan`) and when a migration is under way. Nodes it was moved away from within
    /// `CRASH_MOVE_RETENTION_SECS` (`ClusterState::crash_moves`) are excluded so a service that
    /// crashes everywhere stops hopping. Returns whether a move was submitted.
    async fn try_reschedule_away(&self, state: &ClusterState, id: &InstanceId) -> bool {
        let Some(assignment) = state.assignments.get(id) else {
            return false;
        };
        if assignment.node != self.node {
            return false;
        }
        if assignment
            .migration
            .as_ref()
            .is_some_and(|m| !m.phase.is_terminal())
        {
            return false;
        }

        let live = self.live_config(state);
        let colocate_proxy_hosts = live.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
        let desired = desired_instances(
            state,
            &live.egress_networks(),
            &live.port_gated_networks(),
            colocate_proxy_hosts,
        );
        let desired = with_container_routers(desired, state, &live);
        let Some(item) = desired.iter().find(|d| &d.id == id) else {
            return false;
        };

        if item.global || item.pinned_node.is_some() || item.anchor_keys().next().is_some() {
            warn!(instance = %id, "keeps restarting here but cannot be safely moved (pinned, global, or anchored), restarting in place");
            return false;
        }

        let Ok((eligible, _)) = place::candidates(
            state,
            item,
            &live.served_drives(),
            &live.everywhere_networks(),
        ) else {
            return false;
        };
        let now = unix_now().max(0) as u64;
        let Some(target) = pick_crash_move_target(
            eligible.iter().copied(),
            &self.node,
            state.crash_moves.get(id),
            now,
            |node| state.assignments_on(node).count(),
        ) else {
            warn!(instance = %id, "keeps restarting here and every other node that can run it has already been tried, restarting in place");
            return false;
        };

        warn!(instance = %id, from = %self.node, to = %target.name, "instance keeps restarting here, moving it to another node");
        match self
            .cluster
            .submit(Request::BeginMigration {
                instance: id.clone(),
                to: target.name.clone(),
                drives: item.drives.clone(),
            })
            .await
        {
            Ok(response) => {
                if let Some(error) = response.error {
                    warn!(instance = %id, %error, "could not move a crash-looping instance away");
                } else {
                    self.restarts.write().await.remove(id);
                    // Remember where it crashed so the next owner does not send it back. Submitted after the
                    // move so a rejected move leaves no history.
                    let record = Request::RecordCrashMove {
                        instance: id.clone(),
                        from: self.node.clone(),
                        at: now,
                    };
                    if let Err(e) = self.cluster.submit(record).await {
                        warn!(instance = %id, error = %e, "could not record a crash-loop move");
                    }
                    return true;
                }
            }
            Err(e) => {
                warn!(instance = %id, error = %e, "could not submit a move for a crash-looping instance")
            }
        }
        false
    }

    /// Build what this node should run, materializing configs and secrets, and return any edit
    /// a container made to a writable config mount.
    fn targets(&self, state: &ClusterState) -> crate::Result<Targets> {
        let live = self.live_config(state);
        let (drive_roots, dynamic_mounts) = self.drive_roots(state, &live);

        // Decoded once per pass; every container reads the same address book.
        let allocations = Allocations::decode(state.overlay.as_deref());
        let egress_networks = live.egress_networks();

        let mut targets = Vec::new();
        let mut config_edits = Vec::new();
        let mut refused = Vec::new();

        for assignment in state.assignments_on(&self.node) {
            let instance = &assignment.instance;
            // In emergency mode (`clouduler emergency`) another stack's instance is not a target; its
            // container is left alone because `reconcile::restrict_to_emergency_stack` drops the removal.
            if !state.emergency_allows(&instance.stack) {
                continue;
            }
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

            // A served-drive volume this node does not serve is a FUSE mount set up on demand and maybe
            // not up yet; starting against the empty directory would bind the wrong data, so wait.
            if !volumes_ready(service, &drive_roots, &dynamic_mounts) {
                debug!(instance = %instance, "waiting for a served drive volume to be mounted");
                continue;
            }

            // A missing secret or key blocks this container, not the whole pass.
            let objects =
                match self
                    .materializer
                    .prepare(state, instance, &service.configs, &service.secrets)
                {
                    Ok(objects) => objects,
                    Err(e) => {
                        warn!(instance = %instance, error = %e, "cannot prepare objects, skipping");
                        continue;
                    }
                };
            config_edits.extend(objects.config_edits.clone());

            let network =
                overlay_network_name(&file, &live).map(|n| overlay_name(&instance.stack, n));

            // Containers on a stack network use the leader-allocated address (other nodes build DNS and
            // proxy backends from it). Nothing to run until allocated.
            let mut placement = spec::Placement {
                network: network.clone(),
                ..Default::default()
            };
            if let Some(network) = &network {
                let Some(ip) = allocations
                    .as_ref()
                    .and_then(|a| a.instance_ip(network, instance))
                else {
                    debug!(instance = %instance, network = %network, "no overlay address yet");
                    continue;
                };
                placement.ip = Some(ip);
                placement.resolver = allocations.as_ref().and_then(|a| a.gateway(network));
                placement.zone = self.zone.clone();
                placement.published_hosts =
                    exposed_allowed_hosts(state, &file).into_iter().collect();
            }
            placement.egress_network = service
                .networks
                .iter()
                .find(|n| egress_networks.contains(*n))
                .map(
                    |n| match live.networks.get(n.as_str()).map(|net| net.kind) {
                        Some(NetworkKind::Nat) => cl_net::egress::nat_docker_name(n),
                        _ => cl_net::egress::egress_docker_name(n),
                    },
                );

            // Container-kind networks: the instance's own segment address (pinned, part of the revision)
            // and routes (not: they follow the router and are re-applied every tick).
            match crate::container_net::plan(
                instance,
                &file.container_networks(&instance.service, &live),
                &live,
                allocations.as_ref(),
                &self.node,
            ) {
                Ok(plan) => {
                    placement.container_networks = plan.attachments;
                    placement.ip_forward = plan.router;
                }
                Err(pending) => {
                    debug!(instance = %instance, reason = %pending.0, "no container network address yet");
                    continue;
                }
            }

            // Internal-network node addresses where the proxy answers; only member services get them
            // kept off the egress default route.
            let on_internal = live
                .internal_network()
                .is_some_and(|(internal, _)| service.networks.contains(internal));
            if on_internal {
                placement.proxy_addresses = live
                    .nodes
                    .values()
                    .filter_map(|node| match live.internal_ip(node) {
                        Some(std::net::IpAddr::V4(ip)) => Some(ip),
                        _ => None,
                    })
                    .collect();
            }

            // An instance migrating away must not be restarted here between the migration stopping it
            // and the handover finishing (the loops would fight and dirty the frozen volume). Recorded
            // as a stopped target so a failed migration is one start from restored. Past `Switching`
            // the assignment names the target, so this test lets the target start it.
            let mut assignment = assignment.clone();
            if assignment
                .migration
                .as_ref()
                .is_some_and(|m| !m.phase.is_terminal() && m.to != self.node)
            {
                assignment.desired = DesiredState::Stopped;
            }

            let container_ips: Vec<std::net::Ipv4Addr> =
                placement.container_networks.iter().map(|a| a.ip).collect();
            let revision = revision_with_mounts(
                stack.revision,
                service,
                &dynamic_mounts,
                placement.ip,
                &container_ips,
            );

            let mut spec = match spec::build(
                instance,
                service,
                spec::Materialization {
                    objects: &objects.mounts,
                    env: &objects.env,
                    drive_roots: &drive_roots,
                    docker_api_socket: self.materializer.docker_api_socket(),
                },
                placement,
                assignment.image_digest.as_deref(),
                revision,
            ) {
                Ok(spec) => spec,
                Err(e) => {
                    // Deterministic, so left out of `targets`: no `Start` is planned, which keeps it out of the
                    // crash-loop counter and `try_reschedule_away` (another node would refuse too). The operator
                    // sees it through `refused`.
                    error!(instance = %instance, error = %e, "refusing to start an instance whose spec is invalid");
                    refused.push((instance.clone(), e.to_string()));
                    continue;
                }
            };

            spec.labels.insert(
                reconcile::LABEL_STACK_REVISION.to_string(),
                stack.revision.to_string(),
            );

            if !objects.secret_revisions.is_empty() {
                spec.labels.insert(
                    reconcile::LABEL_SECRETS.to_string(),
                    reconcile::encode_secret_revisions(&objects.secret_revisions),
                );
            }

            if !objects.config_revisions.is_empty() {
                spec.labels.insert(
                    reconcile::LABEL_CONFIGS.to_string(),
                    reconcile::encode_secret_revisions(&objects.config_revisions),
                );
            }

            targets.push(Target {
                assignment,
                spec,
                revision,
                stop_grace: effective_stop_grace(service, live.failover.stop_grace),
            });
        }

        Ok((targets, config_edits, refused))
    }

    /// Where this node resolves each drive's root, and which roots are FUSE mountpoints owned by
    /// the drive subsystem (`volumes_ready` waits on those). A replicated drive resolves to the
    /// node's declared local copy; a served drive only on the node serving it, elsewhere to a
    /// per-volume directory the drive subsystem (`clouduler::drives`) FUSE-mounts on demand.
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
            if active_server(name, drive, state) != Some(self.node.as_str()) {
                roots.insert(name.clone(), data_dir.join("volume-mounts").join(name));
                dynamic.insert(name.clone());
            }
        }

        (roots, dynamic)
    }
}

/// Folds the FUSE-mounted state of a service's drives and the instance's overlay address into
/// the drift revision, on top of `stack.revision`. Hashed since it is only compared for equality.
///
/// Drives: a served drive's active server can change with no stack change, and `diff` only
/// compares `LABEL_REVISION`, so a container keeps a stale FUSE bind source baked in and
/// `Action::Start` never recreates it.
///
/// Address: `cl_net::ipam` frees and reissues an instance's address when it briefly drops out
/// of the assignment table (a redeploy's replanning), while the untouched container keeps the
/// old one; other nodes' proxy and DNS move on and every request gets a permanent `bad gateway`.
fn revision_with_mounts(
    revision: u64,
    service: &cl_config::Service,
    dynamic_mounts: &BTreeSet<String>,
    ip: Option<std::net::Ipv4Addr>,
    container_ips: &[std::net::Ipv4Addr],
) -> u64 {
    let mut drives: Vec<&str> = service
        .volumes
        .iter()
        .filter_map(|v| v.drive.as_deref())
        .collect();
    drives.sort_unstable();
    drives.dedup();

    let mut bits: u64 = 0;
    for (i, drive) in drives.iter().enumerate().take(32) {
        if dynamic_mounts.contains(*drive) {
            bits |= 1 << i;
        }
    }

    use std::hash::{Hash, Hasher};
    let mut hasher = cl_config::stable_hasher();
    revision.hash(&mut hasher);
    bits.hash(&mut hasher);
    ip.hash(&mut hasher);
    // Only the instance's own container-network addresses: the router's address and routes are
    // left out so a router appearing or moving never recreates clients. Skipped when empty.
    if !container_ips.is_empty() {
        container_ips.hash(&mut hasher);
    }
    hasher.finish()
}

/// The node serving a `Served` drive: the raft holder while reachable, else the first
/// reachable `served_by` candidate. Same rule as `DriveRegistry::active_server`, duplicated
/// to avoid depending on `cl-drive`.
fn active_server<'a>(
    name: &str,
    drive: &'a cl_config::Drive,
    state: &ClusterState,
) -> Option<&'a str> {
    cl_raft::served::choose_active_server(
        &drive.served_by,
        &state.unreachable,
        state.served.get(name),
    )
}

/// Whether `path` is a mount point (device differs from its parent's), telling a live FUSE
/// mount from an unmounted directory without `cl-drive`.
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

/// Whether every served-drive volume this service mounts is mounted here yet. Replicated
/// volumes and served drives this node serves are always ready; only `dynamic` mounts lag.
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

/// `UpsertVolume` for every volume a placed instance mounts that the cluster does not know.
/// Without a record nothing replicates, `/api/volumes` reports nothing and a migration has
/// nothing to hand over.
///
/// The primary registered is the node holding the data: where the instance landed for a
/// replicated drive, the drive's active server for a served one (`check_placement` only waives
/// the drive requirement for a served drive's real primary).
///
/// # Following the instance
///
/// A replicated volume already on record gets a `TouchVolume` when its primary is no longer
/// the node running the instance. This fires after a real failover (placement prefers the
/// primary, `place::sole_primary`): until the primary follows, the sync scheduler treats the
/// departed node as source and, when it returns, syncs its stale copy over the live one,
/// every interval, forever (every rolling restart makes nodes briefly unreachable).
///
/// Not done for a served drive (its primary is the active server) or an instance mid-migration
/// (`cl_drive::migration` hands the primary over itself at `Switching`).
/// Hostnames this stack's `allowed` lists reach on a port another service publishes with
/// `x-clouduler.expose` (`host:port` where a service has `hostname: host` and that published
/// port/protocol). Lower case.
///
/// Such a name must lead to the expose forwarder on the caller's own node, not the backend's
/// shared-bridge address (port unreachable through the firewall and not behind the PROXY
/// header). The container resolves it to its node's gateway (`Placement::published_hosts`) and
/// the firewall opens the port (`cl_daemon::restrict`).
pub fn exposed_allowed_hosts(state: &ClusterState, file: &StackFile) -> BTreeSet<String> {
    use cl_config::AllowedHost;
    let wanted: Vec<(String, cl_config::AllowedPorts)> = file
        .networks
        .values()
        .filter_map(|def| def.as_ref()?.allowed.as_ref())
        .flatten()
        .filter_map(|t| match (&t.host, t.ports) {
            (AllowedHost::Name(n), Some(ports)) => Some((n.to_ascii_lowercase(), ports)),
            _ => None,
        })
        .collect();
    if wanted.is_empty() {
        return BTreeSet::new();
    }
    let mut out = BTreeSet::new();
    for stack in state.stacks.values() {
        let Ok(other) = StackFile::parse(&stack.spec) else {
            continue;
        };
        for service in other.services.values() {
            let Some(host) = service.hostname.as_deref() else {
                continue;
            };
            let host = host.to_ascii_lowercase();
            let Some(ext) = service.x_clouduler.as_ref() else {
                continue;
            };
            for (name, ports) in &wanted {
                if *name == host
                    && ext.expose.iter().any(|e| {
                        e.protocol == ports.protocol
                            && (ports.range.start..=ports.range.end).contains(&e.published)
                    })
                {
                    out.insert(name.clone());
                }
            }
        }
    }
    out
}

pub fn volume_requests(state: &ClusterState, live: &ClusterConfig) -> Vec<Request> {
    let mut requests = Vec::new();
    // Every replica mounts the same volume, so the same id comes up once per instance.
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
            if !seen.insert(id.clone()) {
                continue;
            }
            let served = live.drives.get(drive).is_some_and(|cfg| cfg.is_served());

            if let Some(record) = state.volumes.get(&id) {
                if !served && record.primary != assignment.node && assignment.migration.is_none() {
                    requests.push(Request::TouchVolume {
                        id,
                        primary: assignment.node.clone(),
                    });
                }
                continue;
            }

            let primary = match live.drives.get(drive) {
                Some(cfg) if served => active_server(drive, cfg, state).map(str::to_string),
                _ => Some(assignment.node.clone()),
            };
            let Some(primary) = primary else {
                // Every candidate is unreachable; retry next sweep.
                continue;
            };
            requests.push(Request::UpsertVolume { id, primary });
        }
    }
    requests
}

/// Pushes `now` and reports whether `CRASH_LOOP_THRESHOLD` restarts sit within
/// `CRASH_LOOP_WINDOW`, pruning older ones. Pure, so it is testable without a cluster.
fn record_restart(deque: &mut VecDeque<Instant>, now: Instant) -> bool {
    deque.push_back(now);
    while deque
        .front()
        .is_some_and(|t| now.duration_since(*t) > CRASH_LOOP_WINDOW)
    {
        deque.pop_front();
    }
    deque.len() >= CRASH_LOOP_THRESHOLD
}

/// The least-loaded node to move a crash-looping instance to, excluding its own node and
/// ones it was moved away from within the retention window. `None` once all were tried.
fn pick_crash_move_target<'a>(
    eligible: impl Iterator<Item = &'a NodeState>,
    current: &str,
    history: Option<&BTreeMap<String, u64>>,
    now: u64,
    load: impl Fn(&str) -> usize,
) -> Option<&'a NodeState> {
    eligible
        .filter(|n| n.name != current)
        .filter(|n| {
            history
                .and_then(|h| h.get(&n.name))
                .is_none_or(|at| now.saturating_sub(*at) > CRASH_MOVE_RETENTION_SECS)
        })
        .min_by_key(|n| load(&n.name))
}

/// In-place restart delay for an instance that crash-loops on every node.
#[derive(Debug, Clone, Copy)]
struct RestartBackoff {
    loops: u32,
    next_start: Instant,
}

impl RestartBackoff {
    fn new(now: Instant) -> Self {
        Self {
            loops: 0,
            next_start: now,
        }
    }

    /// Another crash loop: double the delay (capped).
    fn record_loop(&mut self, now: Instant) {
        let delay = CRASH_BACKOFF_BASE
            .saturating_mul(1u32.checked_shl(self.loops).unwrap_or(u32::MAX))
            .min(CRASH_BACKOFF_MAX);
        self.loops = self.loops.saturating_add(1);
        self.next_start = now + delay;
    }

    /// Whether the instance stayed up long enough since the backoff expired to reset it.
    fn is_forgettable(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.next_start) > CRASH_BACKOFF_FORGET_AFTER
    }
}

/// What one pass of `Reconciler::targets` decided: what to run, the config
/// edits containers made, and the instances refused (with why).
type Targets = (
    Vec<Target>,
    Vec<crate::materialize::ConfigEdit>,
    Vec<(InstanceId, String)>,
);

/// Docker network name for a stack's declared network.
pub fn overlay_name(stack: &str, network: &str) -> String {
    format!("{stack}_{network}")
}

/// The one declared network a stack gets a VXLAN overlay for: an internal or private network
/// if declared (its domain is what containers resolve on), else a public network (needed
/// cross-node for the proxy; isolated per stack). A stack with only egress or wireguard
/// networks needs none and must not wait for an address on one.
pub fn overlay_network_name<'a>(file: &'a StackFile, cfg: &ClusterConfig) -> Option<&'a str> {
    file.overlay_network(cfg)
}

/// The first pair of distinct stacks whose overlays share a docker network name, as
/// `(overlay, stack, other stack)`. `overlay_name` joins with `_`, so stack `a_b` on `c` and
/// `a` on `b_c` both map to `a_b_c` (one IPAM allocation, no isolation). Names cannot change
/// (live clusters), so a deploy or rename that would collide is refused.
pub fn overlay_collision<'a>(
    cfg: &ClusterConfig,
    stacks: impl IntoIterator<Item = (&'a str, &'a StackFile)>,
) -> Option<(String, String, String)> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for (stack, file) in stacks {
        let Some(network) = overlay_network_name(file, cfg) else {
            continue;
        };
        let overlay = overlay_name(stack, network);
        match seen.get(overlay.as_str()) {
            Some(other) if *other != stack => {
                return Some((overlay, (*other).to_string(), stack.to_string()));
            }
            _ => {
                seen.insert(overlay, stack);
            }
        }
    }
    None
}

/// The overlay subnet a stack pins with `x-clouduler.subnet`, if any.
pub fn overlay_subnet_of(file: &StackFile, cfg: &ClusterConfig) -> Option<ipnet::Ipv4Net> {
    overlay_network_name(file, cfg)?;
    file.x_clouduler.as_ref()?.subnet
}

/// The first pair of distinct stacks that pin overlapping overlay subnets, as
/// `(subnet, stack, other stack)`.
pub fn overlay_subnet_collision<'a>(
    cfg: &ClusterConfig,
    stacks: impl IntoIterator<Item = (&'a str, &'a StackFile)>,
) -> Option<(ipnet::Ipv4Net, String, String)> {
    let mut seen: Vec<(ipnet::Ipv4Net, &str)> = Vec::new();
    for (stack, file) in stacks {
        let Some(subnet) = overlay_subnet_of(file, cfg) else {
            continue;
        };
        if let Some((other_net, other)) = seen.iter().find(|(n, o)| {
            *o != stack && (n.contains(&subnet.network()) || subnet.contains(&n.network()))
        }) {
            let _ = other_net;
            return Some((subnet, (*other).to_string(), stack.to_string()));
        }
        seen.push((subnet, stack));
    }
    None
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

/// `x-clouduler.priority` of every deployed stack that parses, with the stacks
/// `clouduler prioritize` bumped ahead of all the others.
pub fn stack_priorities(state: &ClusterState) -> reconcile::Priorities {
    state
        .stacks
        .iter()
        .filter_map(|(name, stack)| {
            let own = StackFile::parse(&stack.spec).ok()?.priority();
            Some((name.clone(), state.effective_priority(name, own)))
        })
        .collect()
}

/// Every instance every deployed stack wants, expanded against the cluster's egress/public
/// network sets (`ClusterConfig::egress_networks` / `port_gated_networks`). Shared with the
/// api's unplaced reporting.
///
/// While `state.emergency_stacks` is non-empty, other stacks are left out entirely (not
/// unplaceable); no assignment's `desired` is mutated, so they resume after `ClearEmergency`.
pub fn desired_instances(
    state: &ClusterState,
    egress_networks: &BTreeSet<String>,
    port_gated_networks: &BTreeSet<String>,
    colocate_proxy_hosts: bool,
) -> Vec<Desired> {
    let mut desired = Vec::new();
    for (name, stack) in &state.stacks {
        if !state.emergency_allows(name) {
            continue;
        }
        match StackFile::parse(&stack.spec) {
            Ok(file) => desired.extend(plan::expand(
                name,
                &file,
                egress_networks,
                port_gated_networks,
                colocate_proxy_hosts,
            )),
            // A stored stack that no longer parses must not stop other stacks scheduling.
            Err(e) => warn!(stack = %name, error = %e, "stored stack does not parse"),
        }
    }
    desired
}

/// Fill `near` on every container network's router with the services listing that network.
/// Routers are moved after the instances they prefer, so placement has counted those; order
/// is otherwise untouched.
pub fn with_container_routers(
    mut desired: Vec<Desired>,
    state: &ClusterState,
    cfg: &ClusterConfig,
) -> Vec<Desired> {
    if cfg.container_networks().next().is_none() {
        return desired;
    }
    // network -> (stack, service) of every client
    let mut clients: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
    for (name, stack) in &state.stacks {
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        for service in file.services.keys() {
            for net in file.container_networks(service, cfg) {
                clients
                    .entry(net)
                    .or_default()
                    .insert((name.clone(), service.clone()));
            }
        }
    }
    for item in &mut desired {
        if let Some(net) = cfg.container_router_of(&item.id.stack, &item.id.service) {
            if let Some(users) = clients.get(net) {
                item.near = users
                    .iter()
                    .filter(|(s, svc)| !(s == &item.id.stack && svc == &item.id.service))
                    .cloned()
                    .collect();
            }
        }
    }
    desired.sort_by_key(|d| !d.near.is_empty());
    desired
}

/// Drives whose mode is `Served`; empty (strict replicated eligibility) if no config yet or unparseable.
fn served_drives(state: &ClusterState) -> BTreeSet<String> {
    state
        .cluster_config
        .as_ref()
        .and_then(|entry| ClusterConfig::parse(&entry.yaml).ok())
        .map(|cfg| cfg.served_drives())
        .unwrap_or_default()
}

/// A service's own `stop_grace_period` when set, else the cluster default.
fn effective_stop_grace(
    service: &cl_config::Service,
    default: std::time::Duration,
) -> std::time::Duration {
    service.stop_grace_period.unwrap_or(default)
}

/// `failover.stop_grace` for a container without its own `stop_grace_period`; built-in
/// default if no config yet or unparseable.
pub fn configured_stop_grace(state: &ClusterState) -> std::time::Duration {
    state
        .cluster_config
        .as_ref()
        .and_then(|entry| ClusterConfig::parse(&entry.yaml).ok())
        .map(|cfg| cfg.failover.stop_grace)
        .unwrap_or(cl_config::DEFAULT_STOP_GRACE)
}

/// Nat-kind network names, like `served_drives`; empty if no config yet or unparseable.
fn everywhere_networks(state: &ClusterState) -> BTreeSet<String> {
    state
        .cluster_config
        .as_ref()
        .and_then(|entry| ClusterConfig::parse(&entry.yaml).ok())
        .map(|cfg| cfg.everywhere_networks())
        .unwrap_or_default()
}

/// Every request the leader would submit for a state, without submitting (for tests).
pub fn plan_requests(state: &ClusterState) -> crate::Result<Vec<Request>> {
    let desired = desired_instances(state, &BTreeSet::new(), &BTreeSet::new(), false);

    let outcome = place::place(
        state,
        &desired,
        &served_drives(state),
        &everywhere_networks(state),
    );
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

/// What the leader does with another node's update record (same shape as `expire_verdict`):
/// orphaned when its node left, is unreachable or no longer hosts the instance; a `Queued`
/// one is also given up on after `QUEUED_UPDATE_TTL_SECS`.
fn orphan_verdict(
    state: &ClusterState,
    instance: &InstanceId,
    update: &cl_raft::state::InstanceUpdate,
    now: i64,
) -> Option<Option<&'static str>> {
    let moved = state
        .assignments
        .get(instance)
        .is_none_or(|a| a.node != update.node);
    let orphaned = !state.nodes.contains_key(&update.node)
        || state.unreachable.contains(&update.node)
        || moved;
    if update.phase.is_terminal() {
        return (orphaned && now - update.at >= FINISHED_UPDATE_TTL_SECS).then_some(None);
    }
    if !state.nodes.contains_key(&update.node) {
        return Some(Some(
            "interrupted: the node running this update left the cluster",
        ));
    }
    if state.unreachable.contains(&update.node) {
        return Some(Some(
            "interrupted: the node running this update is unreachable",
        ));
    }
    if moved {
        return Some(Some("interrupted: the instance moved to another node"));
    }
    if update.phase == UpdatePhase::Queued && now - update.at >= QUEUED_UPDATE_TTL_SECS {
        return Some(Some("stalled: queued but never started"));
    }
    None
}

/// What to do with one of this node's update records: `None` keep it,
/// `Some(None)` clear it, `Some(Some(why))` mark it failed.
fn expire_verdict(
    update: &cl_raft::state::InstanceUpdate,
    now: i64,
    booted_at: i64,
) -> Option<Option<&'static str>> {
    match update.phase {
        phase if phase.is_terminal() => {
            (now - update.at >= FINISHED_UPDATE_TTL_SECS).then_some(None)
        }
        _ if now - update.at >= STALLED_UPDATE_TTL_SECS => {
            Some(Some("stalled: no progress for an hour"))
        }
        // Queued is written by whichever node took the request, so only its age matters; the
        // driver that would start it may be gone.
        UpdatePhase::Queued => (now - update.at >= QUEUED_UPDATE_TTL_SECS)
            .then_some(Some("stalled: queued but never started")),
        // Pulling, stopping, starting: only ever written on this node, so
        // one from before this process started has lost its driver.
        _ => {
            (update.at < booted_at).then_some(Some("interrupted: the daemon restarted mid-update"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounts_service() -> cl_config::Service {
        service_with_volumes("      - fast/data:/data\n")
    }

    fn state_of(stacks: &[(&str, &str)]) -> ClusterState {
        let mut state = ClusterState::default();
        for (name, spec) in stacks {
            state.stacks.insert(
                name.to_string(),
                serde_json::from_value(serde_json::json!({
                    "name": name, "spec": spec, "revision": 1
                }))
                .unwrap(),
            );
        }
        state
    }

    const MAIL: &str = "services:
  mail:
    image: x
    hostname: Mail.example.org
    x-clouduler:
      expose:
        - { port: 587, published: 587, network: public }
";

    #[test]
    fn an_allowed_port_that_another_service_exposes_is_found() {
        let client = StackFile::parse(
            "services:\n  app:\n    image: x\nnetworks:\n  wan:\n    allowed: [\"mail.example.org:587\", \"mail.example.org:25\", \"other.example.org:587\"]\n",
        )
        .unwrap();
        let state = state_of(&[("mail", MAIL)]);
        assert_eq!(
            exposed_allowed_hosts(&state, &client),
            BTreeSet::from(["mail.example.org".to_string()])
        );
        assert!(exposed_allowed_hosts(&state_of(&[]), &client).is_empty());
    }

    #[test]
    fn a_services_stop_grace_period_overrides_the_cluster_default() {
        let default = Duration::from_secs(30);
        let mut service = mounts_service();
        assert_eq!(effective_stop_grace(&service, default), default);
        service.stop_grace_period = Some(Duration::from_secs(90));
        assert_eq!(
            effective_stop_grace(&service, default),
            Duration::from_secs(90)
        );
    }

    // Guards the move off `DefaultHasher` on the current toolchain. If a future
    // rustc changes `DefaultHasher`, this may start failing: that is expected,
    // delete it then and keep the pinned values.
    #[test]
    fn revision_matches_default_hasher_today() {
        use std::hash::{Hash, Hasher};
        let service = mounts_service();
        let dynamic: BTreeSet<String> = ["fast".to_string()].into();
        let ips = [None, Some(std::net::Ipv4Addr::new(10, 1, 2, 3))];
        for revision in [0u64, 7, u64::MAX] {
            for ip in ips {
                let mut old = std::collections::hash_map::DefaultHasher::new();
                revision.hash(&mut old);
                1u64.hash(&mut old);
                ip.hash(&mut old);
                assert_eq!(
                    revision_with_mounts(revision, &service, &dynamic, ip, &[]),
                    old.finish()
                );
            }
        }
    }

    #[test]
    fn revision_values_are_pinned() {
        let service = mounts_service();
        let dynamic: BTreeSet<String> = ["fast".to_string()].into();
        assert_eq!(
            revision_with_mounts(7, &service, &dynamic, None, &[]),
            5074014468841562254
        );
        assert_eq!(
            revision_with_mounts(
                7,
                &service,
                &BTreeSet::new(),
                Some(std::net::Ipv4Addr::new(10, 1, 2, 3)),
                &[]
            ),
            12979125382987782001
        );
    }
    use cl_raft::state::{Assignment, Availability, DesiredState, InstanceUpdate, NodeState, Role};

    fn update(phase: UpdatePhase, at: i64) -> InstanceUpdate {
        InstanceUpdate {
            node: "n1".into(),
            phase,
            started_at: at,
            at,
            error: None,
        }
    }

    #[test]
    fn finished_updates_are_cleared_after_a_while() {
        let done = update(UpdatePhase::Started, 1000);
        assert_eq!(expire_verdict(&done, 1010, 0), None);
        assert_eq!(
            expire_verdict(&done, 1000 + FINISHED_UPDATE_TTL_SECS, 0),
            Some(None)
        );
    }

    #[test]
    fn a_queued_update_is_given_up_on_after_fifteen_minutes() {
        let queued = update(UpdatePhase::Queued, 1000);
        assert_eq!(
            expire_verdict(&queued, 1000 + QUEUED_UPDATE_TTL_SECS - 1, 0),
            None
        );
        assert!(matches!(
            expire_verdict(&queued, 1000 + QUEUED_UPDATE_TTL_SECS, 0),
            Some(Some(_))
        ));
    }

    #[test]
    fn the_leader_expires_updates_whose_owner_is_gone_unreachable_or_no_longer_the_node() {
        let instance = InstanceId::new("s", "web", 0);
        let mut state = ClusterState::default();
        state.nodes.insert("n1".into(), node("n1", &[]));
        state.assignments.insert(
            instance.clone(),
            Assignment {
                instance: instance.clone(),
                node: "n1".into(),
                desired: DesiredState::Running,
                image_digest: None,
                migration: None,
            },
        );
        let pulling = update(UpdatePhase::Pulling, 1000);

        // Healthy owner, in progress: leave it to that node.
        assert_eq!(orphan_verdict(&state, &instance, &pulling, 1010), None);

        // Unreachable.
        state.unreachable.insert("n1".into());
        assert!(matches!(
            orphan_verdict(&state, &instance, &pulling, 1010),
            Some(Some(_))
        ));
        state.unreachable.clear();

        // The instance moved on.
        state
            .assignments
            .get_mut(&instance)
            .expect("assignment")
            .node = "n2".into();
        assert!(matches!(
            orphan_verdict(&state, &instance, &pulling, 1010),
            Some(Some(_))
        ));
        state
            .assignments
            .get_mut(&instance)
            .expect("assignment")
            .node = "n1".into();

        // Owner left the cluster.
        state.nodes.clear();
        assert!(matches!(
            orphan_verdict(&state, &instance, &pulling, 1010),
            Some(Some(_))
        ));

        // A finished orphan is cleared once old enough, not failed.
        let done = update(UpdatePhase::Failed, 1000);
        assert_eq!(orphan_verdict(&state, &instance, &done, 1010), None);
        assert_eq!(
            orphan_verdict(&state, &instance, &done, 1000 + FINISHED_UPDATE_TTL_SECS),
            Some(None)
        );
    }

    #[test]
    fn an_update_from_before_this_daemon_started_is_failed_but_a_queued_one_waits() {
        let pulling = update(UpdatePhase::Pulling, 1000);
        assert_eq!(expire_verdict(&pulling, 1010, 900), None);
        assert!(matches!(
            expire_verdict(&pulling, 1010, 1005),
            Some(Some(_))
        ));

        let queued = update(UpdatePhase::Queued, 1000);
        assert_eq!(expire_verdict(&queued, 1010, 1005), None);
        assert!(matches!(
            expire_verdict(&queued, 1000 + STALLED_UPDATE_TTL_SECS, 0),
            Some(Some(_))
        ));
    }
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
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
            memory_total_bytes: Default::default(),
            gpus: Default::default(),
            images: Default::default(),
            version: Default::default(),
        }
    }

    const STACK: &str = r#"
services:
  web:
    image: nginx
    deploy:
      replicas: 2
"#;

    const CLUSTER: &str = r#"
cluster: { name: home }
networks:
  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
  wan:       { type: egress }
  public:    { type: public, domain: example.com }
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network:
      tailscale: 10.10.0.1
      wan: eth0
      public: { domain: n1.example.com }
    drives: {}
web: { address: cloud, network: tailscale }
"#;

    #[test]
    fn overlay_network_name_picks_the_subnetted_one() {
        let cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");

        let internal =
            StackFile::parse("networks:\n  tailscale: {}\n  wan: {}\nservices: {}\n").unwrap();
        assert_eq!(overlay_network_name(&internal, &cfg), Some("tailscale"));

        // A stack whose networks are all egress needs no overlay at all:
        // this is the case that used to send `targets()` into an infinite
        // wait for an address on a network that never gets one.
        let no_overlay = StackFile::parse("networks:\n  wan: {}\nservices: {}\n").unwrap();
        assert_eq!(overlay_network_name(&no_overlay, &cfg), None);
    }

    /// A stack reachable only from the internet still needs a cross-node address for the proxy,
    /// so a public network falls back to its own overlay.
    #[test]
    fn a_public_only_stack_still_gets_its_own_overlay() {
        let cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");
        let public_only =
            StackFile::parse("networks:\n  public: {}\n  wan: {}\nservices: {}\n").unwrap();
        assert_eq!(overlay_network_name(&public_only, &cfg), Some("public"));
    }

    /// A stack declaring both keeps the internal/private network (its domain is what containers
    /// resolve on); a public network is only a fallback.
    #[test]
    fn an_internal_network_is_preferred_over_a_public_one() {
        let cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");
        let both =
            StackFile::parse("networks:\n  public: {}\n  tailscale: {}\nservices: {}\n").unwrap();
        assert_eq!(overlay_network_name(&both, &cfg), Some("tailscale"));
    }

    #[test]
    fn overlay_collision_catches_underscore_ambiguity() {
        let mut cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");
        let public = cfg.networks.get("public").unwrap().clone();
        cfg.networks.insert("b_public".into(), public);
        let on = |net: &str| {
            StackFile::parse(&format!("networks:\n  {net}: {{}}\nservices: {{}}\n")).unwrap()
        };
        let p = on("public");
        let q = on("b_public");

        // Stack `a_b` on `public` and stack `a` on `b_public` both want
        // `a_b_public`.
        assert_eq!(
            overlay_collision(&cfg, [("a_b", &p), ("a", &q)]),
            Some(("a_b_public".into(), "a_b".into(), "a".into()))
        );
        // Distinct stacks on one network, and a stack against itself, are fine.
        assert_eq!(overlay_collision(&cfg, [("a", &p), ("b", &p)]), None);
        assert_eq!(overlay_collision(&cfg, [("a", &q), ("a", &q)]), None);
    }

    fn state() -> ClusterState {
        let mut state = ClusterState::default();
        state.apply(&Request::UpsertNode(node("n1", &["fast"])));
        state.apply(&Request::UpsertNode(node("n2", &["fast"])));
        state.apply(&Request::DeployStack {
            name: "site".into(),
            spec: STACK.into(),
            at: 0,
        });
        state
    }

    #[test]
    fn a_deployed_stack_produces_placements() {
        let requests = plan_requests(&state()).expect("plan");
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| matches!(r, Request::Assign { .. })));
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
            at: 0,
        });
        // The good stack still schedules.
        let requests = plan_requests(&state).expect("plan");
        assert_eq!(requests.len(), 2);
    }

    /// During emergency mode only the named stack is a placement candidate:
    /// another deployed stack must not schedule anywhere, not even on the
    /// sole surviving node, until an operator ends emergency mode.
    #[test]
    fn emergency_mode_only_plans_the_named_stack() {
        let mut state = state();
        state.apply(&Request::DeployStack {
            name: "other".into(),
            spec: STACK.into(),
            at: 0,
        });
        state.emergency_stacks = ["site".to_string()].into();

        let requests = plan_requests(&state).expect("plan");
        assert_eq!(requests.len(), 2, "only site's two replicas");
        assert!(requests.iter().all(|r| match r {
            Request::Assign { instance, .. } => instance.stack == "site",
            _ => false,
        }));
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
            at: 0,
        });
        for request in plan_requests(&state).expect("plan") {
            assert!(state.apply(&request).is_ok());
        }
        state
    }

    #[test]
    fn a_placed_volume_is_registered_once() {
        let mut state = stateful_state();
        let cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");
        let requests = volume_requests(&state, &cfg);
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0],
            Request::UpsertVolume { id, .. } if id == &VolumeId::new("fast", "data")
        ));

        for request in requests {
            assert!(state.apply(&request).is_ok());
        }
        // A registered volume whose primary already is the instance's node
        // needs nothing: no raft entry per volume per sweep, forever.
        assert!(volume_requests(&state, &cfg).is_empty());
    }

    /// After a failover the volume's primary has to follow the instance; otherwise the node it
    /// left stays the sync source and overwrites the live copy when it returns.
    #[test]
    fn a_volumes_primary_follows_an_instance_that_failed_over() {
        let mut state = stateful_state();
        let cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");
        for request in volume_requests(&state, &cfg) {
            assert!(state.apply(&request).is_ok());
        }

        let id = VolumeId::new("fast", "data");
        let instance = state
            .assignments
            .keys()
            .next()
            .expect("one assignment")
            .clone();
        let from = state.volumes[&id].primary.clone();
        let to = if from == "n1" { "n2" } else { "n1" };

        // The failover itself: the primary went away and placement moved the
        // instance to the other node holding a copy.
        state.apply(&Request::SetVolumeReplica {
            id: id.clone(),
            node: to.into(),
            generation: state.volumes[&id].generation,
        });
        state.apply(&Request::Assign {
            instance,
            node: to.into(),
            drives: ["fast".to_string()].into(),
        });

        let requests = volume_requests(&state, &cfg);
        assert!(
            matches!(&requests[..], [Request::TouchVolume { id: got, primary }] if got == &id && primary == to),
            "expected the primary to move to {to}, got {requests:?}"
        );

        for request in requests {
            assert!(state.apply(&request).is_ok());
        }
        assert_eq!(state.volumes[&id].primary, to);
        // And it settles: nothing to say once the primary has caught up.
        assert!(volume_requests(&state, &cfg).is_empty());
    }

    /// A served drive's volume is registered against the drive's active server, not the
    /// instance's node (`check_placement` only waives the drive requirement for the real primary).
    #[test]
    fn a_served_volumes_primary_is_the_active_server_not_the_assignment() {
        const SERVED_CLUSTER: &str = r#"
cluster: { name: home }
networks:
  tailscale: { type: internal, subnet: 10.10.0.0/24, domain: int.example.com }
drives:
  fast: { mode: served, served_by: [n2, n3] }
nodes:
  n1:
    id: 3b0f6c4a-3d3f-4a8e-9b8b-2c1e6a2a9f01
    role: manager
    network: { tailscale: 10.10.0.1 }
    drives: {}
  n2:
    id: 4c1f7d5b-4e4f-4b9f-9c9c-3d2f7b3a0f02
    role: worker
    network: { tailscale: 10.10.0.2 }
    drives: { fast: /srv/fast }
  n3:
    id: 5d2f8e6c-5f5f-4caf-adad-4e3f8c4b1f03
    role: worker
    network: { tailscale: 10.10.0.3 }
    drives: { fast: /srv/fast }
web: { address: cloud, network: tailscale }
"#;
        let cfg = ClusterConfig::parse(SERVED_CLUSTER).expect("valid cluster config");
        let mut state = ClusterState::default();
        // n1 implements nothing; the instance still lands here, since a
        // served drive never requires that (`cl_sched::place::rejection`).
        state.apply(&Request::UpsertNode(node("n1", &[])));
        state.apply(&Request::UpsertNode(node("n2", &["fast"])));
        state.apply(&Request::UpsertNode(node("n3", &["fast"])));
        state.apply(&Request::DeployStack {
            name: "app".into(),
            spec: STATEFUL.into(),
            at: 0,
        });
        // `check_placement` reads the *replicated* config, not the `cfg`
        // this test hands `volume_requests` directly, so the served-drive
        // carve-out needs it published here too or the assign below is
        // rejected and `state.assignments` stays empty.
        assert!(state
            .apply(&Request::PutClusterConfig {
                yaml: SERVED_CLUSTER.into(),
                expect_generation: 0,
            })
            .is_ok());
        assert!(state
            .apply(&Request::Assign {
                instance: InstanceId::new("app", "db", 0),
                node: "n1".into(),
                drives: ["fast".to_string()].into(),
            })
            .is_ok());

        let requests = volume_requests(&state, &cfg);
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0],
            Request::UpsertVolume { id, primary } if id == &VolumeId::new("fast", "data") && primary == "n2"
        ));
        for request in &requests {
            assert!(
                state.apply(request).is_ok(),
                "primary must satisfy check_placement too"
            );
        }
    }

    fn service_with_volumes(volumes: &str) -> cl_config::Service {
        let stack = StackFile::parse(&format!(
            "services:\n  web:\n    image: nginx\n    volumes:\n{volumes}\n"
        ))
        .expect("valid stack");
        stack.services["web"].clone()
    }

    /// A container created while its drive was FUSE-mounted keeps a stale bind path unless a
    /// failover that leaves the stack file unchanged also moves the revision `diff` compares.
    #[test]
    fn a_drive_switching_between_local_and_fuse_mounted_changes_the_revision() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let local = revision_with_mounts(1, &service, &BTreeSet::new(), None, &[]);
        let fuse_mounted = revision_with_mounts(
            1,
            &service,
            &BTreeSet::from(["fast".to_string()]),
            None,
            &[],
        );
        assert_ne!(local, fuse_mounted);
    }

    /// A drive this service doesn't even mount switching between local and
    /// FUSE-mounted must not force every other container to be recreated.
    #[test]
    fn an_unrelated_drive_switching_does_not_change_the_revision() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let a = revision_with_mounts(1, &service, &BTreeSet::new(), None, &[]);
        let b = revision_with_mounts(
            1,
            &service,
            &BTreeSet::from(["slow".to_string()]),
            None,
            &[],
        );
        assert_eq!(a, b);
    }

    /// The stack's own revision must still move the combined value, or redeploys go unnoticed.
    #[test]
    fn the_stack_revision_still_changes_the_combined_value() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let dynamic = BTreeSet::from(["fast".to_string()]);
        assert_ne!(
            revision_with_mounts(1, &service, &dynamic, None, &[]),
            revision_with_mounts(2, &service, &dynamic, None, &[])
        );
    }

    /// A new overlay address (a redeploy's replanning released and reallocated it) must recreate
    /// the container, else it keeps the old address and the proxy gets a permanent `bad gateway`.
    #[test]
    fn an_instance_getting_a_new_overlay_address_changes_the_revision() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let dynamic = BTreeSet::new();
        let none = revision_with_mounts(1, &service, &dynamic, None, &[]);
        let a = revision_with_mounts(
            1,
            &service,
            &dynamic,
            Some("10.244.0.23".parse().unwrap()),
            &[],
        );
        let b = revision_with_mounts(
            1,
            &service,
            &dynamic,
            Some("10.244.0.30".parse().unwrap()),
            &[],
        );
        assert_ne!(none, a);
        assert_ne!(a, b);
    }

    /// A client's own container-network address is part of its revision; the router's address and
    /// routes are not, so a router appearing or moving never recreates a client.
    #[test]
    fn only_the_clients_own_container_address_changes_the_revision() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let dynamic = BTreeSet::new();
        let none = revision_with_mounts(1, &service, &dynamic, None, &[]);
        let a: std::net::Ipv4Addr = "10.246.0.5".parse().unwrap();
        let b: std::net::Ipv4Addr = "10.246.0.6".parse().unwrap();
        let with_a = revision_with_mounts(1, &service, &dynamic, None, &[a]);
        assert_ne!(none, with_a);
        assert_ne!(
            with_a,
            revision_with_mounts(1, &service, &dynamic, None, &[b])
        );
        assert_eq!(
            with_a,
            revision_with_mounts(1, &service, &dynamic, None, &[a])
        );
    }

    #[test]
    fn a_stack_with_no_volumes_registers_nothing() {
        let cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");
        assert!(volume_requests(&state(), &cfg).is_empty());
    }

    #[test]
    fn three_restarts_within_the_window_trip_the_crash_loop() {
        let mut deque = VecDeque::new();
        let base = Instant::now();
        assert!(!record_restart(&mut deque, base));
        assert!(!record_restart(&mut deque, base + Duration::from_secs(10)));
        assert!(record_restart(&mut deque, base + Duration::from_secs(20)));
    }

    fn plain_node(name: &str) -> NodeState {
        NodeState {
            name: name.to_string(),
            id: uuid::Uuid::new_v4(),
            role: cl_raft::state::Role::Worker,
            rpc_addr: "10.10.0.1:7373".parse().expect("addr"),
            drives: Default::default(),
            availability: cl_raft::state::Availability::Active,
            labels: Default::default(),
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: Default::default(),
            binary_hash: Default::default(),
            node_pubkey: Default::default(),
            memory_total_bytes: Default::default(),
            gpus: Default::default(),
            images: Default::default(),
            version: Default::default(),
        }
    }

    #[test]
    fn crash_move_targets_skip_recently_tried_nodes_until_all_are_exhausted() {
        let nodes = [plain_node("a"), plain_node("b"), plain_node("c")];
        let load = |_: &str| 0usize;
        let now = 10_000;

        // On `a`, nothing tried yet: `b` (first least-loaded) is picked.
        let target = pick_crash_move_target(nodes.iter(), "a", None, now, load).unwrap();
        assert_eq!(target.name, "b");

        // Now on `b`, having left `a`: only `c` remains.
        let history = BTreeMap::from([("a".to_string(), now - 10)]);
        let target = pick_crash_move_target(nodes.iter(), "b", Some(&history), now, load).unwrap();
        assert_eq!(target.name, "c");

        // On `c`, having left `a` and `b`: exhausted, stay put.
        let history = BTreeMap::from([("a".to_string(), now - 20), ("b".to_string(), now - 10)]);
        assert!(pick_crash_move_target(nodes.iter(), "c", Some(&history), now, load).is_none());

        // Entries older than the window are forgiven.
        let later = now + CRASH_MOVE_RETENTION_SECS + 1;
        let target =
            pick_crash_move_target(nodes.iter(), "c", Some(&history), later, load).unwrap();
        assert_eq!(target.name, "a");
    }

    #[test]
    fn restart_backoff_doubles_up_to_the_cap_and_is_eventually_forgotten() {
        let base = Instant::now();
        let mut backoff = RestartBackoff::new(base);
        backoff.record_loop(base);
        assert_eq!(backoff.next_start, base + CRASH_BACKOFF_BASE);
        backoff.record_loop(base);
        assert_eq!(backoff.next_start, base + CRASH_BACKOFF_BASE * 2);
        for _ in 0..100 {
            backoff.record_loop(base);
        }
        assert_eq!(backoff.next_start, base + CRASH_BACKOFF_MAX);

        assert!(!backoff.is_forgettable(backoff.next_start + CRASH_BACKOFF_FORGET_AFTER));
        assert!(backoff.is_forgettable(
            backoff.next_start + CRASH_BACKOFF_FORGET_AFTER + Duration::from_secs(1)
        ));
    }

    #[test]
    fn restarts_spread_outside_the_window_never_accumulate() {
        let mut deque = VecDeque::new();
        let base = Instant::now();
        assert!(!record_restart(&mut deque, base));
        assert!(!record_restart(
            &mut deque,
            base + CRASH_LOOP_WINDOW + Duration::from_secs(1)
        ));
        // The first restart aged out, so this is only the second live one.
        assert!(!record_restart(
            &mut deque,
            base + CRASH_LOOP_WINDOW + Duration::from_secs(2)
        ));
        assert_eq!(deque.len(), 2);
    }
}

#[cfg(test)]
mod container_router_tests {
    use super::*;
    use cl_raft::state::Request;

    #[test]
    fn a_router_prefers_its_clients_and_follows_them_in_order() {
        let yaml = include_str!("../../../examples/cluster.yaml").replace(
            "stack: tailscale-router\n      service: tailscale",
            "stack: rt\n      service: router",
        );
        let cfg = ClusterConfig::parse(&yaml).unwrap();
        let mut state = ClusterState::default();
        for (name, spec) in [
            ("rt", "services:\n  router:\n    image: x\n"),
            (
                "site",
                "networks:\n  tailnet: {}\nservices:\n  web:\n    image: x\n    networks: [tailnet]\n  db:\n    image: x\n",
            ),
        ] {
            state.apply(&Request::DeployStack {
                name: name.into(),
                spec: spec.into(),
                at: 0,
            });
        }
        let desired = desired_instances(&state, &BTreeSet::new(), &BTreeSet::new(), false);
        let desired = with_container_routers(desired, &state, &cfg);
        let router = desired
            .iter()
            .position(|d| d.id.service == "router")
            .unwrap();
        assert_eq!(router, desired.len() - 1, "routers come last");
        assert_eq!(
            desired[router].near,
            BTreeSet::from([("site".to_string(), "web".to_string())])
        );
        assert!(desired
            .iter()
            .filter(|d| d.id.service != "router")
            .all(|d| d.near.is_empty()));
    }
}
