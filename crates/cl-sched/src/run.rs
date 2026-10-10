//! The scheduler loop.
//!
//! Two jobs on one timer. The leader decides where everything should run and
//! writes those decisions through raft. Every node, leader or not, then makes
//! its own docker match the decisions that name it. Splitting it this way means
//! a node never waits on the leader to start a container it has already been
//! given, and the leader never has to reach into anyone else's docker.

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

/// How often to sweep, on top of reacting to state changes. A periodic pass
/// catches drift docker never told us about, like a container someone stopped
/// by hand.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// How many times a container needs restarting within `CRASH_LOOP_WINDOW`
/// before this node gives up on running it in place and asks the leader to
/// move it elsewhere. Three catches a genuine loop (an OOM kill, a crash on
/// startup) within a minute or so while still giving a single transient
/// failure - a dockerd blip, a slow pull racing the sweep - room to recover
/// on its own.
const CRASH_LOOP_THRESHOLD: usize = 3;

/// The window `CRASH_LOOP_THRESHOLD` is counted over.
const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(120);

/// First delay between in-place restarts once an instance has crash-looped
/// everywhere it can run (see `try_reschedule_away`), doubled for each further
/// loop up to `CRASH_BACKOFF_MAX`. Without it such an instance is restarted
/// every sweep forever, which for a container that dies on start is a busy
/// loop of docker calls, pulls and log noise.
const CRASH_BACKOFF_BASE: Duration = Duration::from_secs(30);

/// Cap on the in-place restart delay.
const CRASH_BACKOFF_MAX: Duration = Duration::from_secs(10 * 60);

/// An in-place restart backoff is forgotten once this long has passed after
/// it expired without the instance needing another restart, i.e. it has
/// stayed up long enough to be considered healthy again.
const CRASH_BACKOFF_FORGET_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long a finished (started or failed) update stays in the replicated
/// record, so whoever started it can still see how it ended.
const FINISHED_UPDATE_TTL_SECS: i64 = 15 * 60;

/// How long an update may sit in one unfinished phase before it is presumed
/// abandoned: queued by a node that went away before getting to it, or
/// waiting on a container the reconciler never got to create.
const STALLED_UPDATE_TTL_SECS: i64 = 60 * 60;

