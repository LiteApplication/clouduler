//! Keeping a served drive's data safe when its preferred server comes back.
//!
//! Which copy is current is not knowable from `served_by` order: a standby
//! that took over was written to, and the preferred node may have been
//! written to before it dropped out. Every node therefore remembers the tree
//! its copy held when it last agreed with its peer (`DriveRegistry::record_synced`),
//! and a copy whose tree has since changed is "dirty". `sync_direction`
//! turns two copies' states into what may safely happen between them, and
//! `ServedSupervisor` runs the handover that returns a drive to its
//! preferred node: stop serving, final sync, switch.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

use cl_config::Failback;
use cl_raft::served::{ServedAuthority, ServedOp, FAILBACK_RETRY_SECS};
use cl_raft::{ClusterNode, ClusterState, QuorumWatch, Request, VolumeId};

use crate::delta::SyncEngine;
use crate::error::Result;
use crate::registry::DriveRegistry;
use crate::transport::CopyState;

/// Whether a copy was written since it last agreed with its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirtiness {
    /// Untouched since the last sync: overwriting it loses nothing.
    Clean,
    /// Written since the last sync.
    Dirty,
    /// Holds data but has no sync history, so nothing says whether it is
    /// newer or older.
    Unknown,
}

/// `allow_unknown` treats a copy with data but no history as clean. Only
/// right while the drive has never failed over, when the first sync after
/// an upgrade has no history to go on.
pub fn classify(state: &CopyState, allow_unknown: bool) -> Dirtiness {
    match &state.recorded {
        Some(recorded) if *recorded == state.current => Dirtiness::Clean,
        Some(_) => Dirtiness::Dirty,
        None if state.empty || allow_unknown => Dirtiness::Clean,
        None => Dirtiness::Unknown,
    }
}

/// What may happen between the serving copy and another candidate's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Direction {
    /// The target's copy is untouched since they last agreed: copy the
    /// serving node's data over it.
    Push,
    /// The target was written and the serving copy was not: the target has
    /// data the serving node lacks, so nothing may be pushed to it.
    TargetAhead,
    /// Both were written, or it cannot be told: stop and keep both.
    Conflict(String),
}

pub fn sync_direction(
    holder: &CopyState,
    target: &CopyState,
    allow_unknown: bool,
    overwrite_allowed: bool,
) -> Direction {
    if overwrite_allowed {
        return Direction::Push;
    }
    match (classify(target, allow_unknown), classify(holder, false)) {
        (Dirtiness::Clean, _) => Direction::Push,
        (Dirtiness::Dirty, Dirtiness::Clean) => Direction::TargetAhead,
        (Dirtiness::Dirty, Dirtiness::Dirty) => Direction::Conflict(
            "both copies were written since they last agreed".into(),
        ),
        (Dirtiness::Dirty, Dirtiness::Unknown) => Direction::Conflict(
            "the target's copy was written and the serving copy has no sync history to compare against"
                .into(),
        ),
        (Dirtiness::Unknown, _) => Direction::Conflict(
            "the target holds data but has no sync history, so it cannot be told whether it is newer"
                .into(),
        ),
    }
}

/// The drive-level outcome of several volumes' directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrivePlan {
    /// Push these volumes, leave the rest.
    Push(Vec<VolumeId>),
    Conflict {
        volume: VolumeId,
        reason: String,
    },
}

pub fn plan_drive(directions: Vec<(VolumeId, Direction)>) -> DrivePlan {
    let mut push = Vec::new();
    for (id, direction) in directions {
        match direction {
            Direction::Push => push.push(id),
            Direction::TargetAhead => {}
            Direction::Conflict(reason) => return DrivePlan::Conflict { volume: id, reason },
        }
    }
    DrivePlan::Push(push)
}

/// A volume's direction towards one target, with this node's own state
/// (whose digest is recorded once the push succeeds).
pub struct TargetPlan {
    pub direction: Direction,
    pub local: CopyState,
}

