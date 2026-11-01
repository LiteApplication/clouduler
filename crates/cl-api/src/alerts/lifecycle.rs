//! The alert lifecycle as pure functions over a working copy of the feed.
//!
//! The engine loads the records from raft, feeds the [`Ledger`] what it
//! observed (conditions, signals), asks it which messages are owed, writes
//! the changed records back and only then sends. Every "was this already
//! announced" decision rests on fields stored in the record
//! (`sent_severity`, `resolved_sent`, `digested`), never on engine memory, so
//! a new leader picks up exactly where the old one stopped.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use cl_alert::catalog;
use cl_alert::{AlertSettings, Phase, Severity};
use cl_config::AlertSeverity;
use cl_raft::alerts::{AlertLink, AlertRecord, AlertState};

use super::route;
use super::sev;

/// Passes a condition must be missing before its alert resolves, so one
/// missed reading does not flap an alert.
pub const ABSENT_PASSES: u32 = 2;

/// What the world looks like for one problem right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub event: String,
    pub dedupe: String,
    pub severity: AlertSeverity,
    pub title: String,
    pub detail: String,
    pub subject: String,
    pub stack: Option<String>,
    pub instance: Option<String>,
    pub node: Option<String>,
    pub links: Vec<AlertLink>,
    pub excerpt: Vec<String>,
}

