//! Log watches: raise `log.match` when a container prints a line one of its
//! `x-clouduler.alerts.logs` rules matches.
//!
//! Only containers on this node whose effective alert settings are enabled
//! and carry log rules get a follower. A container without rules has no
//! stream, no task and no per-line work: the supervisor only lists containers
//! every [`SCAN_EVERY`] and compares rule fingerprints.
//!
//! A follower compiles a container's rules once into a `RegexSet`, so a line
//! costs one `is_match`; the rule lookup only happens on a hit. Signals go
//! through a bounded channel to a single sender task, so a flood of matches
//! can never block the docker stream reader, and a per-node limit caps how
//! many are queued per minute.

use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use cl_api::alerts::{new_signal, push_signal};
use cl_config::{LogRule, LogStream, StackAlerts, StackFile};
use cl_docker::{Docker, LogOptions, Tail};
use cl_raft::alerts::{AlertSignal, SignalAction};
use cl_raft::ids::InstanceId;
use cl_raft::ClusterNode;
use futures::StreamExt;
use regex::{RegexSet, RegexSetBuilder};
use tokio::sync::RwLock;
use tokio::time::Instant;
use tracing::{debug, warn};

const SCAN_EVERY: Duration = Duration::from_secs(10);
/// Lines longer than this are cut before matching.
const MAX_LINE: usize = 8 * 1024;
/// The excerpt in an alert is trimmed to this many characters per line.
const EXCERPT_CHARS: usize = 500;
/// Matches of one rule within this window of the first are added to its excerpt.
const GATHER: Duration = Duration::from_secs(1);
const MAX_EXCERPT_LINES: usize = 3;
/// Signals one node queues per minute, over all containers.
const MAX_PER_MINUTE: usize = 30;
const QUEUE: usize = 64;

/// A container's rules, compiled. `None` when nothing is left to match.
pub struct Compiled {
    set: RegexSet,
    rules: Vec<LogRule>,
}

impl Compiled {
    pub fn new(rules: &[LogRule]) -> Option<Self> {
        let mut patterns = Vec::new();
        let mut kept = Vec::new();
        for rule in rules {
            let body = match (&rule.literal, &rule.regex) {
                (Some(l), _) => regex::escape(l),
                (None, Some(r)) => r.clone(),
                _ => continue,
            };
            let body = if rule.ignore_case {
                format!("(?i){body}")
            } else {
                body
            };
            // Validation already checked these; a rule that still fails to
            // compile is dropped rather than taking the others with it.
            if regex::Regex::new(&body).is_err() {
                continue;
            }
            patterns.push(body);
            kept.push(rule.clone());
        }
        if patterns.is_empty() {
            return None;
        }
        let set = RegexSetBuilder::new(&patterns).build().ok()?;
        Some(Self { set, rules: kept })
    }

    /// Indexes of the rules `line` matches on `stream`.
    pub fn hits(&self, stream: cl_docker::LogStream, line: &str) -> Vec<usize> {
        if !self.set.is_match(line) {
            return Vec::new();
        }
        self.set
            .matches(line)
            .into_iter()
            .filter(|&i| {
                matches!(
                    (self.rules[i].stream, stream),
                    (LogStream::Both, _)
                        | (LogStream::Stdout, cl_docker::LogStream::Stdout)
                        | (LogStream::Stderr, cl_docker::LogStream::Stderr)
                )
            })
            .collect()
    }
}

/// What a rule has done lately on one container.
#[derive(Default)]
struct RuleState {
    last_raised: Option<Instant>,
    suppressed: u32,
    pending: Option<Pending>,
}

struct Pending {
    deadline: Instant,
    lines: Vec<String>,
    /// How many earlier matches this alert reports.
    earlier: u32,
}

/// What `RuleState::on_match` decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Counted only (cooldown), or added to a pending excerpt.
    Held,
    /// Start gathering an excerpt; flush at the pending deadline.
    Started,
}

impl RuleState {
    fn on_match(&mut self, now: Instant, cooldown: Duration, line: &str) -> Verdict {
        if let Some(p) = &mut self.pending {
            if p.lines.len() < MAX_EXCERPT_LINES {
                p.lines.push(trim(line));
            } else {
                self.suppressed += 1;
            }
            return Verdict::Held;
        }
        if let Some(at) = self.last_raised {
            if now.duration_since(at) < cooldown {
                self.suppressed += 1;
                return Verdict::Held;
            }
        }
        self.pending = Some(Pending {
            deadline: now + GATHER,
            lines: vec![trim(line)],
            earlier: std::mem::take(&mut self.suppressed),
        });
        Verdict::Started
    }