/// Asks the target about its copy and decides what may be pushed to it.
pub async fn plan_push<E: SyncEngine + ?Sized>(
    engine: &E,
    registry: &DriveRegistry,
    id: &VolumeId,
    peer: SocketAddr,
    target_node: &str,
    authority: Option<&ServedAuthority>,
) -> Result<TargetPlan> {
    let local = crate::transport::local_copy_state(registry, id).await?;
    let overwrite = authority.is_some_and(|a| a.overwrite.as_deref() == Some(target_node));
    let remote = engine.copy_state(peer, id).await?;
    let allow_unknown = authority.is_none_or(|a| a.epoch <= 1);
    let direction = sync_direction(&local, &remote, allow_unknown, overwrite);
    Ok(TargetPlan { direction, local })
}

/// What a node should do about one served drive right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// This node is the active server and the record does not say so yet.
    Takeover,
    /// A handover away from this node was begun: finish it.
    RunHandover {
        to: String,
    },
    /// A preferred node is back: check it, and begin a handover if safe.
    ConsiderFailback {
        to: String,
    },
}

pub fn next_step(
    node: &str,
    served_by: &[String],
    state: &ClusterState,
    drive: &str,
    mode: Failback,
    now: i64,
) -> Step {
    let authority = state.served.get(drive);
    let active = cl_raft::served::choose_active_server(served_by, &state.unreachable, authority);
    let Some(authority) = authority else {
        return if active == Some(node) {
            Step::Takeover
        } else {
            Step::Nothing
        };
    };
    if authority.holder != node {
        let holder_gone =
            state.unreachable.contains(&authority.holder) || !served_by.contains(&authority.holder);
        return if holder_gone && active == Some(node) {
            Step::Takeover
        } else {
            Step::Nothing
        };
    }
    if let Some(handover) = &authority.handover {
        return if handover.to == node {
            Step::Nothing
        } else {
            Step::RunHandover {
                to: handover.to.clone(),
            }
        };
    }
    if authority.conflict.is_some() {
        return Step::Nothing;
    }
    let Some(preferred) = served_by
        .iter()
        .take_while(|n| *n != node)
        .find(|n| !state.unreachable.contains(*n))
    else {
        return Step::Nothing;
    };
    let wanted = authority.failback_requested || mode == Failback::Auto;
    let waiting = !authority.failback_requested
        && authority
            .blocked
            .as_ref()
            .is_some_and(|b| now.saturating_sub(b.at) < FAILBACK_RETRY_SECS);
    if wanted && !waiting {
        Step::ConsiderFailback {
            to: preferred.clone(),
        }
    } else {
        Step::Nothing
    }
}

/// What the daemon provides around a handover.
pub trait ServedHost: Send + Sync {
    /// The drive's configured fail-back mode.
    fn failback(&self, drive: &str) -> Failback;
    /// Stops this node exporting the drive over NFS, cutting clients off.
    fn suspend_export(&self, drive: &str);
    /// Exports the drive again.
    fn resume_export(&self, drive: &str);
    /// Where `node`'s drive transport listens.
    fn peer_addr(&self, node: &str, state: &ClusterState) -> Option<SocketAddr>;
}

/// How long a handover waits after stopping the export before the final sync.
const DRAIN: Duration = Duration::from_secs(10);
/// How soon a node tries the same drive again after a failed attempt.
const LOCAL_RETRY: Duration = Duration::from_secs(60);

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Runs, on every `served_by` candidate, the bookkeeping that decides who
/// serves a drive: recording a takeover, and handing the drive back to a
/// preferred node that returned.
pub struct ServedSupervisor<E: SyncEngine> {
    node: Arc<ClusterNode>,
    registry: Arc<DriveRegistry>,
    engine: Arc<E>,
    host: Arc<dyn ServedHost>,
    quorum: QuorumWatch,
    busy: Mutex<BTreeSet<String>>,
    last_failure: Mutex<BTreeMap<String, Instant>>,
    drain: Duration,
}

