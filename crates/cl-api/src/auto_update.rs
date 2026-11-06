//! The leader's image update checker and `x-clouduler.auto_update`.
//!
//! Two clocks, both leader only:
//!
//! * A timer (`updates.check_interval`, or the Updates page's Refresh button)
//!   asks the registries about every image the stacks declare and stores the
//!   answers in `ClusterState::image_checks`. Nothing else talks to a
//!   registry; the GET endpoints only read that cache.
//! * Every tick, due auto-updates are handed to the ordinary update pipeline
//!   (`routes::updates::apply_selection`), exactly as if the Updates page's
//!   button had been clicked, and finished ones are settled in
//!   `ClusterState::auto_updates`.
//!
//! Everything that survives between ticks lives in raft (`AutoUpdateRecord`),
//! so a new leader continues a delay countdown instead of restarting it and
//! never retries a digest that already failed. The decisions are the pure
//! functions below.

use std::time::Duration;

use cl_config::{AutoUpdate, AutoUpdateMode};
use cl_raft::state::{
    AutoUpdateOutcome, AutoUpdateRecord, ClusterState, DesiredState, InstanceUpdate, UpdatePhase,
};
use cl_raft::{InstanceId, Request};
use tracing::{info, warn};

use crate::server::AppState;

/// How often the leader looks at due auto-updates and the check timer.
const TICK: Duration = Duration::from_secs(30);

/// After becoming leader (or booting), how long before a check is considered.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);

/// How long an auto-update may stay `Queued` with no update record of its own
/// before it is called failed.
const NEVER_STARTED_SECS: i64 = 15 * 60;

/// The reason `Batch::fail` gives instances it cancelled, which were not at
/// fault.
const CANCELLED_PREFIX: &str = "cancelled:";

/// What a check just learned about one instance.
pub struct Seen<'a> {
    /// The registry serves a different digest than the one running.
    pub update_available: bool,
    pub latest: Option<&'a str>,
}

/// After a registry check: what to record for an instance, if anything.
/// `None` for mode off, nothing newer, or a digest already recorded.
pub fn detect(
    auto: AutoUpdate,
    seen: &Seen<'_>,
    record: Option<&AutoUpdateRecord>,
) -> Option<(String, AutoUpdateOutcome)> {
    let latest = seen.latest?;
    if auto.mode == AutoUpdateMode::Off || !seen.update_available {
        return None;
    }
    let outcome = match auto.mode {
        AutoUpdateMode::Apply => AutoUpdateOutcome::Waiting,
        AutoUpdateMode::Notify => AutoUpdateOutcome::Notified,
        AutoUpdateMode::Off => return None,
    };
    match record {
        // The same digest again: the countdown started when it was first
        // seen, and a failure is not retried.
        Some(r) if r.digest == latest => {
            // Switching from apply to notify while waiting still notifies.
            (outcome == AutoUpdateOutcome::Notified && r.outcome == AutoUpdateOutcome::Waiting)
                .then(|| (latest.to_string(), outcome))
        }
        // A newer digest replaces whatever came before, restarting the delay.
        _ => Some((latest.to_string(), outcome)),
    }
}

/// Reasons an otherwise due auto-update waits.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Blockers {
    /// A stack is in emergency mode; the cluster is in a degraded state and
    /// nothing is restarted for convenience.
    pub emergency: bool,
    /// An update or redeploy of this instance is under way.
    pub in_flight: bool,
    /// The instance is stopped on purpose.
    pub stopped: bool,
}

/// Whether to queue the update now. `cached_latest` is what the last
/// registry check says the tag serves: a record for an older digest is
/// stale, and the check that saw the newer one has replaced it.
pub fn due(
    auto: AutoUpdate,
    record: &AutoUpdateRecord,
    cached_latest: Option<&str>,
    blockers: Blockers,
    now: i64,
) -> bool {
    if auto.mode != AutoUpdateMode::Apply {
        return false;
    }
    // A notified record turns into a waiting one when the mode becomes apply.
    if !matches!(
        record.outcome,
        AutoUpdateOutcome::Waiting | AutoUpdateOutcome::Notified
    ) {
        return false;
    }
    if cached_latest != Some(record.digest.as_str()) {
        return false;
    }
    if blockers.emergency || blockers.in_flight || blockers.stopped {
        return false;
    }
    now - record.detected_at >= auto.delay.as_secs() as i64
}

/// Unix seconds an apply-mode record will be applied, for display.
pub fn apply_at(auto: AutoUpdate, record: &AutoUpdateRecord) -> Option<i64> {
    (auto.mode == AutoUpdateMode::Apply
        && matches!(
            record.outcome,
            AutoUpdateOutcome::Waiting | AutoUpdateOutcome::Notified
        ))
    .then(|| record.detected_at + auto.delay.as_secs() as i64)
}