/// Why a message goes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Opened,
    Escalated(Severity),
    Resolved,
    Event,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Opened => "opened",
            Kind::Escalated(_) => "escalated",
            Kind::Resolved => "resolved",
            Kind::Event => "event",
        }
    }

    pub fn phase(self) -> Phase {
        match self {
            Kind::Opened => Phase::Opened,
            Kind::Escalated(p) => Phase::Escalated { previous: p },
            Kind::Resolved => Phase::Resolved,
            Kind::Event => Phase::Event,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendJob {
    pub record: String,
    pub destination: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestJob {
    pub destination: String,
    pub records: Vec<String>,
}

/// Working copy of the feed with change tracking.
#[derive(Debug, Default)]
pub struct Ledger {
    pub records: BTreeMap<String, AlertRecord>,
    pub dirty: BTreeSet<String>,
    pub removed: BTreeSet<String>,
    open: HashMap<String, String>,
}

impl Ledger {
    pub fn new(records: &BTreeMap<String, AlertRecord>) -> Self {
        let mut open = HashMap::new();
        for r in records.values().filter(|r| r.state.is_open()) {
            open.insert(r.dedupe.clone(), r.id.clone());
        }
        Self {
            records: records.clone(),
            open,
            ..Default::default()
        }
    }

    pub fn is_open(&self, dedupe: &str) -> bool {
        self.open.contains_key(dedupe)
    }

    fn touch(&mut self, id: &str) {
        self.dirty.insert(id.to_string());
    }

    /// A problem that holds right now: open it, refresh it, escalate it.
    pub fn observe(
        &mut self,
        obs: &Observed,
        now: i64,
        debounce: std::time::Duration,
        new_id: &mut dyn FnMut() -> String,
    ) {
        let Some(def) = catalog::find(&obs.event) else {
            return;
        };
        let debounce = debounce.as_secs() as i64;
        let Some(id) = self.open.get(&obs.dedupe).cloned() else {
            let state = match obs.severity {
                AlertSeverity::Warning if debounce > 0 => AlertState::Pending,
                _ => AlertState::Active,
            };
            let id = new_id();
            let record = AlertRecord {
                id: id.clone(),
                event: obs.event.clone(),
                dedupe: obs.dedupe.clone(),
                category: def.category.as_str().to_string(),
                severity: obs.severity,
                state,
                title: obs.title.clone(),
                detail: obs.detail.clone(),
                subject: obs.subject.clone(),
                stack: obs.stack.clone(),
                instance: obs.instance.clone(),
                node: obs.node.clone(),
                opened_at: now,
                last_at: now,
                resolved_at: None,
                count: 1,
                escalated_from: None,
                sent_severity: None,
                resolved_sent: false,
                digested: false,
                links: obs.links.clone(),
                excerpt: obs.excerpt.clone(),
                deliveries: Vec::new(),
            };
            self.open.insert(obs.dedupe.clone(), id.clone());
            self.records.insert(id.clone(), record);
            self.touch(&id);
            return;
        };
        let Some(r) = self.records.get_mut(&id) else {
            return;
        };
        let mut changed = false;
        if r.title != obs.title || r.detail != obs.detail || r.links != obs.links {
            r.title = obs.title.clone();
            r.detail = obs.detail.clone();
            r.links = obs.links.clone();
            changed = true;
        }
        if r.node != obs.node {
            r.node = obs.node.clone();
            changed = true;
        }
        if obs.severity != r.severity {
            if obs.severity > r.severity {
                r.escalated_from = Some(r.severity);
            } else {
                r.escalated_from = None;
            }
            r.severity = obs.severity;
            changed = true;
        }
        if r.state == AlertState::Pending
            && (r.severity != AlertSeverity::Warning || now - r.opened_at >= debounce)
        {
            r.state = AlertState::Active;
            changed = true;
        }
        if changed {
            r.last_at = now;
            self.touch(&id);
        }
    }

    /// The problem is gone. A warning nobody was told about is dropped
    /// without a trace; anything else becomes `Resolved`. `silent` marks it
    /// as not needing a resolved message (muted since, or never announced).
    pub fn resolve(&mut self, dedupe: &str, now: i64, silent: bool) {
        let Some(id) = self.open.remove(dedupe) else {
            return;
        };
        let Some(r) = self.records.get_mut(&id) else {
            return;
        };
        if r.state == AlertState::Pending {
            self.records.remove(&id);
            self.dirty.remove(&id);
            self.removed.insert(id);
            return;
        }
        r.state = AlertState::Resolved;
        r.resolved_at = Some(now);
        r.last_at = now;
        if silent {
            r.resolved_sent = true;
        }
        self.touch(&id);
    }

    /// Resolve open records of `derived` events that are no longer observed.
    /// `present` are the dedupes observed this pass, `silent` the ones that
    /// stopped because they are muted now, `held_instances` instances whose
    /// state could not be read (their alerts are kept as they are).
    pub fn close_absent(
        &mut self,
        derived: &dyn Fn(&str) -> bool,
        present: &HashSet<String>,
        silent: &HashSet<String>,
        held_instances: &HashSet<String>,
        absent: &mut HashMap<String, u32>,
        now: i64,
    ) {
        let candidates: Vec<(String, Option<String>)> = self
            .open
            .keys()
            .filter_map(|d| {
                let r = self.records.get(self.open.get(d)?)?;
                derived(&r.event).then(|| (d.clone(), r.instance.clone()))
            })
            .collect();
        for (dedupe, instance) in candidates {
            if present.contains(&dedupe) {
                absent.remove(&dedupe);
                continue;
            }
            if instance
                .as_ref()
                .is_some_and(|i| held_instances.contains(i))
            {
                continue;
            }
            let quiet = silent.contains(&dedupe);
            let n = absent.entry(dedupe.clone()).or_insert(0);
            *n += 1;
            if quiet || *n >= ABSENT_PASSES {
                absent.remove(&dedupe);
                self.resolve(&dedupe, now, quiet);
            }
        }
        absent.retain(|d, _| self.open.contains_key(d));
    }

    /// Record a one-shot event under a fixed `id` (so a replay overwrites).
    pub fn once(&mut self, id: String, obs: &Observed, count: u32, at: i64) {
        let Some(def) = catalog::find(&obs.event) else {
            return;
        };
        let record = AlertRecord {
            id: id.clone(),
            event: obs.event.clone(),
            dedupe: obs.dedupe.clone(),
            category: def.category.as_str().to_string(),
            severity: obs.severity,
            state: AlertState::Event,
            title: obs.title.clone(),
            detail: obs.detail.clone(),
            subject: obs.subject.clone(),
            stack: obs.stack.clone(),
            instance: obs.instance.clone(),
            node: obs.node.clone(),
            opened_at: at,
            last_at: at,
            resolved_at: None,
            count: count.max(1),
            escalated_from: None,
            sent_severity: None,
            resolved_sent: false,
            digested: false,
            links: obs.links.clone(),
            excerpt: obs.excerpt.clone(),
            deliveries: Vec::new(),
        };
        self.records.insert(id.clone(), record);
        self.touch(&id);
    }

    /// Messages owed now, per the settings of the moment. Marks the records
    /// as announced (the caller must persist them before sending).
    pub fn collect_sends(&mut self, settings: &AlertSettings) -> Vec<SendJob> {
        let mut jobs = Vec::new();
        let ids: Vec<String> = self.records.keys().cloned().collect();
        for id in ids {
            let r = &self.records[&id];
            let mut update: Option<AlertRecord> = None;
            match r.state {
                AlertState::Pending => {}
                AlertState::Active | AlertState::Event => {
                    let is_event = r.state == AlertState::Event;
                    let current = sev(r.severity);
                    let owed = match r.sent_severity {
                        None => true,
                        Some(prev) => !is_event && prev < r.severity,
                    };
                    if owed {
                        let t = route::targets_for_record(settings, r);
                        // Info waits for the digest unless routed explicitly.
                        let wait_for_digest = current == Severity::Info && t.immediate.is_empty();
                        if !wait_for_digest {
                            let mut n = r.clone();
                            n.sent_severity = Some(r.severity);
                            let kind = match (is_event, r.sent_severity) {
                                (true, _) => Some(Kind::Event),
                                (false, None) => Some(Kind::Opened),
                                (false, Some(prev)) if settings.renotify_on_escalation => {
                                    Some(Kind::Escalated(sev(prev)))
                                }
                                _ => None,
                            };
                            if let Some(kind) = kind {
                                for d in &t.immediate {
                                    jobs.push(SendJob {
                                        record: id.clone(),
                                        destination: d.clone(),
                                        kind,
                                    });
                                }
                            }
                            update = Some(n);
                        }
                    }
                }
                AlertState::Resolved => {
                    if !r.resolved_sent {
                        let mut n = r.clone();
                        n.resolved_sent = true;
                        if settings.enabled && settings.notify_resolved && r.sent_severity.is_some()
                        {
                            for d in route::resolved_targets(settings, &r.deliveries) {
                                jobs.push(SendJob {
                                    record: id.clone(),
                                    destination: d,
                                    kind: Kind::Resolved,
                                });
                            }
                        }
                        update = Some(n);
                    }
                }
            }
            if let Some(n) = update {
                self.records.insert(id.clone(), n);
                self.touch(&id);
            }
        }
        jobs
    }

    /// Info records opened at or before `cutoff` that no digest has covered.
    /// Marks them covered. One job per destination that wants them.
    pub fn collect_digest(&mut self, settings: &AlertSettings, cutoff: i64) -> Vec<DigestJob> {
        let mut per_dest: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let ids: Vec<String> = self
            .records
            .values()
            .filter(|r| {
                r.severity == AlertSeverity::Info
                    && !r.digested
                    && r.opened_at <= cutoff
                    && r.state != AlertState::Pending
            })
            .map(|r| r.id.clone())
            .collect();
        for id in ids {
            let r = &mut self.records.get_mut(&id).expect("listed above");
            r.digested = true;
            if r.state != AlertState::Resolved {
                let t = route::targets_for_record(settings, r);
                for d in t.digest {
                    per_dest.entry(d).or_default().push(id.clone());
                }
            }
            self.touch(&id);
        }
        per_dest
            .into_iter()
            .map(|(destination, mut records)| {
                records.sort_by_key(|id| self.records[id].opened_at);
                DigestJob {
                    destination,
                    records,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_alert::{Destination, DestinationKind};
    use std::time::Duration;

    fn obs(event: &str, dedupe: &str, severity: AlertSeverity) -> Observed {
        Observed {
            event: event.into(),
            dedupe: dedupe.into(),
            severity,
            title: "t".into(),
            detail: "d".into(),
            subject: "s".into(),
            stack: None,
            instance: None,
            node: None,
            links: vec![],
            excerpt: vec![],
        }
    }

    fn settings() -> AlertSettings {
        let mut s = AlertSettings::default();
        s.destinations.push(Destination {
            name: "main".into(),
            enabled: true,
            kind: DestinationKind::Webhook,
            severities: [Severity::Critical, Severity::Warning].into(),
            digest: true,
            categories: vec![],
            stacks: vec![],
        });
        s
    }

    fn ids() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("r{n}")
        }
    }

    const DEBOUNCE: Duration = Duration::from_secs(300);

    #[test]
    fn warning_waits_for_the_debounce_then_opens_once() {
        let mut l = Ledger::default();
        let o = obs("instance.unhealthy", "u:a", AlertSeverity::Warning);
        let mut id = ids();
        l.observe(&o, 1000, DEBOUNCE, &mut id);
        assert_eq!(l.records["r1"].state, AlertState::Pending);
        assert!(l.collect_sends(&settings()).is_empty());
        l.observe(&o, 1100, DEBOUNCE, &mut id);
        assert!(l.collect_sends(&settings()).is_empty());
        l.observe(&o, 1300, DEBOUNCE, &mut id);
        assert_eq!(l.records["r1"].state, AlertState::Active);
        let jobs = l.collect_sends(&settings());
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, Kind::Opened);
        // Announced: a second collect (or a new leader reading the record) sends nothing.
        assert!(l.collect_sends(&settings()).is_empty());
        let mut next = Ledger::new(&l.records);
        assert!(next.collect_sends(&settings()).is_empty());
    }

    #[test]
    fn a_pending_warning_that_resolves_leaves_no_trace() {
        let mut l = Ledger::default();
        let mut id = ids();
        l.observe(
            &obs("instance.unhealthy", "u:a", AlertSeverity::Warning),
            0,
            DEBOUNCE,
            &mut id,
        );
        l.resolve("u:a", 10, false);
        assert!(l.records.is_empty());
        assert!(l.removed.contains("r1"));
        assert!(!l.dirty.contains("r1"));
    }

    #[test]
    fn critical_is_immediate_escalation_renotifies_and_resolution_follows() {
        let mut l = Ledger::default();
        let mut id = ids();
        l.observe(
            &obs("node.disk_low", "d", AlertSeverity::Warning),
            0,
            Duration::ZERO,
            &mut id,
        );
        let jobs = l.collect_sends(&settings());
        assert_eq!(jobs[0].kind, Kind::Opened);
        l.observe(
            &obs("node.disk_low", "d", AlertSeverity::Critical),
            5,
            DEBOUNCE,
            &mut id,
        );
        assert_eq!(l.records["r1"].escalated_from, Some(AlertSeverity::Warning));
        let jobs = l.collect_sends(&settings());
        assert_eq!(jobs[0].kind, Kind::Escalated(Severity::Warning));
        // Pretend the open delivery succeeded.
        l.records
            .get_mut("r1")
            .unwrap()
            .deliveries
            .push(cl_raft::alerts::AlertDelivery {
                destination: "main".into(),
                at: 1,
                ok: true,
                error: None,
                attempts: 1,
                phase: "opened".into(),
            });
        l.resolve("d", 50, false);
        let jobs = l.collect_sends(&settings());
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, Kind::Resolved);
        assert!(l.collect_sends(&settings()).is_empty());
    }

    #[test]
    fn escalation_without_renotify_is_swallowed() {
        let mut s = settings();
        s.renotify_on_escalation = false;
        let mut l = Ledger::default();
        let mut id = ids();
        l.observe(
            &obs("node.disk_low", "d", AlertSeverity::Warning),
            0,
            Duration::ZERO,
            &mut id,
        );
        l.collect_sends(&s);
        l.observe(
            &obs("node.disk_low", "d", AlertSeverity::Critical),
            5,
            DEBOUNCE,
            &mut id,
        );
        assert!(l.collect_sends(&s).is_empty());
        assert_eq!(l.records["r1"].sent_severity, Some(AlertSeverity::Critical));
    }

    #[test]
    fn nothing_flaps_on_a_single_missed_reading() {
        let mut l = Ledger::default();
        let mut id = ids();
        l.observe(
            &obs("instance.crash_loop", "c", AlertSeverity::Critical),
            0,
            DEBOUNCE,
            &mut id,
        );
        let derived = |_: &str| true;
        let mut absent = HashMap::new();
        let none = HashSet::new();
        l.close_absent(&derived, &none, &none, &none, &mut absent, 10);
        assert!(l.records["r1"].state.is_open());
        let present: HashSet<String> = ["c".to_string()].into();
        l.close_absent(&derived, &present, &none, &none, &mut absent, 20);
        l.close_absent(&derived, &none, &none, &none, &mut absent, 30);
        assert!(l.records["r1"].state.is_open());
        l.close_absent(&derived, &none, &none, &none, &mut absent, 40);
        assert_eq!(l.records["r1"].state, AlertState::Resolved);
    }

    #[test]
    fn held_instances_keep_their_alerts() {
        let mut l = Ledger::default();
        let mut id = ids();
        let mut o = obs("instance.crash_loop", "c", AlertSeverity::Critical);
        o.instance = Some("a.b.0".into());
        l.observe(&o, 0, DEBOUNCE, &mut id);
        let none = HashSet::new();
        let held: HashSet<String> = ["a.b.0".to_string()].into();
        let mut absent = HashMap::new();
        for t in 1..5 {
            l.close_absent(&|_| true, &none, &none, &held, &mut absent, t);
        }
        assert!(l.records["r1"].state.is_open());
    }

    #[test]
    fn info_is_digested_once_and_events_send_immediately() {
        let s = settings();
        let mut l = Ledger::default();
        l.once(
            "e1".into(),
            &obs("cluster.node_joined", "x", AlertSeverity::Info),
            1,
            100,
        );
        l.once(
            "e2".into(),
            &obs("auth.api_key_created", "y", AlertSeverity::Warning),
            1,
            100,
        );
        let jobs = l.collect_sends(&s);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].record, "e2");
        let digest = l.collect_digest(&s, 99);
        assert!(digest.is_empty());
        let digest = l.collect_digest(&s, 100);
        assert_eq!(digest.len(), 1);
        assert_eq!(digest[0].records, vec!["e1"]);
        assert!(l.collect_digest(&s, 1000).is_empty());
    }

    #[test]
    fn disabling_alerts_swallows_instead_of_queueing() {
        let mut s = settings();
        s.enabled = false;
        let mut l = Ledger::default();
        let mut id = ids();
        l.observe(
            &obs("node.disk_low", "d", AlertSeverity::Critical),
            0,
            DEBOUNCE,
            &mut id,
        );
        assert!(l.collect_sends(&s).is_empty());
        s.enabled = true;
        assert!(l.collect_sends(&s).is_empty());
    }
}