/// How long an update may sit `Queued`. A rolling batch queues everything
/// before starting, so a long batch legitimately waits, but 15 minutes is
/// beyond what one instance's turn takes (`UPDATE_FINISH_TIMEOUT` in the api
/// bounds each earlier one).
const QUEUED_UPDATE_TTL_SECS: i64 = 15 * 60;

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
    /// Recent `Action::Start` timestamps per instance this node has had to
    /// restart. Node-local and in-memory only, never persisted or shared:
    /// enough to notice a container this node keeps failing to keep running
    /// (an OOM kill, a crash on start) without needing a raft round trip just
    /// to count it. See `note_restart_and_check_crash_loop`.
    restarts: Arc<RwLock<BTreeMap<InstanceId, VecDeque<Instant>>>>,
    /// In-place restart delays for instances that crash-looped on every node
    /// they could be moved to. Node-local like `restarts`; losing it on a
    /// daemon restart merely restarts the backoff from its first step.
    backoffs: Arc<RwLock<BTreeMap<InstanceId, RestartBackoff>>>,
    /// Unix seconds this scheduler was built, i.e. roughly when this daemon
    /// started. An in-flight update on this node older than that was being
    /// driven by a process that no longer exists.
    booted_at: i64,
    /// Rung when the fence releases, so a node that just regained quorum
    /// starts its containers now instead of waiting out the sweep interval.
    wake: Arc<tokio::sync::Notify>,
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
            restarts: Arc::new(RwLock::new(BTreeMap::new())),
            backoffs: Arc::new(RwLock::new(BTreeMap::new())),
            booted_at: unix_now(),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Share the handle that cuts the wait for the next sweep short.
    pub fn with_wake(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.wake = wake;
        self
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

    /// Run until cancelled. `fence` reports whether this node has lost quorum
    /// long enough to give up its containers — `None` when that has not been
    /// determined yet (e.g. the first sweep after this node's own restart,
    /// which can easily land before the fence's own first tick). A pass that
    /// lands during `None` does nothing at all: guessing "fenced" would stop
    /// every container this node already correctly owns, only for the next
    /// sweep to start them all back up once the fence catches up. A pass
    /// during `Hold` does nothing either, for the leader side as well.
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

    /// Leader side: work out where everything belongs and record it.
    async fn schedule(&self, state: &ClusterState) -> crate::Result<()> {
        let live = self.live_config(state);
        let colocate_proxy_hosts = live.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
        let desired = desired_instances(
            state,
            &live.egress_networks(),
            &live.port_gated_networks(),
            colocate_proxy_hosts,
        );

        let outcome = place::place(state, &desired, &live.served_drives(), &live.nat_networks());
        for unplaceable in &outcome.unplaceable {
            warn!(instance = %unplaceable.instance, reason = %unplaceable.reason, "cannot place instance");
        }

        for decision in outcome.decisions {
            // `submit` forwards to whichever node is leader, so a pass that
            // outlives this node's leadership would otherwise keep writing
            // decisions computed from its own, now stale, view of the state.
            if !self.cluster.is_leader() {
                warn!("lost leadership mid-pass, abandoning the scheduling pass");
                return Ok(());
            }
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

        // Re-read: `state` is from before the placements above were submitted,
        // and `volume_requests` decides where a volume's primary belongs from
        // the assignments. Working off the stale copy makes it chase the node
        // each instance was on a moment ago, which is one step behind for
        // exactly one sweep - long enough to hand the primary back and forth
        // with placement forever, stopping and starting the containers each
        // time round.
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

    /// Node side: make docker match the assignments naming this node.
    async fn reconcile(
        &self,
        state: &ClusterState,
        gate: FenceState,
        still_fenced: &(dyn Fn() -> bool + Sync),
    ) -> crate::Result<()> {
        let fenced = gate == FenceState::Closed;
        // No docker to reconcile against yet (or right now) is not an error:
        // this pass is simply skipped, and the next one tries again once
        // `docker` is reconnected.
        let Some(docker) = self.docker.read().await.clone() else {
            return Ok(());
        };
        // Opened fresh each pass: see `Reconciler::registries`. A node with
        // no cluster key yet gets none, and pulls anonymously — the same
        // thing it did before logins existed, rather than failing outright.
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

        // `depends_on`: hold back the first creation of an instance until the
        // services it depends on are up. Only `Create` is gated, so an existing
        // container is never stopped or blocked by a dependency going down.
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

        // An instance that crash-looped everywhere it can run is restarted in
        // place on a growing delay instead of every sweep. Dropped before the
        // crash-loop bookkeeping below so a deferred start is not counted as a
        // restart that never happened.
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

        // An instance this node keeps having to restart is never fixed by
        // restarting it again: ask the leader to move it somewhere else.
        // Checked before `apply` runs the actual restart, but the restart
        // still happens either way - a rejected or unsubmittable move must
        // not leave the instance down for no reason.
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
                // Nowhere better to go: restart in place, but less eagerly
                // each time round.
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
            .apply(&actions, &targets, &actual, fencing)
            .await?;
        self.report_container_status(state, &actual).await;
        self.expire_updates(state, &actual).await;

        // Objects for instances that have moved on must not linger on disk.
        let keep: BTreeMap<String, ()> = targets
            .iter()
            .map(|t| (t.assignment.instance.container_name(), ()))
            .collect();
        if let Err(e) = self.materializer.clean_orphans(&keep) {
            warn!(error = %e, "could not clean orphaned objects");
        }

        // A writable config a container changed since we last mounted it:
        // feed it back through raft, same path anyone else's edit takes.
        // `submit` forwards to the leader on its own, so this runs the same
        // way whether or not this node happens to be it.
        for edit in config_edits {
            let request = Request::PutConfig {
                name: edit.name.clone(),
                content: base64_encode(&edit.content),
                owner: edit.owner,
            };
            match self.cluster.submit(request).await {
                Ok(response) => {
                    if let Some(error) = response.error {
                        warn!(config = %edit.name, %error, "container's edit to a config was rejected");
                    } else {
                        info!(config = %edit.name, "wrote a container's config edit back to the cluster");
                    }
                }
                Err(e) => {
                    warn!(config = %edit.name, error = %e, "could not submit a container's config edit")
                }
            }
        }

        Ok(())
    }

    /// Show the operator why an instance was not started: a failed update
    /// record carrying the reason, the one per-instance error channel the UI
    /// already renders. Skipped when the same failure is already recorded, so
    /// a steady refusal costs one raft write rather than one per sweep.
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

    /// Self-report this node's running/stopped state for every instance
    /// assigned to it, so anyone elsewhere waiting for one of them to stop
    /// (a backup, a restore, a folder import) can poll `container_status`
    /// instead of blindly sleeping out a worst-case deadline. Skipped when
    /// nothing changed since the last report, so a quiet node doesn't spam
    /// raft with an identical write every sweep.
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

        let report: BTreeMap<InstanceId, bool> = state
            .assignments_on(&self.node)
            .map(|a| {
                (
                    a.instance.clone(),
                    running.get(&a.instance).copied().unwrap_or(false),
                )
            })
            .collect();

        let current: BTreeMap<InstanceId, bool> = state
            .container_status
            .iter()
            .filter(|(_, status)| status.node == self.node)
            .map(|(id, status)| (id.clone(), status.running))
            .collect();

        if report == current {
            return;
        }
        if let Err(e) = self
            .cluster
            .submit(Request::ReportContainerStatus {
                node: self.node.clone(),
                running: report,
            })
            .await
        {
            warn!(error = %e, "could not report container status");
        }
    }

    /// Tidy this node's entries in `instance_updates`: drop finished ones once
    /// they have been visible long enough, and fail the ones nothing is
    /// driving any more, so a daemon restart mid-update cannot leave a
    /// progress bar spinning forever.
    ///
    /// Also the safety net for an update whose new container came up without
    /// the reconciler recording it (a sweep that raced the update's own
    /// writes): at `Starting` the old container is already gone, so any
    /// running one is the new one.
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

    /// Leader only: settle update records no live process is driving. The
    /// owning node's own sweep handles its records, but that cannot help when
    /// the owner is gone, unreachable, or no longer the instance's node (the
    /// instance moved), nor a `Queued` record whose driving api node
    /// restarted. Left alone they stay non-terminal for an hour and
    /// `apply_selection` refuses to start another update on the instance.
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

    /// Ask the leader to move a crash-looping instance to a different
    /// eligible node, the same `BeginMigration` request a manual move or
    /// `clouduler rebalance` submits - so a redeploy or a config edit racing
    /// this is rejected exactly where a manual move would be.
    ///
    /// Skipped (restarting in place is all that is left) for an instance
    /// that is pinned, global, or anchored to another instance by a shared
    /// proxy host or `x-clouduler.colocate` group, the same exclusions
    /// `rebalance::plan` applies: none of those can safely move to just any
    /// other eligible node on their own. Also skipped if a migration is
    /// already under way, so a slow move in progress is not raced by a
    /// second one.
    ///
    /// Nodes this instance was already moved away from within
    /// `CRASH_MOVE_RETENTION_SECS` (`ClusterState::crash_moves`) are not
    /// candidates, so a service that crashes everywhere tries each node once
    /// and then stays put instead of hopping between them forever. Returns
    /// whether a move was submitted; `false` means the caller restarts in
    /// place.
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
        let Some(item) = desired.iter().find(|d| &d.id == id) else {
            return false;
        };

        if item.global || item.pinned_node.is_some() || item.anchor_keys().next().is_some() {
            warn!(instance = %id, "keeps restarting here but cannot be safely moved (pinned, global, or anchored), restarting in place");
            return false;
        }

        let Ok((eligible, _)) =
            place::candidates(state, item, &live.served_drives(), &live.nat_networks())
        else {
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
                    // Remember where it crashed so the next owner does not
                    // send it straight back. Submitted after the move so a
                    // rejected move leaves no history; losing this write only
                    // loses the memory, never the move.
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

    /// Build what this node should be running, materializing each instance's
    /// configs and secrets on the way. Also returns any edit a container
    /// made to one of its writable config mounts, for the caller to feed
    /// back into the cluster.
    fn targets(&self, state: &ClusterState) -> crate::Result<Targets> {
        let live = self.live_config(state);
        let (drive_roots, dynamic_mounts) = self.drive_roots(state, &live);

        // Decoded once per pass rather than per instance: every container on
        // this node reads out of the same address book.
        let allocations = Allocations::decode(state.overlay.as_deref());
        let egress_networks = live.egress_networks();

        let mut targets = Vec::new();
        let mut config_edits = Vec::new();
        let mut refused = Vec::new();

        for assignment in state.assignments_on(&self.node) {
            let instance = &assignment.instance;
            // During emergency mode (see `clouduler emergency`), a container
            // this node was already running for some other stack before it
            // lost quorum must not keep running: its `desired` was never
            // touched, so leaving it in `targets` here would restart it
            // untouched the moment this loop runs, defeating the whole point
            // of asking an operator to confirm what else is really up first.
            // Simply excluding it (rather than forcing `desired` to
            // `Stopped`) is what lets it resume on its own, no restore step
            // needed, the instant `emergency_stack` clears.
            if state
                .emergency_stack
                .as_deref()
                .is_some_and(|only| only != instance.stack)
            {
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

            // Node addresses on the internal network, where the proxy answers
            // for routes published on it. Only a service that is a member of
            // that network gets them kept off the egress default route.
            let (internal_name, _) = live.internal_network();
            if service.networks.iter().any(|n| n == internal_name) {
                placement.proxy_addresses = live
                    .nodes
                    .values()
                    .filter_map(|node| match live.internal_ip(node) {
                        Some(std::net::IpAddr::V4(ip)) => Some(ip),
                        _ => None,
                    })
                    .collect();
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

            let revision =
                revision_with_mounts(stack.revision, service, &dynamic_mounts, placement.ip);

            let spec = match spec::build(
                instance,
                service,
                spec::Materialization {
                    objects: &objects.mounts,
                    env: &objects.env,
                    drive_roots: &drive_roots,
                },
                placement,
                assignment.image_digest.as_deref(),
                revision,
            ) {
                Ok(spec) => spec,
                Err(e) => {
                    // Deterministic, so left out of `targets` entirely: no
                    // `Start` is planned, which keeps it out of the
                    // crash-loop counter and away from `try_reschedule_away`
                    // (another node would refuse the same mount). The
                    // operator sees it through `refused`.
                    error!(instance = %instance, error = %e, "refusing to start an instance whose spec is invalid");
                    refused.push((instance.clone(), e.to_string()));
                    continue;
                }
            };

            targets.push(Target {
                assignment,
                spec,
                revision,
                stop_grace: effective_stop_grace(service, live.failover.stop_grace),
            });
        }

        Ok((targets, config_edits, refused))
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

/// Folds which of a service's drives are currently FUSE-mounted (rather than
/// backed by this node's own local copy), and the instance's own overlay
/// address, into the revision used to detect drift, so a container gets
/// recreated on top of `stack.revision` changing.
///
/// A served drive's active server can change after a container already
/// exists (the previously-active candidate failed, or came back and this
/// node handed the role back) without `stack.revision` moving at all — the
/// stack file didn't change, only which host path answers for the drive did.
/// `diff` only compares `LABEL_REVISION`, so without this, a container
/// created while its drive was FUSE-mounted keeps that bind source baked in
/// forever: `Action::Start` never recreates it, and the mountpoint it points
/// at is only maintained by `clouduler::drives::watch_volume_mounts` while
/// this node still needs it as a FUSE mount, which stops being true the
/// moment this node becomes the active server. The result is a container
/// stuck failing to start against a bind path nothing owns any more.
///
/// The overlay address has the same shape of problem: `cl_net::ipam` frees
/// an instance's address and hands out a fresh one whenever the instance
/// briefly drops out of the assignment table (a redeploy's replanning pass,
/// say), even when the container backing it was never touched — `diff` only
/// compares `LABEL_REVISION`, so a container built with the old address keeps
/// running under it forever while every other node's proxy and internal DNS
/// have already moved on to the new one. Confirmed live: a stack redeploy
/// left an already-running, perfectly healthy container answering on an
/// address the reverse proxy no longer knew about, so every request that
/// landed on it got a permanent `bad gateway`, indistinguishable from the
/// backend actually being down.
///
/// Not reversible bit-packing (an IPv4 address alone needs all 32 bits a
/// per-drive flag set was previously borrowing) — hashed together instead,
/// since nothing reads this value apart from testing it for equality.
fn revision_with_mounts(
    revision: u64,
    service: &cl_config::Service,
    dynamic_mounts: &BTreeSet<String>,
    ip: Option<std::net::Ipv4Addr>,
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
    hasher.finish()
}

/// The node currently serving a `Served` drive: the first of `served_by` that
/// is not reported unreachable. Mirrors `cl_drive::registry::DriveRegistry`'s
/// `active_server`, computed independently here since placement/mount
/// resolution only need the answer, not the rest of that crate.
fn active_server<'a>(
    drive: &'a cl_config::Drive,
    unreachable: &BTreeSet<String>,
) -> Option<&'a str> {
    drive
        .served_by
        .iter()
        .map(String::as_str)
        .find(|n| !unreachable.contains(*n))
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
/// The primary registered is the node that actually holds the data: for a
/// replicated drive that's wherever the instance landed (the only place with
/// a local copy), but for a served drive it's the drive's own active server
/// — an instance using a served drive can land on any node, most of which
/// hold no local copy at all, and the raft state machine's own
/// `check_placement` only waives the "must implement this drive" rule for a
/// served drive's real primary, not for an arbitrary assignment node.
///
/// # Following the instance
///
/// A replicated volume already on record also gets a `TouchVolume` whenever
/// its primary is no longer the node running the instance that mounts it.
///
/// Placement prefers the primary (`place::sole_primary`), so this only fires
/// after a real failover: the primary went unreachable, the instance moved to
/// a node holding a replica, and the volume has to follow it. Until it does,
/// the sync scheduler still treats the node that just went away as the source
/// and, the moment it comes back, syncs its stale copy over the live one —
/// deleting files the running container is using, reverting its writes, and
/// doing it again every interval, forever. That is not a corner case: every
/// rolling restart of the fleet makes each node briefly unreachable.
///
/// Deliberately not done for a served drive (its primary is the active
/// server, which has nothing to do with where instances run) or for an
/// instance mid-migration (`cl_drive::migration` hands the primary over
/// itself, at `Switching`, and must not be raced).
pub fn volume_requests(state: &ClusterState, live: &ClusterConfig) -> Vec<Request> {
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
                Some(cfg) if served => active_server(cfg, &state.unreachable).map(str::to_string),
                _ => Some(assignment.node.clone()),
            };
            let Some(primary) = primary else {
                // Every candidate is unreachable right now; try again next
                // sweep once one comes back.
                continue;
            };
            requests.push(Request::UpsertVolume { id, primary });
        }
    }
    requests
}

/// Pushes `now` and reports whether `CRASH_LOOP_THRESHOLD` restarts now sit
/// within `CRASH_LOOP_WINDOW` of it, pruning anything older than the window
/// on the way so a deque tracking a long-lived instance never grows past
/// what the window can hold. Pulled out of `Scheduler::note_restart_and_check_crash_loop`
/// as a pure function so the counting logic is testable without a running
/// cluster.
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

/// Picks the least-loaded node to move a crash-looping instance to: not the
/// node it is on, and not one it was moved away from within the retention
/// window. `None` once every eligible node has been tried.
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

    /// Another crash loop was detected: double the delay (capped) before the
    /// next in-place restart.
    fn record_loop(&mut self, now: Instant) {
        let delay = CRASH_BACKOFF_BASE
            .saturating_mul(1u32.checked_shl(self.loops).unwrap_or(u32::MAX))
            .min(CRASH_BACKOFF_MAX);
        self.loops = self.loops.saturating_add(1);
        self.next_start = now + delay;
    }

    /// Whether the instance has stayed up long enough since the backoff
    /// expired that it should start from the first step again.
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

/// The one declared network a stack should get a VXLAN overlay built for, if
/// any. An internal or private network wins when the stack declares one,
/// since that is the network whose domain its containers should actually
/// resolve on. Failing that, a public network still needs its containers
/// reachable cross-node for the proxy to forward to them, so it falls back to
/// getting its own per-stack overlay too - isolated to this stack alone, the
/// same as any other overlay, so this grants no access to whatever internal
/// or private network other stacks happen to share.
///
/// A stack whose networks are all egress or wireguard needs no overlay at
/// all, and must not be made to wait forever for an address on one of those
/// instead.
pub fn overlay_network_name<'a>(file: &'a StackFile, cfg: &ClusterConfig) -> Option<&'a str> {
    let kind_of = |n: &str| cfg.networks.get(n).map(|net| net.kind);

    file.networks
        .keys()
        .find(|n| kind_of(n).is_some_and(NetworkKind::is_subnetted))
        .or_else(|| {
            file.networks
                .keys()
                .find(|n| kind_of(n) == Some(NetworkKind::Public))
        })
        .map(String::as_str)
}