    fn flush(&mut self, now: Instant) -> Option<Pending> {
        let p = self.pending.take()?;
        self.last_raised = Some(now);
        Some(p)
    }
}

fn trim(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() <= EXCERPT_CHARS {
        return line.to_string();
    }
    let mut s: String = line.chars().take(EXCERPT_CHARS).collect();
    s.push('…');
    s
}

fn cut(line: &str) -> &str {
    if line.len() <= MAX_LINE {
        return line;
    }
    let mut end = MAX_LINE;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// At most `MAX_PER_MINUTE` passes per rolling minute.
#[derive(Default)]
struct Limiter {
    sent: VecDeque<Instant>,
}

impl Limiter {
    fn allow(&mut self, now: Instant) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            self.sent.pop_front();
        }
        if self.sent.len() >= MAX_PER_MINUTE {
            return false;
        }
        self.sent.push_back(now);
        true
    }
}

fn pattern_of(rule: &LogRule) -> String {
    rule.literal
        .clone()
        .or_else(|| rule.regex.clone())
        .unwrap_or_default()
}

fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{} h", s / 3600)
    } else if s >= 60 {
        format!("{} m", s / 60)
    } else {
        format!("{s} s")
    }
}

/// Everything a follower needs to build a signal.
#[derive(Clone)]
struct Target {
    stack: String,
    service: String,
    instance: String,
    container: String,
    node: String,
}

fn build_signal(t: &Target, idx: usize, rule: &LogRule, p: Pending) -> AlertSignal {
    let mut s = new_signal(
        "log.match",
        format!("log.match:{}:{idx}", t.container),
        SignalAction::Once,
    );
    s.severity = Some(rule.severity);
    s.title = rule
        .title
        .clone()
        .unwrap_or_else(|| format!("{} logged \"{}\"", t.service, pattern_of(rule)));
    s.detail = if p.earlier > 0 {
        format!(
            "{} printed a matching line. It matched {} more times in the last {}.",
            t.instance,
            p.earlier,
            ago(rule.cooldown)
        )
    } else {
        format!("{} printed a matching line.", t.instance)
    };
    s.subject = t.instance.clone();
    s.stack = Some(t.stack.clone());
    s.instance = Some(t.instance.clone());
    s.node = Some(t.node.clone());
    s.excerpt = p.lines;
    s.count = 1 + p.earlier;
    s
}

/// Follow one container until its stream ends or the task is aborted.
async fn follow(
    docker: Docker,
    t: Target,
    compiled: Compiled,
    tx: tokio::sync::mpsc::Sender<AlertSignal>,
) {
    let mut states: Vec<RuleState> = compiled
        .rules
        .iter()
        .map(|_| RuleState::default())
        .collect();
    let mut limiter = Limiter::default();
    let opts = LogOptions {
        follow: true,
        tail: Tail::Lines(0),
        since: None,
    };
    let mut stream = Box::pin(docker.logs(&t.container, opts));
    loop {
        let wake = states
            .iter()
            .filter_map(|s| s.pending.as_ref().map(|p| p.deadline))
            .min();
        let item = tokio::select! {
            item = stream.next() => Some(item),
            _ = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => None,
        };
        let now = Instant::now();
        match item {
            Some(Some(Ok(line))) => {
                for i in compiled.hits(line.stream, cut(&line.text)) {
                    states[i].on_match(now, compiled.rules[i].cooldown, &line.text);
                }
            }
            Some(Some(Err(e))) => {
                debug!(container = %t.container, error = %e, "log follow ended");
                break;
            }
            Some(None) => break,
            None => {}
        }
        for (i, st) in states.iter_mut().enumerate() {
            if st.pending.as_ref().is_some_and(|p| p.deadline <= now) {
                if let Some(p) = st.flush(now) {
                    if limiter.allow(now) {
                        // Never wait on the queue: drop the signal instead.
                        let _ = tx.try_send(build_signal(&t, i, &compiled.rules[i], p));
                    }
                }
            }
        }
    }
    // Flush what was gathered when the stream ended (the container died).
    let now = Instant::now();
    for (i, st) in states.iter_mut().enumerate() {
        if let Some(p) = st.flush(now) {
            if limiter.allow(now) {
                let _ = tx.try_send(build_signal(&t, i, &compiled.rules[i], p));
            }
        }
    }
}