impl<E: SyncEngine + 'static> ServedSupervisor<E> {
    pub fn new(
        node: Arc<ClusterNode>,
        registry: Arc<DriveRegistry>,
        engine: Arc<E>,
        host: Arc<dyn ServedHost>,
    ) -> Arc<Self> {
        let quorum = node.quorum();
        Arc::new(Self {
            node,
            registry,
            engine,
            host,
            quorum,
            busy: Mutex::default(),
            last_failure: Mutex::default(),
            drain: DRAIN,
        })
    }

    /// How long a handover waits between cutting clients off and the final
    /// sync. For tests that do not want to wait it out.
    pub fn with_drain(mut self: Arc<Self>, drain: Duration) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("set before the supervisor is shared")
            .drain = drain;
        self
    }

    pub async fn run(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            if !self.quorum.has_quorum() {
                continue;
            }
            self.pass().await;
        }
    }

    /// One look at every served drive this node is a candidate for.
    pub async fn pass(self: &Arc<Self>) {
        let state = self.node.state().await;
        let drives: Vec<String> = self
            .registry
            .drives()
            .map(|(name, _)| name.to_string())
            .filter(|name| {
                self.registry
                    .served_candidates(name)
                    .iter()
                    .any(|n| n == self.registry.node())
            })
            .collect();
        for drive in drives {
            let served_by = self.registry.served_candidates(&drive).to_vec();
            let step = next_step(
                self.registry.node(),
                &served_by,
                &state,
                &drive,
                self.host.failback(&drive),
                unix_now(),
            );
            let is_handover = matches!(step, Step::RunHandover { .. });
            match step {
                Step::Nothing => {}
                Step::Takeover => {
                    self.submit(
                        &drive,
                        ServedOp::Takeover {
                            node: self.registry.node().to_string(),
                            served_by,
                        },
                    )
                    .await;
                }
                Step::RunHandover { to } | Step::ConsiderFailback { to } => {
                    let handover = is_handover;
                    if !self.claim(&drive) {
                        continue;
                    }
                    let this = self.clone();
                    tokio::spawn(async move {
                        let progressed = if handover {
                            this.run_handover(&drive, &to).await
                        } else {
                            this.consider_failback(&drive, &to).await
                        };
                        this.release(&drive, progressed);
                    });
                }
            }
        }
    }

    /// Takes the drive for one attempt, unless one is running or one failed a
    /// moment ago.
    fn claim(&self, drive: &str) -> bool {
        let failed_recently = self
            .last_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(drive)
            .is_some_and(|at| at.elapsed() < LOCAL_RETRY);
        if failed_recently {
            return false;
        }
        self.busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(drive.to_string())
    }

    /// Ends an attempt; one that got nowhere is not repeated for a while.
    fn release(&self, drive: &str, progressed: bool) {
        self.busy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(drive);
        let mut failures = self.last_failure.lock().unwrap_or_else(|e| e.into_inner());
        if progressed {
            failures.remove(drive);
        } else {
            failures.insert(drive.to_string(), Instant::now());
        }
    }

    async fn submit(&self, drive: &str, op: ServedOp) -> bool {
        match self
            .node
            .submit(Request::Served {
                drive: drive.to_string(),
                op: op.clone(),
            })
            .await
        {
            Ok(response) => match response.error {
                None => true,
                Some(error) => {
                    warn!(%drive, ?op, %error, "served drive change refused");
                    false
                }
            },
            Err(e) => {
                warn!(%drive, ?op, error = %e, "could not record a served drive change");
                false
            }
        }
    }

    /// The directions of every volume of `drive` towards `to`.
    async fn plan(
        &self,
        drive: &str,
        to: &str,
        state: &ClusterState,
    ) -> std::result::Result<(DrivePlan, BTreeMap<VolumeId, TargetPlan>), String> {
        let peer = self
            .host
            .peer_addr(to, state)
            .ok_or_else(|| format!("no address for {to}"))?;
        let mut directions = Vec::new();
        let mut plans = BTreeMap::new();
        for id in state.volumes.keys().filter(|id| id.drive == drive) {
            let plan = plan_push(
                self.engine.as_ref(),
                &self.registry,
                id,
                peer,
                to,
                state.served.get(drive),
            )
            .await
            .map_err(|e| format!("could not compare {id} with {to}: {e}"))?;
            directions.push((id.clone(), plan.direction.clone()));
            plans.insert(id.clone(), plan);
        }
        Ok((plan_drive(directions), plans))
    }

    async fn record_conflict(
        &self,
        drive: &str,
        other: &str,
        volume: &VolumeId,
        reason: &str,
    ) -> bool {
        warn!(%drive, %other, %volume, %reason, "served drive copies diverged, keeping both");
        self.submit(
            drive,
            ServedOp::Conflict {
                holder: self.registry.node().to_string(),
                other: other.to_string(),
                reason: format!("{volume}: {reason}"),
                at: unix_now(),
            },
        )
        .await
    }

    /// Looks at a returned preferred node before touching anything: a
    /// conflict is recorded without stopping the drive, and a safe handover
    /// is begun.
    async fn consider_failback(&self, drive: &str, to: &str) -> bool {
        let state = self.node.state().await;
        match self.plan(drive, to, &state).await {
            Err(reason) => {
                info!(%drive, %to, %reason, "not handing the drive back yet");
                false
            }
            Ok((DrivePlan::Conflict { volume, reason }, _)) => {
                self.record_conflict(drive, to, &volume, &reason).await
            }
            Ok((DrivePlan::Push(_), _)) => {
                info!(%drive, %to, "handing the drive back to its preferred server");
                self.submit(
                    drive,
                    ServedOp::BeginHandover {
                        to: to.to_string(),
                        at: unix_now(),
                    },
                )
                .await
            }
        }
    }

    /// The handover itself: clients are cut off, what the preferred node is
    /// missing is copied over, and only then does it start serving. Any
    /// failure leaves this node serving, with the reason recorded.
    async fn run_handover(&self, drive: &str, to: &str) -> bool {
        self.host.suspend_export(drive);
        tokio::time::sleep(self.drain).await;
        let outcome = self.hand_over(drive, to).await;
        self.host.resume_export(drive);
        match outcome {
            Ok(()) => {
                self.submit(drive, ServedOp::CompleteHandover { to: to.to_string() })
                    .await
            }
            Err(Failure::Conflict { volume, reason }) => {
                self.record_conflict(drive, to, &volume, &reason).await
            }
            Err(Failure::Other(reason)) => {
                warn!(%drive, %to, %reason, "handover failed, this node keeps serving");
                self.submit(
                    drive,
                    ServedOp::AbortHandover {
                        reason,
                        at: unix_now(),
                    },
                )
                .await;
                false
            }
        }
    }

    async fn hand_over(&self, drive: &str, to: &str) -> std::result::Result<(), Failure> {
        let state = self.node.state().await;
        let (plan, plans) = self.plan(drive, to, &state).await.map_err(Failure::Other)?;
        let push = match plan {
            DrivePlan::Conflict { volume, reason } => {
                return Err(Failure::Conflict { volume, reason })
            }
            DrivePlan::Push(push) => push,
        };
        let peer = self
            .host
            .peer_addr(to, &state)
            .ok_or_else(|| Failure::Other(format!("no address for {to}")))?;
        for id in push {
            let src = self
                .registry
                .volume_path(&id)
                .map_err(|e| Failure::Other(e.to_string()))?;
            let generation = state.volumes.get(&id).map_or(0, |v| v.generation);
            self.engine
                .sync(
                    &src,
                    peer,
                    &id,
                    generation,
                    None,
                    crate::progress::ProgressSink::none(),
                )
                .await
                .map_err(|e| Failure::Other(format!("final sync of {id} to {to} failed: {e}")))?;
            self.registry.record_synced(&id, &plans[&id].local.current);
        }
        Ok(())
    }
}