/// The first pair of distinct stacks whose overlays share a docker network
/// name, as `(overlay, stack, other stack)`.
///
/// `overlay_name` joins with `_` and neither stack nor network names forbid
/// it, so stack `a_b` on network `c` and stack `a` on network `b_c` both map
/// to `a_b_c`: one IPAM allocation, one bridge, no isolation between them.
/// The names can't change (live clusters already have them), so a deploy or
/// a network rename that would create the collision is refused instead.
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
///
/// While `state.emergency_stack` holds a name (see `clouduler emergency`),
/// every other stack is left out entirely, rather than reported unplaceable:
/// its instances are not a placement problem, they are simply not wanted
/// until an operator ends emergency mode. Nothing here mutates any
/// assignment's own `desired` state, so once `emergency_stack` clears (see
/// `Request::ClearEmergency`) every stack picks back up exactly where its own
/// `desired` field already said it should be.
pub fn desired_instances(
    state: &ClusterState,
    egress_networks: &BTreeSet<String>,
    port_gated_networks: &BTreeSet<String>,
    colocate_proxy_hosts: bool,
) -> Vec<Desired> {
    let mut desired = Vec::new();
    for (name, stack) in &state.stacks {
        if state
            .emergency_stack
            .as_deref()
            .is_some_and(|only| only != name)
        {
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

/// A service's own `stop_grace_period` when set, else the cluster default.
fn effective_stop_grace(
    service: &cl_config::Service,
    default: std::time::Duration,
) -> std::time::Duration {
    service.stop_grace_period.unwrap_or(default)
}

/// `failover.stop_grace` from the replicated cluster config: how long a
/// container without its own `stop_grace_period` gets to stop. The built-in
/// default if no config has been published yet or it fails to parse.
pub fn configured_stop_grace(state: &ClusterState) -> std::time::Duration {
    state
        .cluster_config
        .as_ref()
        .and_then(|entry| ClusterConfig::parse(&entry.yaml).ok())
        .map(|cfg| cfg.failover.stop_grace)
        .unwrap_or(cl_config::DEFAULT_STOP_GRACE)
}

/// Nat-kind network names, read from the replicated cluster config the same
/// way `served_drives` is. Empty (falling back to strict per-node network
/// eligibility) if no config has been published yet or it fails to parse.
fn nat_networks(state: &ClusterState) -> BTreeSet<String> {
    state
        .cluster_config
        .as_ref()
        .and_then(|entry| ClusterConfig::parse(&entry.yaml).ok())
        .map(|cfg| cfg.nat_networks())
        .unwrap_or_default()
}

/// Every request the leader would submit for a state, without submitting them.
/// Exists so the scheduling decision can be tested without a cluster.
pub fn plan_requests(state: &ClusterState) -> crate::Result<Vec<Request>> {
    let desired = desired_instances(state, &BTreeSet::new(), &BTreeSet::new(), false);

    let outcome = place::place(state, &desired, &served_drives(state), &nat_networks(state));
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

/// What the leader does with an update record owned by another node: same
/// shape as `expire_verdict`. A record is orphaned when its node left the
/// cluster, is unreachable, or is no longer where the instance is assigned,
/// and a `Queued` one is also given up on after `QUEUED_UPDATE_TTL_SECS`
/// whatever the owner is doing.
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
        // Queued is written by whichever node took the request, so its
        // timestamp says nothing about this process - only its age does. The
        // driver that would start it may be gone (the api node restarted).
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
                    revision_with_mounts(revision, &service, &dynamic, ip),
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
            revision_with_mounts(7, &service, &dynamic, None),
            5074014468841562254
        );
        assert_eq!(
            revision_with_mounts(
                7,
                &service,
                &BTreeSet::new(),
                Some(std::net::Ipv4Addr::new(10, 1, 2, 3))
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

    /// A stack reachable only from the internet, with no internal/private
    /// network of its own, still needs its container reachable cross-node for
    /// the proxy to forward to it - so a public network falls back to
    /// getting its own overlay rather than leaving the stack with no address
    /// at all.
    #[test]
    fn a_public_only_stack_still_gets_its_own_overlay() {
        let cfg = ClusterConfig::parse(CLUSTER).expect("valid cluster config");
        let public_only =
            StackFile::parse("networks:\n  public: {}\n  wan: {}\nservices: {}\n").unwrap();
        assert_eq!(overlay_network_name(&public_only, &cfg), Some("public"));
    }

    /// A stack declaring both keeps the internal/private one: that is the
    /// network whose domain the containers should resolve on, and the one
    /// other stacks on it are reachable through - a public network is only
    /// ever a fallback, not a preference.
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
        });
        state.emergency_stack = Some("site".into());

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

    /// After a failover the instance is on a node that is not the volume's
    /// primary, and the primary has to follow it — otherwise the node it left
    /// stays the sync source and, once it is back, overwrites the live copy
    /// with its stale one every interval.
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

    /// A served drive's volume is registered against the drive's own active
    /// server, not the placed instance's node — the instance can land
    /// anywhere, most of which hold no local copy, and the raft state
    /// machine's `check_placement` only waives "must implement this drive"
    /// for a served drive's real primary.
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

    /// A container created while its drive was FUSE-mounted, then never
    /// recreated, keeps that stale bind path forever if a failover (or fail
    /// back) that changes nothing about the stack file also changes nothing
    /// about the revision `diff` compares.
    #[test]
    fn a_drive_switching_between_local_and_fuse_mounted_changes_the_revision() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let local = revision_with_mounts(1, &service, &BTreeSet::new(), None);
        let fuse_mounted =
            revision_with_mounts(1, &service, &BTreeSet::from(["fast".to_string()]), None);
        assert_ne!(local, fuse_mounted);
    }

    /// A drive this service doesn't even mount switching between local and
    /// FUSE-mounted must not force every other container to be recreated.
    #[test]
    fn an_unrelated_drive_switching_does_not_change_the_revision() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let a = revision_with_mounts(1, &service, &BTreeSet::new(), None);
        let b = revision_with_mounts(1, &service, &BTreeSet::from(["slow".to_string()]), None);
        assert_eq!(a, b);
    }

    /// The stack's own revision still has to move the combined value, the
    /// same as before this was folded in, or an ordinary redeploy would stop
    /// being noticed.
    #[test]
    fn the_stack_revision_still_changes_the_combined_value() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let dynamic = BTreeSet::from(["fast".to_string()]);
        assert_ne!(
            revision_with_mounts(1, &service, &dynamic, None),
            revision_with_mounts(2, &service, &dynamic, None)
        );
    }

    /// The whole reason this was folded in: an instance handed a new overlay
    /// address (a redeploy's replanning pass released and reallocated it,
    /// say, without the container itself ever being touched) has to be
    /// recreated so it actually binds to the address the rest of the cluster
    /// now believes it holds — otherwise it keeps answering on the old one
    /// forever while the reverse proxy and internal DNS point everyone else
    /// at the new one, a permanent "bad gateway" indistinguishable from the
    /// backend actually being down.
    #[test]
    fn an_instance_getting_a_new_overlay_address_changes_the_revision() {
        let service = service_with_volumes("      - fast/data:/data\n");
        let dynamic = BTreeSet::new();
        let none = revision_with_mounts(1, &service, &dynamic, None);
        let a = revision_with_mounts(1, &service, &dynamic, Some("10.244.0.23".parse().unwrap()));
        let b = revision_with_mounts(1, &service, &dynamic, Some("10.244.0.30".parse().unwrap()));
        assert_ne!(none, a);
        assert_ne!(a, b);
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
