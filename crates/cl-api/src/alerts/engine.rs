//! The leader's alert engine.
//!
//! Runs on every node, acts only while the node leads. Each tick it reads the
//! settings and the feed out of raft, folds in what it observes (standing
//! conditions, signals pushed by nodes, state changes), persists the records
//! that changed and only then sends. See `lifecycle` for why a change of
//! leader does not repeat messages.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use cl_alert::catalog;
use cl_alert::{AlertMessage, AlertSettings, Body, Link, Notification};
use cl_config::AlertSeverity;
use cl_raft::alerts::{AlertDelivery, AlertRecord, AlertSignal, DestinationHealth, SignalAction};
use cl_raft::{ClusterState, Request};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::conditions::{self, Derived};
use super::events::{self, LeaderChanges, Snapshot};
use super::filter::effective_severity;
use super::lifecycle::{Kind, Ledger, Observed, SendJob};
use super::send::{Outcome, Sender};
use super::{quorum, settings, sev, ui_url, unix_now};
use crate::mcp::redact::Redactor;
use crate::server::AppState;

const TICK: Duration = Duration::from_secs(5);
const CONDITIONS_EVERY: Duration = Duration::from_secs(15);
const CONDITIONS_TIMEOUT: Duration = Duration::from_secs(20);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// Records kept in the feed no matter how recent.
const FEED_SOFT_CAP: usize = 2000;
const OOM_WINDOW_SECS: i64 = 3600;

pub fn spawn(state: AppState) {
    let mut shutdown = state.shutdown.subscribe();
    tokio::spawn(async move {
        let mut engine = Engine::new(state);
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => engine.tick().await,
                _ = shutdown.changed() => return,
            }
        }
    });
}

pub struct Engine {
    state: AppState,
    sender: Sender,
    tx: mpsc::UnboundedSender<Outcome>,
    rx: mpsc::UnboundedReceiver<Outcome>,
    quorum: quorum::QuorumReporter,
    leading: bool,
    last_leader: Option<u64>,
    leader_changes: LeaderChanges,
    snapshot: Option<Snapshot>,
    absent: HashMap<String, u32>,
    first_seen: HashMap<String, i64>,
    last_conditions: Option<Instant>,
    last_prune: Instant,
}