enum Failure {
    Conflict { volume: VolumeId, reason: String },
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::served::{FailbackBlock, ServedConflict, ServedHandover};

    fn copy(current: &str, recorded: Option<&str>, empty: bool) -> CopyState {
        CopyState {
            current: current.into(),
            recorded: recorded.map(str::to_string),
            empty,
        }
    }

    fn clean() -> CopyState {
        copy("a", Some("a"), false)
    }
    fn dirty() -> CopyState {
        copy("b", Some("a"), false)
    }
    fn unknown() -> CopyState {
        copy("a", None, false)
    }

    #[test]
    fn a_copy_is_dirty_only_when_its_tree_moved_off_the_recorded_one() {
        assert_eq!(classify(&clean(), false), Dirtiness::Clean);
        assert_eq!(classify(&dirty(), false), Dirtiness::Dirty);
        assert_eq!(classify(&unknown(), false), Dirtiness::Unknown);
        assert_eq!(classify(&unknown(), true), Dirtiness::Clean);
        assert_eq!(
            classify(&copy("e", None, true), false),
            Dirtiness::Clean,
            "an empty copy has nothing to lose"
        );
    }

    #[test]
    fn a_clean_target_is_pushed_to_whatever_the_holder_looks_like() {
        for holder in [clean(), dirty(), unknown()] {
            assert_eq!(
                sync_direction(&holder, &clean(), false, false),
                Direction::Push
            );
        }
    }