/// How a `Queued` record ended, from the instance's update entry.
/// `None` while still in progress.
pub fn settle(
    record: &AutoUpdateRecord,
    update: Option<&InstanceUpdate>,
    now: i64,
) -> Option<(AutoUpdateOutcome, Option<String>)> {
    if record.outcome != AutoUpdateOutcome::Queued {
        return None;
    }
    // An entry older than the record belongs to an earlier update.
    match update.filter(|u| u.at >= record.at) {
        Some(u) => match u.phase {
            UpdatePhase::Started => Some((AutoUpdateOutcome::Updated, None)),
            UpdatePhase::Failed => {
                let error = u
                    .error
                    .clone()
                    .unwrap_or_else(|| "the update failed".into());
                if error.starts_with(CANCELLED_PREFIX) {
                    // Another instance's failure stopped the batch; this one
                    // did nothing wrong and goes back to waiting.
                    Some((AutoUpdateOutcome::Waiting, None))
                } else {
                    Some((AutoUpdateOutcome::Failed, Some(error)))
                }
            }
            _ => None,
        },
        None if now - record.at > NEVER_STARTED_SECS => Some((
            AutoUpdateOutcome::Failed,
            Some("the update never started (the leader changed or the instance was busy)".into()),
        )),
        None => None,
    }
}

/// What blocks `instance` from being updated right now.
pub fn blockers(state: &ClusterState, instance: &InstanceId) -> Blockers {
    Blockers {
        emergency: state.emergency_stack.is_some(),
        in_flight: state
            .instance_updates
            .get(instance)
            .is_some_and(|u| !u.phase.is_terminal()),
        stopped: state
            .assignments
            .get(instance)
            .is_none_or(|a| a.desired == DesiredState::Stopped),
    }
}

/// The resolved auto_update setting of `instance`.
pub fn setting(state: &ClusterState, instance: &InstanceId) -> AutoUpdate {
    state
        .stacks
        .get(&instance.stack)
        .and_then(|s| cl_config::StackFile::parse(&s.spec).ok())
        .map(|f| f.effective_auto_update(&instance.service))
        .unwrap_or_default()
}

async fn submit(state: &AppState, request: Request) {
    match state.cluster.submit(request).await {
        Ok(r) => {
            if let Some(e) = r.error {
                warn!(error = %e, "auto_update record was rejected");
            }
        }
        Err(e) => warn!(error = %e, "could not record auto_update"),
    }
}

/// Record what a fresh registry check found, per instance. Called by the
/// leader right after the check, with the `Seen` facts it just computed.
pub async fn record_detections(state: &AppState, seen: Vec<(InstanceId, Option<String>, bool)>) {
    let cluster_state = state.cluster.state().await;
    let now = crate::alerts::unix_now();
    for (instance, latest, update_available) in seen {
        let auto = setting(&cluster_state, &instance);
        let s = Seen {
            update_available,
            latest: latest.as_deref(),
        };
        if let Some((digest, outcome)) = detect(auto, &s, cluster_state.auto_updates.get(&instance))
        {
            info!(%instance, %digest, ?outcome, "auto_update saw a newer image");
            submit(
                state,
                Request::SetAutoUpdate {
                    instance,
                    digest,
                    outcome,
                    error: None,
                    at: now,
                },
            )
            .await;
        }
    }
}

/// One pass over the records: settle finished ones, apply due ones.
pub async fn act(state: &AppState) {
    let cluster_state = state.cluster.state().await;
    let now = crate::alerts::unix_now();
    let mut to_apply = Vec::new();

    for (instance, record) in &cluster_state.auto_updates {
        if let Some((outcome, error)) =
            settle(record, cluster_state.instance_updates.get(instance), now)
        {
            submit(
                state,
                Request::SetAutoUpdate {
                    instance: instance.clone(),
                    digest: record.digest.clone(),
                    outcome,
                    error,
                    at: now,
                },
            )
            .await;
            continue;
        }
        let auto = setting(&cluster_state, instance);
        let declared = crate::routes::updates::declared_image(&cluster_state, instance);
        let cached = declared
            .as_ref()
            .and_then(|i| cluster_state.image_checks.get(i))
            .and_then(|c| c.latest_digest.as_deref());
        if due(
            auto,
            record,
            cached,
            blockers(&cluster_state, instance),
            now,
        ) {
            to_apply.push(instance.clone());
        }
    }
    if to_apply.is_empty() {
        return;
    }

    // Recorded before the update is queued: a leader that dies in between
    // leaves a Queued record, which `settle` fails rather than retries.
    for instance in &to_apply {
        let digest = cluster_state.auto_updates[instance].digest.clone();
        submit(
            state,
            Request::SetAutoUpdate {
                instance: instance.clone(),
                digest,
                outcome: AutoUpdateOutcome::Queued,
                error: None,
                at: now,
            },
        )
        .await;
    }
    info!(
        count = to_apply.len(),
        "auto_update is applying image updates"
    );
    let selection = crate::routes::updates::InstanceSelection {
        instances: to_apply.iter().map(|i| i.to_string()).collect(),
    };
    let _ = crate::routes::updates::apply_selection(state, selection).await;
}