impl Engine {
    pub fn new(state: AppState) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            sender: Sender::new(state.cluster.clone(), state.key.clone()),
            quorum: quorum::QuorumReporter::new(state.clone()),
            state,
            tx,
            rx,
            leading: false,
            last_leader: None,
            leader_changes: LeaderChanges::default(),
            snapshot: None,
            absent: HashMap::new(),
            first_seen: HashMap::new(),
            last_conditions: None,
            last_prune: Instant::now(),
        }
    }

    pub async fn tick(&mut self) {
        let now = unix_now();
        let leader = self.state.cluster.quorum().leader();
        if leader.is_some() && leader != self.last_leader {
            if self.last_leader.is_some() {
                self.leader_changes.record(now);
            }
            self.last_leader = leader;
        }
        self.quorum.tick().await;
        self.drain_outcomes().await;

        if !self.state.cluster.is_leader() {
            self.leading = false;
            self.snapshot = None;
            self.absent.clear();
            self.first_seen.clear();
            self.last_conditions = None;
            return;
        }
        if let Err(e) = self.lead(now).await {
            debug!(error = %e, "alert pass abandoned, will retry");
        }
    }

    async fn drain_outcomes(&mut self) {
        let mut batch = Vec::new();
        while let Ok(o) = self.rx.try_recv() {
            batch.push(o);
        }
        if batch.is_empty() {
            return;
        }
        let st = self.state.cluster.state().await;
        let settings = settings::load(&st);
        let mut records: HashMap<String, AlertRecord> = HashMap::new();
        let mut health: HashMap<String, DestinationHealth> = HashMap::new();
        for o in batch {
            for id in &o.records {
                let rec = records
                    .entry(id.clone())
                    .or_insert_with(|| st.alerts.get(id).cloned().unwrap_or_else(|| missing(id)));
                if rec.id == "-" {
                    continue;
                }
                rec.deliveries.push(AlertDelivery {
                    destination: o.destination.clone(),
                    at: o.at,
                    ok: o.ok,
                    error: o.error.clone(),
                    attempts: o.attempts,
                    phase: o.phase.to_string(),
                    message_id: o.message_id.clone(),
                });
            }
            if o.counts_for_health
                && settings
                    .destinations
                    .iter()
                    .any(|d| d.name == o.destination)
            {
                let prev = health
                    .get(&o.destination)
                    .or_else(|| st.alert_destinations.get(&o.destination))
                    .map(|h| h.consecutive_failures)
                    .unwrap_or(0);
                health.insert(
                    o.destination.clone(),
                    DestinationHealth {
                        ok: o.ok,
                        at: o.at,
                        error: o.error.clone(),
                        consecutive_failures: if o.ok { 0 } else { prev + 1 },
                    },
                );
            }
        }
        for (_, rec) in records.into_iter().filter(|(_, r)| r.id != "-") {
            let _ = self.state.cluster.submit(Request::PutAlert(rec)).await;
        }
        for (name, h) in health {
            let _ = self
                .state
                .cluster
                .submit(Request::SetDestinationHealth { name, health: h })
                .await;
        }
    }

    async fn lead(&mut self, now: i64) -> Result<(), String> {
        let st = self.state.cluster.state().await;
        let settings = settings::load(&st);
        let node_name = self.state.node_name.clone();

        let mut ledger = Ledger::new(&st.alerts);
        let mut new_id = || uuid::Uuid::new_v4().to_string();

        // Becoming leader: take the baseline silently, announce the change.
        let mut pre_events = Vec::new();
        if !self.leading {
            self.leading = true;
            self.snapshot = Some(Snapshot::take(&st));
            let recent = self.leader_changes.recent(now).max(1);
            pre_events.push(events::leader_changed(&node_name, recent));
        }

        // Signals from nodes.
        let mut signals: Vec<&AlertSignal> = st.alert_inbox.values().collect();
        signals.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
        let taken: Vec<String> = signals.iter().map(|s| s.id.clone()).collect();
        for sig in signals {
            apply_signal(&mut ledger, &st, &settings, sig, now, &mut new_id);
        }

        // Standing conditions.
        let due = self
            .last_conditions
            .is_none_or(|t| t.elapsed() >= CONDITIONS_EVERY);
        if due {
            let summaries = tokio::time::timeout(
                CONDITIONS_TIMEOUT,
                crate::routes::instances::list_all(&self.state),
            )
            .await;
            if let Ok(summaries) = summaries {
                self.last_conditions = Some(Instant::now());
                let derived = conditions::derive(&st, &summaries, now, &settings);
                self.apply_conditions(&mut ledger, &st, &settings, &derived, now, &mut new_id);
            } else {
                warn!("reading the instances took too long, alerts keep their state this pass");
            }
        }

        // One-shot events from state changes.
        if let Some(prev) = &self.snapshot {
            pre_events.extend(events::diff(prev, &st));
        }
        for obs in pre_events {
            let Some(severity) = effective_severity(
                &st,
                &settings,
                &obs.event,
                obs.severity,
                obs.stack.as_deref(),
                obs.instance.as_deref(),
                &obs.subject,
            ) else {
                continue;
            };
            let mut obs = obs;
            obs.severity = severity;
            ledger.once(new_id(), &obs, 1, now);
        }

        // Owed messages, per the settings of this moment.
        let mut jobs = ledger.collect_sends(&settings);
        let digests = match digest_cutoff(&settings, now) {
            Some(cutoff) => ledger.collect_digest(&settings, cutoff),
            None => Vec::new(),
        };

        // Persist first, send after: a record written as announced is never
        // announced twice, whoever leads next.
        let mut dirty: Vec<String> = ledger.dirty.iter().cloned().collect();
        dirty.sort();
        let mut persisted: HashSet<String> = HashSet::new();
        for id in &dirty {
            let Some(rec) = ledger.records.get(id) else {
                continue;
            };
            if st.alerts.get(id) == Some(rec) {
                persisted.insert(id.clone());
                continue;
            }
            let resp = self
                .state
                .cluster
                .submit(Request::PutAlert(rec.clone()))
                .await
                .map_err(|e| e.to_string())?;
            if let Some(err) = resp.error {
                return Err(err);
            }
            persisted.insert(id.clone());
        }
        for id in &ledger.removed {
            if st.alerts.contains_key(id) {
                self.state
                    .cluster
                    .submit(Request::RemoveAlert { id: id.clone() })
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        if !taken.is_empty() {
            self.state
                .cluster
                .submit(Request::TakeAlertSignals { ids: taken })
                .await
                .map_err(|e| e.to_string())?;
        }
        self.snapshot = Some(Snapshot::take(&st));

        jobs.retain(|j| persisted.contains(&j.record));
        if !jobs.is_empty() || !digests.is_empty() {
            self.dispatch(&st, &settings, &ledger, jobs, digests).await;
        }

        if self.last_prune.elapsed() >= PRUNE_EVERY {
            self.last_prune = Instant::now();
            let before = now - settings.retention.as_secs() as i64;
            let _ = self
                .state
                .cluster
                .submit(Request::PruneAlerts {
                    before,
                    max: FEED_SOFT_CAP,
                })
                .await;
            self.prune_health(&st, &settings).await;
        }
        Ok(())
    }

    fn apply_conditions(
        &mut self,
        ledger: &mut Ledger,
        st: &ClusterState,
        settings: &AlertSettings,
        derived: &Derived,
        now: i64,
        new_id: &mut dyn FnMut() -> String,
    ) {
        let mut present: HashSet<String> = HashSet::new();
        let mut silent: HashSet<String> = HashSet::new();
        for obs in &derived.observed {
            if let Some(min) = derived.min_age.get(&obs.dedupe) {
                let first = *self.first_seen.entry(obs.dedupe.clone()).or_insert(now);
                if now - first < *min {
                    // Not worth an alert yet, but not gone either.
                    present.insert(obs.dedupe.clone());
                    continue;
                }
            }
            match effective_severity(
                st,
                settings,
                &obs.event,
                obs.severity,
                obs.stack.as_deref(),
                obs.instance.as_deref(),
                &obs.subject,
            ) {
                Some(severity) => {
                    present.insert(obs.dedupe.clone());
                    let mut o = obs.clone();
                    o.severity = severity;
                    ledger.observe(&o, now, settings.warning_debounce, new_id);
                }
                None => {
                    silent.insert(obs.dedupe.clone());
                }
            }
        }
        let observed: HashSet<&String> = derived.observed.iter().map(|o| &o.dedupe).collect();
        self.first_seen.retain(|d, _| observed.contains(d));
        ledger.close_absent(
            &conditions::is_derived,
            &present,
            &silent,
            &derived.held_instances,
            &mut self.absent,
            now,
        );
    }

    async fn dispatch(
        &self,
        st: &ClusterState,
        settings: &AlertSettings,
        ledger: &Ledger,
        jobs: Vec<SendJob>,
        digests: Vec<super::lifecycle::DigestJob>,
    ) {
        let key = self.state.key.read().await.clone();
        let redactor = match &key {
            Some(k) => Redactor::from_state(st, k).ok(),
            None => None,
        };
        let url = ui_url(&self.state, settings).await;
        let cluster_name = self.state.cluster_config.read().await.cluster.name.clone();
        let mut messages: HashMap<(String, &'static str), AlertMessage> = HashMap::new();
        for job in jobs {
            let Some(rec) = ledger.records.get(&job.record) else {
                continue;
            };
            let msg = messages
                .entry((job.record.clone(), job.kind.label()))
                .or_insert_with(|| message(rec, job.kind, url.as_deref(), redactor.as_ref()))
                .clone();
            let n = Notification {
                cluster: cluster_name.clone(),
                ui_url: url.clone(),
                body: Body::Alert(msg),
            };
            let edit_message_id = if job.kind == Kind::Resolved {
                rec.deliveries
                    .iter()
                    .rev()
                    .find(|d| d.ok && d.destination == job.destination && d.message_id.is_some())
                    .and_then(|d| d.message_id.clone())
            } else {
                None
            };
            self.sender.spawn(
                vec![job.record],
                job.destination,
                job.kind.label(),
                n,
                edit_message_id,
                self.tx.clone(),
            );
        }
        for d in digests {
            let alerts: Vec<AlertMessage> = d
                .records
                .iter()
                .filter_map(|id| ledger.records.get(id))
                .map(|r| message(r, Kind::Event, url.as_deref(), redactor.as_ref()))
                .collect();
            if alerts.is_empty() {
                continue;
            }
            let n = Notification {
                cluster: cluster_name.clone(),
                ui_url: url.clone(),
                body: Body::Digest { alerts },
            };
            self.sender
                .spawn(d.records, d.destination, "digest", n, None, self.tx.clone());
        }
    }

    /// Health of destinations that no longer exist goes away with them.
    async fn prune_health(&self, st: &ClusterState, settings: &AlertSettings) {
        for name in st.alert_destinations.keys() {
            if !settings.destinations.iter().any(|d| &d.name == name) {
                let _ = self
                    .state
                    .cluster
                    .submit(Request::RemoveDestinationHealth { name: name.clone() })
                    .await;
            }
        }
    }
}

fn missing(_id: &str) -> AlertRecord {
    // Marker for "record vanished (pruned) while a delivery was in flight".
    AlertRecord {
        id: "-".into(),
        event: String::new(),
        dedupe: String::new(),
        category: String::new(),
        severity: AlertSeverity::Info,
        state: cl_raft::alerts::AlertState::Event,
        title: String::new(),
        detail: String::new(),
        subject: String::new(),
        stack: None,
        instance: None,
        node: None,
        opened_at: 0,
        last_at: 0,
        resolved_at: None,
        count: 0,
        escalated_from: None,
        sent_severity: None,
        resolved_sent: false,
        digested: false,
        links: Vec::new(),
        excerpt: Vec::new(),
        deliveries: Vec::new(),
    }
}

/// Links a signal's record offers, by what it is about.
fn links_for(sig: &AlertSignal) -> Vec<cl_raft::alerts::AlertLink> {
    use cl_raft::alerts::AlertLink as L;
    let mut out = Vec::new();
    if let Some(i) = &sig.instance {
        out.push(L {
            label: "Open instance".into(),
            path: format!("/instances/{i}"),
        });
        out.push(L {
            label: "Logs".into(),
            path: format!("/logs?instance={i}"),
        });
    } else if let Some(n) = &sig.node {
        out.push(L {
            label: "Open node".into(),
            path: format!("/nodes/{n}"),
        });
    }
    out
}

/// Fold one signal into the ledger.
pub fn apply_signal(
    ledger: &mut Ledger,
    st: &ClusterState,
    settings: &AlertSettings,
    sig: &AlertSignal,
    now: i64,
    new_id: &mut dyn FnMut() -> String,
) {
    let Some(def) = catalog::find(&sig.event) else {
        return;
    };
    let default = sig
        .severity
        .unwrap_or_else(|| super::sev_back(def.severity));
    let mut obs = Observed {
        event: sig.event.clone(),
        dedupe: sig.dedupe.clone(),
        severity: default,
        title: sig.title.clone(),
        detail: sig.detail.clone(),
        subject: sig.subject.clone(),
        stack: sig.stack.clone(),
        instance: sig.instance.clone(),
        node: sig.node.clone(),
        links: links_for(sig),
        excerpt: sig.excerpt.clone(),
    };
    match sig.action {
        SignalAction::Resolve => ledger.resolve(&sig.dedupe, now, false),
        SignalAction::Open => {
            match effective_severity(
                st,
                settings,
                &sig.event,
                default,
                sig.stack.as_deref(),
                sig.instance.as_deref(),
                &sig.subject,
            ) {
                Some(s) => {
                    obs.severity = s;
                    ledger.observe(&obs, sig.at.min(now), settings.warning_debounce, new_id);
                }
                None => ledger.resolve(&sig.dedupe, now, true),
            }
        }
        SignalAction::Once => {
            let mut default = default;
            if sig.event == "instance.oom_killed" {
                let prior = ledger
                    .records
                    .values()
                    .filter(|r| {
                        r.event == sig.event
                            && r.instance == sig.instance
                            && now - r.opened_at <= OOM_WINDOW_SECS
                    })
                    .count();
                if prior + 1 >= 3 {
                    default = AlertSeverity::Critical;
                }
            }
            let Some(s) = effective_severity(
                st,
                settings,
                &sig.event,
                default,
                sig.stack.as_deref(),
                sig.instance.as_deref(),
                &sig.subject,
            ) else {
                return;
            };
            obs.severity = s;
            // Deterministic id: replaying the same signal overwrites.
            ledger.once(format!("sig-{}", sig.id), &obs, sig.count, sig.at);
        }
    }
}

/// The wire form of a record for one transition.
pub fn message(
    r: &AlertRecord,
    kind: Kind,
    ui_url: Option<&str>,
    redactor: Option<&Redactor>,
) -> AlertMessage {
    let red = |s: &str| match redactor {
        Some(x) => x.redact(s),
        None => s.to_string(),
    };
    let links = match ui_url {
        Some(base) => r
            .links
            .iter()
            .map(|l| Link {
                label: l.label.clone(),
                url: format!("{base}{}", l.path),
            })
            .collect(),
        None => Vec::new(),
    };
    AlertMessage {
        id: r.id.clone(),
        event: r.event.clone(),
        category: r.category.clone(),
        severity: sev(r.severity),
        phase: kind.phase(),
        title: red(&r.title),
        detail: red(&r.detail),
        subject: r.subject.clone(),
        stack: r.stack.clone(),
        instance: r.instance.clone(),
        node: r.node.clone(),
        opened_at: r.opened_at,
        at: match kind {
            Kind::Resolved => r.resolved_at.unwrap_or(r.last_at),
            _ => r.last_at,
        },
        count: r.count,
        links,
        excerpt: r.excerpt.iter().map(|l| red(l)).collect(),
    }
}

/// The most recent time `digest_at` struck in the settings' zone, as unix
/// seconds, or `None` if it cannot be worked out.
pub fn digest_cutoff(settings: &AlertSettings, now: i64) -> Option<i64> {
    use jiff::tz::TimeZone;
    let tz = match &settings.timezone {
        Some(name) => TimeZone::get(name).ok()?,
        None => TimeZone::system(),
    };
    let (h, m) = settings.digest_at.split_once(':')?;
    let (h, m): (i8, i8) = (h.parse().ok()?, m.parse().ok()?);
    let at = jiff::Timestamp::from_second(now).ok()?.to_zoned(tz.clone());
    let today = at.date().at(h, m, 0, 0).to_zoned(tz.clone()).ok()?;
    let strike = if today.timestamp() <= at.timestamp() {
        today
    } else {
        at.date()
            .yesterday()
            .ok()?
            .at(h, m, 0, 0)
            .to_zoned(tz)
            .ok()?
    };
    Some(strike.timestamp().as_second())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_alert::{Destination, DestinationKind, Severity};
    use cl_raft::alerts::AlertState;

    fn settings_with_dest(severities: &[Severity]) -> AlertSettings {
        let mut s = AlertSettings::default();
        s.destinations.push(Destination {
            name: "main".into(),
            enabled: true,
            kind: DestinationKind::Webhook,
            severities: severities.iter().copied().collect(),
            digest: false,
            categories: vec![],
            stacks: vec![],
        });
        s
    }

    fn signal(event: &str, dedupe: &str, action: SignalAction, at: i64) -> AlertSignal {
        let mut s = super::super::new_signal(event, dedupe.into(), action);
        s.at = at;
        s.instance = Some("web.app.0".into());
        s.stack = Some("web".into());
        s
    }

    #[test]
    fn the_third_oom_kill_within_the_hour_is_critical() {
        let st = ClusterState::default();
        let s = AlertSettings::default();
        let mut ledger = Ledger::default();
        let mut n = 0;
        let mut id = || {
            n += 1;
            format!("x{n}")
        };
        for (i, at) in [1000, 1100, 1200].iter().enumerate() {
            let mut sig = signal("instance.oom_killed", "oom", SignalAction::Once, *at);
            sig.id = format!("s{i}");
            apply_signal(&mut ledger, &st, &s, &sig, *at, &mut id);
        }
        let sev: Vec<_> = ["sig-s0", "sig-s1", "sig-s2"]
            .iter()
            .map(|k| ledger.records[*k].severity)
            .collect();
        assert_eq!(
            sev,
            vec![
                AlertSeverity::Warning,
                AlertSeverity::Warning,
                AlertSeverity::Critical
            ]
        );
        assert!(ledger
            .records
            .values()
            .all(|r| r.state == AlertState::Event));
    }

    #[test]
    fn replaying_a_signal_overwrites_instead_of_duplicating() {
        let st = ClusterState::default();
        let s = AlertSettings::default();
        let mut ledger = Ledger::default();
        let mut id = || "unused".to_string();
        let sig = signal("log.match", "l", SignalAction::Once, 10);
        apply_signal(&mut ledger, &st, &s, &sig, 10, &mut id);
        apply_signal(&mut ledger, &st, &s, &sig, 12, &mut id);
        assert_eq!(ledger.records.len(), 1);
    }

    #[test]
    fn settings_changes_in_state_change_routing_without_a_restart() {
        // The engine reads `settings::load(state)` on every pass; model two
        // passes over one record with an edit in between.
        let mut st = ClusterState::default();
        let mut ledger = Ledger::default();
        let mut n = 0;
        let mut id = move || {
            n += 1;
            format!("r{n}")
        };
        let sig = signal("node.disk_low", "d", SignalAction::Open, 0);
        let s0 = settings::load(&st);
        apply_signal(&mut ledger, &st, &s0, &sig, 0, &mut id);
        // No destination yet: swallowed.
        assert!(ledger.collect_sends(&settings::load(&st)).is_empty());

        let mut edited = settings_with_dest(&[Severity::Warning, Severity::Critical]);
        edited.warning_debounce = Duration::ZERO;
        let r = st.apply(&Request::PutConfig {
            name: cl_alert::settings::SETTINGS_CONFIG.into(),
            content: settings::encode(&edited),
            owner: None,
        });
        assert!(r.error.is_none());
        let mut sig2 = signal("node.disk_low", "d2", SignalAction::Open, 1);
        sig2.severity = Some(AlertSeverity::Critical);
        let s1 = settings::load(&st);
        apply_signal(&mut ledger, &st, &s1, &sig2, 1, &mut id);
        let jobs = ledger.collect_sends(&s1);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].destination, "main");

        // Muting the event in the settings drops the next occurrence.
        edited.events.insert(
            "node.disk_low".into(),
            cl_alert::EventOverride {
                severity: None,
                route: Some(cl_alert::Route::Muted),
            },
        );
        st.apply(&Request::PutConfig {
            name: cl_alert::settings::SETTINGS_CONFIG.into(),
            content: settings::encode(&edited),
            owner: None,
        });
        let s2 = settings::load(&st);
        let mut ledger2 = Ledger::default();
        apply_signal(&mut ledger2, &st, &s2, &sig2, 2, &mut id);
        assert!(ledger2.records.is_empty());
    }

    #[test]
    fn digest_cutoff_is_the_latest_strike_of_the_configured_time() {
        let mut s = AlertSettings {
            timezone: Some("UTC".into()),
            digest_at: "09:00".into(),
            ..Default::default()
        };
        let day = 86_400;
        let base = 20_000 * day;
        assert_eq!(digest_cutoff(&s, base + 10 * 3600), Some(base + 9 * 3600));
        assert_eq!(
            digest_cutoff(&s, base + 8 * 3600),
            Some(base - day + 9 * 3600)
        );
        s.timezone = Some("Not/AZone".into());
        assert_eq!(digest_cutoff(&s, base), None);
    }

    #[test]
    fn messages_are_redacted_and_links_made_absolute() {
        let mut r = missing("x");
        r.id = "a".into();
        r.detail = "token hunter2secret leaked".into();
        r.links = vec![cl_raft::alerts::AlertLink {
            label: "Open".into(),
            path: "/instances/a".into(),
        }];
        let red = Redactor::from_plaintexts(vec![("t".into(), b"hunter2secret".to_vec())]).unwrap();
        let m = message(&r, Kind::Opened, Some("https://c.example"), Some(&red));
        assert!(!m.detail.contains("hunter2secret"));
        assert_eq!(m.links[0].url, "https://c.example/instances/a");
        let m = message(&r, Kind::Opened, None, None);
        assert!(m.links.is_empty());
    }

    #[tokio::test]
    async fn drain_outcomes_records_message_id_on_alert_deliveries() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = Outcome {
            records: vec!["rec1".into()],
            destination: "discord1".into(),
            phase: "opened",
            at: 100,
            ok: true,
            error: None,
            attempts: 1,
            counts_for_health: true,
            message_id: Some("discord-msg-999".into()),
        };
        tx.send(outcome).unwrap();

        let mut st = ClusterState::default();
        let mut rec = missing("rec1");
        rec.id = "rec1".into();
        st.alerts.insert("rec1".into(), rec);

        // Verify the outcome drained into delivery retains message_id
        let o = rx.recv().await.unwrap();
        let r = st.alerts.get_mut("rec1").unwrap();
        r.deliveries.push(AlertDelivery {
            destination: o.destination.clone(),
            at: o.at,
            ok: o.ok,
            error: o.error.clone(),
            attempts: o.attempts,
            phase: o.phase.to_string(),
            message_id: o.message_id.clone(),
        });

        assert_eq!(r.deliveries.len(), 1);
        assert_eq!(
            r.deliveries[0].message_id.as_deref(),
            Some("discord-msg-999")
        );
    }
}