    #[test]
    fn a_written_target_is_never_pushed_over() {
        assert_eq!(
            sync_direction(&clean(), &dirty(), false, false),
            Direction::TargetAhead
        );
        assert!(matches!(
            sync_direction(&dirty(), &dirty(), false, false),
            Direction::Conflict(_)
        ));
        assert!(matches!(
            sync_direction(&unknown(), &dirty(), false, false),
            Direction::Conflict(_)
        ));
    }

    #[test]
    fn a_target_with_data_and_no_history_is_a_conflict_unless_the_drive_is_new() {
        assert!(matches!(
            sync_direction(&clean(), &unknown(), false, false),
            Direction::Conflict(_)
        ));
        assert_eq!(
            sync_direction(&clean(), &unknown(), true, false),
            Direction::Push
        );
    }

    #[test]
    fn a_resolved_conflict_allows_the_overwrite() {
        assert_eq!(
            sync_direction(&dirty(), &dirty(), false, true),
            Direction::Push
        );
    }

    #[test]
    fn a_drive_plan_pushes_what_it_must_and_stops_at_a_conflict() {
        let a = VolumeId::new("fast", "a");
        let b = VolumeId::new("fast", "b");
        let c = VolumeId::new("fast", "c");
        assert_eq!(
            plan_drive(vec![
                (a.clone(), Direction::Push),
                (b.clone(), Direction::TargetAhead)
            ]),
            DrivePlan::Push(vec![a.clone()])
        );
        assert_eq!(
            plan_drive(vec![
                (a, Direction::Push),
                (b.clone(), Direction::Conflict("x".into())),
                (c, Direction::Push)
            ]),
            DrivePlan::Conflict {
                volume: b,
                reason: "x".into()
            }
        );
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn state(unreachable: &[&str], authority: Option<ServedAuthority>) -> ClusterState {
        let mut state = ClusterState {
            unreachable: unreachable.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        };
        if let Some(a) = authority {
            state.served.insert("fast".into(), a);
        }
        state
    }

    fn held_by(node: &str) -> ServedAuthority {
        ServedAuthority {
            holder: node.into(),
            epoch: 2,
            ..Default::default()
        }
    }

    fn step(node: &str, st: &ClusterState, mode: Failback) -> Step {
        next_step(node, &names(&["n1", "n2"]), st, "fast", mode, 10_000)
    }

    #[test]
    fn the_first_active_server_records_itself() {
        let st = state(&[], None);
        assert_eq!(step("n1", &st, Failback::Auto), Step::Takeover);
        assert_eq!(step("n2", &st, Failback::Auto), Step::Nothing);
    }

    #[test]
    fn a_standby_takes_over_only_once_the_holder_is_unreachable() {
        let mut st = state(&[], Some(held_by("n1")));
        assert_eq!(step("n2", &st, Failback::Auto), Step::Nothing);
        st.unreachable.insert("n1".into());
        assert_eq!(step("n2", &st, Failback::Auto), Step::Takeover);
    }

    #[test]
    fn a_returned_preferred_node_does_not_take_the_drive_by_coming_back() {
        let st = state(&[], Some(held_by("n2")));
        assert_eq!(
            step("n1", &st, Failback::Auto),
            Step::Nothing,
            "the returning node does nothing: the holder decides"
        );
        assert_eq!(
            step("n2", &st, Failback::Auto),
            Step::ConsiderFailback { to: "n1".into() }
        );
    }

    #[test]
    fn manual_failback_waits_for_a_request() {
        let mut st = state(&[], Some(held_by("n2")));
        assert_eq!(step("n2", &st, Failback::Manual), Step::Nothing);
        st.served.get_mut("fast").unwrap().failback_requested = true;
        assert_eq!(
            step("n2", &st, Failback::Manual),
            Step::ConsiderFailback { to: "n1".into() }
        );
    }

    #[test]
    fn nothing_is_handed_to_an_unreachable_node() {
        let st = state(&["n1"], Some(held_by("n2")));
        assert_eq!(step("n2", &st, Failback::Auto), Step::Nothing);
    }

    #[test]
    fn a_failed_failback_is_not_retried_for_an_hour() {
        let mut a = held_by("n2");
        a.blocked = Some(FailbackBlock {
            node: "n1".into(),
            reason: "sync failed".into(),
            at: 10_000 - 60,
        });
        let st = state(&[], Some(a.clone()));
        assert_eq!(step("n2", &st, Failback::Auto), Step::Nothing);

        let later = next_step(
            "n2",
            &names(&["n1", "n2"]),
            &st,
            "fast",
            Failback::Auto,
            10_000 + FAILBACK_RETRY_SECS,
        );
        assert_eq!(later, Step::ConsiderFailback { to: "n1".into() });

        let mut asked = a;
        asked.failback_requested = true;
        assert_eq!(
            step("n2", &state(&[], Some(asked)), Failback::Auto),
            Step::ConsiderFailback { to: "n1".into() },
            "an operator's request skips the wait"
        );
    }

    #[test]
    fn a_conflict_stops_every_automatic_move() {
        let mut a = held_by("n2");
        a.conflict = Some(ServedConflict {
            holder: "n2".into(),
            other: "n1".into(),
            reason: "x".into(),
            at: 1,
        });
        assert_eq!(
            step("n2", &state(&[], Some(a)), Failback::Auto),
            Step::Nothing
        );
    }

    #[test]
    fn the_holder_runs_a_begun_handover() {
        let mut a = held_by("n2");
        a.handover = Some(ServedHandover {
            to: "n1".into(),
            at: 1,
        });
        let st = state(&[], Some(a));
        assert_eq!(
            step("n2", &st, Failback::Auto),
            Step::RunHandover { to: "n1".into() }
        );
        assert_eq!(step("n1", &st, Failback::Auto), Step::Nothing);
    }

    #[test]
    fn the_last_candidate_has_nobody_to_hand_back_to() {
        let st = state(&[], Some(held_by("n1")));
        assert_eq!(step("n1", &st, Failback::Auto), Step::Nothing);
    }
}