struct Follower {
    fingerprint: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// The rules to watch on a container, or none.
pub fn watched_rules(stack: &StackFile, service: &str) -> Vec<LogRule> {
    let alerts: StackAlerts = stack.effective_alerts(service);
    if alerts.enabled {
        alerts.logs
    } else {
        Vec::new()
    }
}

fn fingerprint(rules: &[LogRule]) -> String {
    format!("{rules:?}")
}

/// Run until aborted. Call once per node.
pub async fn run(cluster: Arc<ClusterNode>, docker: Arc<RwLock<Option<Docker>>>, node: String) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AlertSignal>(QUEUE);
    let sender = cluster.clone();
    let sender_task = tokio::spawn(async move {
        while let Some(signal) = rx.recv().await {
            push_signal(&sender, signal).await;
        }
    });
    let mut followers: HashMap<String, Follower> = HashMap::new();
    loop {
        tokio::time::sleep(SCAN_EVERY).await;
        let Some(docker) = docker.read().await.clone() else {
            followers.clear();
            continue;
        };
        let stacks = cluster.state().await.stacks;
        // Nothing declares log rules: skip the docker call entirely.
        let mut wanted: HashMap<String, (Target, Vec<LogRule>)> = HashMap::new();
        let parsed: HashMap<&String, StackFile> = stacks
            .iter()
            .filter(|(_, s)| s.spec.contains("logs"))
            .filter_map(|(n, s)| StackFile::parse(&s.spec).ok().map(|f| (n, f)))
            .collect();
        if !parsed.is_empty() {
            let containers = match docker.list_managed().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "log watches could not list containers");
                    continue;
                }
            };
            for c in containers.into_iter().filter(|c| c.running) {
                let (Some(stack), Some(service), Some(inst)) = (
                    c.labels.get(cl_docker::LABEL_STACK),
                    c.labels.get(cl_docker::LABEL_SERVICE),
                    c.labels.get(cl_docker::LABEL_INSTANCE),
                ) else {
                    continue;
                };
                let Some(file) = stacks.get(stack).and_then(|_| parsed.get(stack)) else {
                    continue;
                };
                let rules = watched_rules(file, service);
                if rules.is_empty() {
                    continue;
                }
                let container = InstanceId::from_str(inst)
                    .map(|i| i.container_name())
                    .unwrap_or_else(|_| c.id.clone());
                wanted.insert(
                    c.id.clone(),
                    (
                        Target {
                            stack: stack.clone(),
                            service: service.clone(),
                            instance: inst.clone(),
                            container,
                            node: node.clone(),
                        },
                        rules,
                    ),
                );
            }
        }
        followers.retain(|id, f| {
            wanted
                .get(id)
                .is_some_and(|(_, r)| fingerprint(r) == f.fingerprint)
                && !f.handle.is_finished()
        });
        for (id, (target, rules)) in wanted {
            if followers.contains_key(&id) {
                continue;
            }
            let Some(compiled) = Compiled::new(&rules) else {
                continue;
            };
            let handle = tokio::spawn(follow(docker.clone(), target, compiled, tx.clone()));
            followers.insert(
                id,
                Follower {
                    fingerprint: fingerprint(&rules),
                    handle,
                },
            );
        }
        if sender_task.is_finished() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::AlertSeverity;

    fn rule(literal: Option<&str>, regex: Option<&str>) -> LogRule {
        LogRule {
            literal: literal.map(String::from),
            regex: regex.map(String::from),
            ignore_case: false,
            severity: AlertSeverity::Warning,
            title: None,
            cooldown: Duration::from_secs(300),
            stream: LogStream::Both,
            services: Vec::new(),
        }
    }

    #[test]
    fn literals_are_escaped_and_regexes_run() {
        let c = Compiled::new(&[rule(Some("a.b"), None), rule(None, Some(r"E\d{3}"))]).unwrap();
        let o = cl_docker::LogStream::Stdout;
        assert_eq!(c.hits(o, "x a.b y"), vec![0]);
        assert!(c.hits(o, "axb").is_empty());
        assert_eq!(c.hits(o, "failed E404"), vec![1]);
        assert!(c.hits(o, "quiet").is_empty());
    }

    #[test]
    fn ignore_case_and_stream_filter() {
        let mut r = rule(Some("panic"), None);
        r.ignore_case = true;
        r.stream = LogStream::Stderr;
        let c = Compiled::new(&[r]).unwrap();
        assert_eq!(c.hits(cl_docker::LogStream::Stderr, "PANIC now"), vec![0]);
        assert!(c.hits(cl_docker::LogStream::Stdout, "PANIC now").is_empty());
    }

    #[test]
    fn broken_or_empty_rules_compile_to_nothing() {
        assert!(Compiled::new(&[]).is_none());
        assert!(Compiled::new(&[rule(None, Some("("))]).is_none());
        assert!(Compiled::new(&[rule(None, Some("(")), rule(Some("ok"), None)]).is_some());
    }

    #[test]
    fn cooldown_counts_and_reports_the_misses() {
        let cd = Duration::from_secs(300);
        let mut st = RuleState::default();
        let t0 = Instant::now();
        assert_eq!(st.on_match(t0, cd, "first"), Verdict::Started);
        assert_eq!(st.on_match(t0, cd, "same second"), Verdict::Held);
        let p = st.flush(t0 + GATHER).unwrap();
        assert_eq!(p.lines, vec!["first", "same second"]);
        assert_eq!(p.earlier, 0);
        for _ in 0..3 {
            assert_eq!(
                st.on_match(t0 + Duration::from_secs(10), cd, "again"),
                Verdict::Held
            );
        }
        assert!(st.flush(t0).is_none());
        let later = t0 + cd + Duration::from_secs(1);
        assert_eq!(st.on_match(later, cd, "after"), Verdict::Started);
        assert_eq!(st.flush(later).unwrap().earlier, 3);
    }

    #[test]
    fn excerpt_is_capped() {
        let mut st = RuleState::default();
        let now = Instant::now();
        st.on_match(now, Duration::ZERO, "1");
        for l in ["2", "3", "4", "5"] {
            st.on_match(now, Duration::ZERO, l);
        }
        let p = st.flush(now).unwrap();
        assert_eq!(p.lines.len(), MAX_EXCERPT_LINES);
        assert_eq!(trim(&"é".repeat(600)).chars().count(), EXCERPT_CHARS + 1);
    }

    #[test]
    fn long_lines_are_cut_on_a_char_boundary() {
        let line = "é".repeat(MAX_LINE);
        assert!(cut(&line).len() <= MAX_LINE);
        assert!(cut("short") == "short");
    }

    #[test]
    fn limiter_caps_a_minute_then_recovers() {
        let mut l = Limiter::default();
        let t = Instant::now();
        for _ in 0..MAX_PER_MINUTE {
            assert!(l.allow(t));
        }
        assert!(!l.allow(t));
        assert!(l.allow(t + Duration::from_secs(61)));
    }

    #[test]
    fn signal_carries_the_rule_and_the_misses() {
        let t = Target {
            stack: "media".into(),
            service: "radarr".into(),
            instance: "media.radarr.1".into(),
            container: "media_radarr_1".into(),
            node: "atlas-01".into(),
        };
        let r = rule(Some("FATAL"), None);
        let s = build_signal(
            &t,
            0,
            &r,
            Pending {
                deadline: Instant::now(),
                lines: vec!["FATAL boom".into()],
                earlier: 37,
            },
        );
        assert_eq!(s.event, "log.match");
        assert_eq!(s.title, "radarr logged \"FATAL\"");
        assert!(s.detail.contains("37 more times in the last 5 m"));
        assert_eq!(s.count, 38);
        assert_eq!(s.excerpt, vec!["FATAL boom"]);
        assert_eq!(s.severity, Some(AlertSeverity::Warning));
    }

    #[test]
    fn stack_rules_respect_the_enabled_switch() {
        let spec = |alerts: &str| {
            format!("services:\n  web:\n    image: nginx\nx-clouduler:\n  alerts:\n{alerts}\n")
        };
        let on = StackFile::parse(&spec("    logs:\n      - match: ERROR")).unwrap();
        assert_eq!(watched_rules(&on, "web").len(), 1);
        let none = StackFile::parse("services:\n  web:\n    image: nginx\n").unwrap();
        assert!(watched_rules(&none, "web").is_empty());
    }
}