/// The leader's loop. Cheap when nothing is due.
pub fn spawn(state: AppState) {
    let mut shutdown = state.shutdown.subscribe();
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => return,
            }
            if !state.cluster.is_leader() {
                continue;
            }
            if started.elapsed() >= FIRST_CHECK_DELAY && check_is_due(&state).await {
                crate::routes::updates::check_images(&state).await;
            }
            act(&state).await;
        }
    });
}

async fn check_is_due(state: &AppState) -> bool {
    let interval = state
        .cluster_config
        .read()
        .await
        .updates
        .effective_interval();
    let last = state.cluster.state().await.image_checks_at;
    due_check(last, crate::alerts::unix_now(), interval)
}

/// Whether a registry check is due: never run, or older than `interval`.
pub fn due_check(last_checked_at: i64, now: i64, interval: Duration) -> bool {
    last_checked_at == 0 || now - last_checked_at >= interval.as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto(mode: AutoUpdateMode, delay_secs: u64) -> AutoUpdate {
        AutoUpdate {
            mode,
            delay: Duration::from_secs(delay_secs),
        }
    }

    fn record(digest: &str, outcome: AutoUpdateOutcome, detected_at: i64) -> AutoUpdateRecord {
        AutoUpdateRecord {
            digest: digest.into(),
            detected_at,
            outcome,
            at: detected_at,
            error: None,
        }
    }

    fn seen(latest: &str) -> Seen<'_> {
        Seen {
            update_available: true,
            latest: Some(latest),
        }
    }

    #[test]
    fn detect_ignores_off_and_up_to_date() {
        let off = auto(AutoUpdateMode::Off, 0);
        assert!(detect(off, &seen("sha256:a"), None).is_none());
        let apply = auto(AutoUpdateMode::Apply, 0);
        let current = Seen {
            update_available: false,
            latest: Some("sha256:a"),
        };
        assert!(detect(apply, &current, None).is_none());
        let unknown = Seen {
            update_available: true,
            latest: None,
        };
        assert!(detect(apply, &unknown, None).is_none());
    }

    #[test]
    fn detect_records_a_new_digest_once() {
        let apply = auto(AutoUpdateMode::Apply, 3600);
        let first = detect(apply, &seen("sha256:a"), None).unwrap();
        assert_eq!(first, ("sha256:a".into(), AutoUpdateOutcome::Waiting));
        // Seen again by the next check: unchanged, so the countdown stands.
        let rec = record("sha256:a", AutoUpdateOutcome::Waiting, 100);
        assert!(detect(apply, &seen("sha256:a"), Some(&rec)).is_none());
        // A newer digest restarts it.
        let newer = detect(apply, &seen("sha256:b"), Some(&rec)).unwrap();
        assert_eq!(newer.0, "sha256:b");
        // A failed digest is not retried by detection either.
        let failed = record("sha256:a", AutoUpdateOutcome::Failed, 100);
        assert!(detect(apply, &seen("sha256:a"), Some(&failed)).is_none());
    }

    #[test]
    fn notify_mode_notifies_once_per_digest() {
        let notify = auto(AutoUpdateMode::Notify, 0);
        let first = detect(notify, &seen("sha256:a"), None).unwrap();
        assert_eq!(first.1, AutoUpdateOutcome::Notified);
        let rec = record("sha256:a", AutoUpdateOutcome::Notified, 1);
        assert!(detect(notify, &seen("sha256:a"), Some(&rec)).is_none());
        assert!(detect(notify, &seen("sha256:b"), Some(&rec)).is_some());
        // Waiting under apply, then switched to notify: notified.
        let waiting = record("sha256:a", AutoUpdateOutcome::Waiting, 1);
        assert!(detect(notify, &seen("sha256:a"), Some(&waiting)).is_some());
    }

    #[test]
    fn due_waits_out_the_delay_from_detection() {
        let a = auto(AutoUpdateMode::Apply, 3600);
        let rec = record("sha256:a", AutoUpdateOutcome::Waiting, 1000);
        let none = Blockers::default();
        assert!(!due(a, &rec, Some("sha256:a"), none, 1000 + 3599));
        assert!(due(a, &rec, Some("sha256:a"), none, 1000 + 3600));
        // Zero delay applies at once.
        assert!(due(
            auto(AutoUpdateMode::Apply, 0),
            &rec,
            Some("sha256:a"),
            none,
            1000
        ));
    }

    #[test]
    fn due_refuses_unsafe_or_stale_cases() {
        let a = auto(AutoUpdateMode::Apply, 0);
        let rec = record("sha256:a", AutoUpdateOutcome::Waiting, 0);
        let ok = Blockers::default();
        assert!(due(a, &rec, Some("sha256:a"), ok, 10));
        for b in [
            Blockers {
                emergency: true,
                ..ok
            },
            Blockers {
                in_flight: true,
                ..ok
            },
            Blockers {
                stopped: true,
                ..ok
            },
        ] {
            assert!(!due(a, &rec, Some("sha256:a"), b, 10), "{b:?}");
        }
        // The cache moved on to a newer digest: the record is stale.
        assert!(!due(a, &rec, Some("sha256:b"), ok, 10));
        assert!(!due(a, &rec, None, ok, 10));
        // Notify and off never apply.
        assert!(!due(
            auto(AutoUpdateMode::Notify, 0),
            &rec,
            Some("sha256:a"),
            ok,
            10
        ));
        assert!(!due(
            auto(AutoUpdateMode::Off, 0),
            &rec,
            Some("sha256:a"),
            ok,
            10
        ));
        // Already tried: failed, queued, updated never go again.
        for outcome in [
            AutoUpdateOutcome::Failed,
            AutoUpdateOutcome::Queued,
            AutoUpdateOutcome::Updated,
        ] {
            let done = record("sha256:a", outcome, 0);
            assert!(!due(a, &done, Some("sha256:a"), ok, 10), "{outcome:?}");
        }
        // A notified record is applied once the mode says apply.
        let notified = record("sha256:a", AutoUpdateOutcome::Notified, 0);
        assert!(due(a, &notified, Some("sha256:a"), ok, 10));
    }

    fn entry(phase: UpdatePhase, at: i64, error: Option<&str>) -> InstanceUpdate {
        InstanceUpdate {
            node: "n1".into(),
            phase,
            started_at: at,
            at,
            error: error.map(String::from),
        }
    }

    #[test]
    fn settle_follows_the_update_record() {
        let mut rec = record("sha256:a", AutoUpdateOutcome::Queued, 100);
        rec.at = 100;
        let started = entry(UpdatePhase::Started, 120, None);
        assert_eq!(
            settle(&rec, Some(&started), 130),
            Some((AutoUpdateOutcome::Updated, None))
        );
        let failed = entry(UpdatePhase::Failed, 120, Some("pull failed"));
        assert_eq!(
            settle(&rec, Some(&failed), 130),
            Some((AutoUpdateOutcome::Failed, Some("pull failed".into())))
        );
        // Still going.
        assert_eq!(
            settle(&rec, Some(&entry(UpdatePhase::Pulling, 120, None)), 130),
            None
        );
        // An older update's entry is not ours.
        assert_eq!(
            settle(&rec, Some(&entry(UpdatePhase::Started, 50, None)), 130),
            None
        );
        // Collateral of another instance's failure goes back to waiting.
        let cancelled = entry(
            UpdatePhase::Failed,
            120,
            Some("cancelled: the update of a/b/0 failed, so the rolling update stopped"),
        );
        assert_eq!(
            settle(&rec, Some(&cancelled), 130),
            Some((AutoUpdateOutcome::Waiting, None))
        );
        // Never started.
        assert_eq!(settle(&rec, None, 100 + NEVER_STARTED_SECS), None);
        let (outcome, error) = settle(&rec, None, 101 + NEVER_STARTED_SECS).unwrap();
        assert_eq!(outcome, AutoUpdateOutcome::Failed);
        assert!(error.unwrap().contains("never started"));
        // Only queued records settle.
        let waiting = record("sha256:a", AutoUpdateOutcome::Waiting, 0);
        assert_eq!(settle(&waiting, Some(&started), 130), None);
    }

    #[test]
    fn check_timer() {
        let six_hours = Duration::from_secs(6 * 3600);
        assert!(due_check(0, 10, six_hours));
        assert!(!due_check(100, 100 + 6 * 3600 - 1, six_hours));
        assert!(due_check(100, 100 + 6 * 3600, six_hours));
    }

    #[test]
    fn apply_at_is_detection_plus_delay_while_pending() {
        let a = auto(AutoUpdateMode::Apply, 600);
        let rec = record("sha256:a", AutoUpdateOutcome::Waiting, 1000);
        assert_eq!(apply_at(a, &rec), Some(1600));
        let done = record("sha256:a", AutoUpdateOutcome::Updated, 1000);
        assert_eq!(apply_at(a, &done), None);
        assert_eq!(apply_at(auto(AutoUpdateMode::Notify, 600), &rec), None);
    }
}
