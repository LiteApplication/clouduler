//! What is wrong with the cluster right now, as data.
//!
//! The same classification `clouduler stack unhealthy` and `clouduler status`
//! print (see `cl-cli/src/health.rs` and `status.rs`), re-expressed over the
//! replicated `ClusterState` plus the live instance summaries the api already
//! builds, so an AI client gets "what is broken" in one call instead of
//! reading the whole cluster. Printing stays with the callers: this returns
//! [`Problem`]s and lets the tool decide how much to show for a focused stack.

use std::time::{SystemTime, UNIX_EPOCH};

use cl_raft::state::{
    BackupRunOutcome, ClusterState, MigrationPhase, ReconcileStatus, RestoreStatus, UpdatePhase,
};
use cl_raft::DesiredState;

use crate::routes::instances::InstanceSummary;

/// Updates not finished after this long are reported as stuck.
const STUCK_UPDATE_SECS: i64 = 15 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Problem {
    /// The stack the problem belongs to, or `None` for a fault that belongs to
    /// the cluster as a whole (a node, a volume, a certificate, a backup).
    pub stack: Option<String>,
    /// Instance id, node name or object the problem is about.
    pub subject: String,
    /// Short machine-friendly category, e.g. `crash_loop`.
    pub kind: &'static str,
    pub detail: String,
}

impl Problem {
    pub fn line(&self) -> String {
        format!("[{}] {}: {}", self.kind, self.subject, self.detail)
    }
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Every problem the cluster currently has.
pub(crate) fn classify(
    state: &ClusterState,
    summaries: &[InstanceSummary],
    now: i64,
) -> Vec<Problem> {
    let mut out = Vec::new();
    for summary in summaries {
        out.extend(instance_problems(state, summary));
    }
    out.extend(update_problems(state, now));
    out.extend(cluster_problems(state, now));
    out
}

fn stack_of(id: &str) -> Option<String> {
    id.parse::<cl_raft::InstanceId>().ok().map(|i| i.stack)
}

/// Explains a state relative to the desired one, so a deliberate stop is told
/// apart from a crash (both read `exited`).
fn describe_state(desired: &str, state: &str) -> Option<(&'static str, String)> {
    let desired = match desired {
        "stopped" => DesiredState::Stopped,
        _ => DesiredState::Running,
    };
    match (desired, state) {
        (DesiredState::Stopped, "exited" | "stopped") => {
            Some(("stopped", "stopped on purpose (stack stop)".to_string()))
        }
        (DesiredState::Stopped, other) => Some((
            "stopped",
            format!("desired state is stopped, but the container is still {other}"),
        )),
        (DesiredState::Running, "exited") => Some((
            "exited",
            "exited but desired state is running (crashed?); check get_logs".to_string(),
        )),
        (DesiredState::Running, "stopped") => Some((
            "no_container",
            "no container exists on its node, but desired state is running".to_string(),
        )),
        (DesiredState::Running, "restarting") => Some((
            "crash_loop",
            "restarting - usually a container crashing and being restarted in a loop".to_string(),
        )),
        _ => None,
    }
}

/// A container not running the image the cluster pinned for it: a pin was
/// recorded and the container was never rebuilt onto it.
fn describe_pin_drift(summary: &InstanceSummary) -> Option<String> {
    let pinned = summary.image_digest.as_deref()?;
    let running = summary.running_digest.as_deref()?;
    if pinned == running {
        return None;
    }
    Some(format!(
        "running {running}, but the cluster pinned {pinned} - the container was never rebuilt onto the pin"
    ))
}

fn instance_problems(state: &ClusterState, s: &InstanceSummary) -> Vec<Problem> {
    let stack = stack_of(&s.id);
    let mk = |kind: &'static str, detail: String| Problem {
        stack: stack.clone(),
        subject: s.id.clone(),
        kind,
        detail,
    };
    let mut out = Vec::new();

    if s.state == "pending_placement" {
        out.push(mk(
            "pending_placement",
            s.placement_error
                .clone()
                .unwrap_or_else(|| "no node can host it".to_string()),
        ));
        return out;
    }

    let id = s.id.parse::<cl_raft::InstanceId>().ok();
    let assignment = id.as_ref().and_then(|i| state.assignments.get(i));

    if let Some(migration) = assignment.and_then(|a| a.migration.as_ref()) {
        match &migration.phase {
            MigrationPhase::Failed { reason } => {
                out.push(mk("migration_failed", reason.clone()));
                return out;
            }
            phase if !phase.is_terminal() => {
                out.push(mk(
                    "migrating",
                    format!(
                        "moving from {} to {} ({}) - if this phase does not change, the move is stuck",
                        migration.from,
                        migration.to,
                        format!("{phase:?}").to_lowercase()
                    ),
                ));
                return out;
            }
            _ => {}
        }
    }

    if let Some(a) = assignment {
        if state.unreachable.contains(&a.node) {
            out.push(mk(
                "node_unreachable",
                format!("node {} is unreachable", a.node),
            ));
            return out;
        }
    }

    if let Some((kind, detail)) = describe_state(&s.desired, &s.state) {
        out.push(mk(kind, detail));
    } else if let Some(detail) = describe_pin_drift(s) {
        out.push(mk("pin_drift", detail));
    }
    if s.state == "unknown" {
        out.push(mk(
            "unknown",
            s.reason
                .clone()
                .unwrap_or_else(|| "its node did not report a state".to_string()),
        ));
    }
    if s.health == "unhealthy" {
        out.push(mk("unhealthy", "failing its healthcheck".to_string()));
    }
    if let Some(error) = s.proxy.as_ref().and_then(|p| p.error.as_ref()) {
        out.push(mk(
            "proxy_check_failed",
            format!("the reverse proxy cannot reach it: {error}"),
        ));
    }
    if let Some(id) = &id {
        if let Some(moves) = state.crash_moves.get(id).filter(|m| !m.is_empty()) {
            let nodes: Vec<&str> = moves.keys().map(String::as_str).collect();
            out.push(mk(
                "crash_loop",
                format!(
                    "was moved off {} after crashing repeatedly; it is likely to keep crashing - read get_logs",
                    nodes.join(", ")
                ),
            ));
        }
    }
    out
}

/// Failed updates, and ones that have stopped making progress.
fn update_problems(state: &ClusterState, now: i64) -> Vec<Problem> {
    let mut out = Vec::new();
    for (instance, update) in &state.instance_updates {
        let (kind, detail) = match update.phase {
            UpdatePhase::Started => continue,
            UpdatePhase::Failed => (
                "update_failed",
                format!(
                    "update failed on {}: {}",
                    update.node,
                    update.error.as_deref().unwrap_or("no error recorded")
                ),
            ),
            phase if now - update.at > STUCK_UPDATE_SECS => (
                "update_stuck",
                format!(
                    "update is {phase:?} on {} and has made no progress for {}s",
                    update.node,
                    now - update.at
                ),
            ),
            _ => continue,
        };
        out.push(Problem {
            stack: Some(instance.stack.clone()),
            subject: instance.to_string(),
            kind,
            detail,
        });
    }
    out
}

/// Faults that belong to no single stack.
fn cluster_problems(state: &ClusterState, now: i64) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut add = |kind: &'static str, subject: String, detail: String| {
        out.push(Problem {
            stack: None,
            subject,
            kind,
            detail,
        });
    };

    if let Some(stack) = &state.emergency_stack {
        add(
            "emergency_mode",
            stack.clone(),
            format!("emergency mode: only stack {stack} is being scheduled"),
        );
    }
    for node in &state.unreachable {
        add(
            "node_unreachable",
            node.clone(),
            format!("node {node} is unreachable"),
        );
    }
    for (id, volume) in &state.volumes {
        if let Some(refusal) = &volume.sync_refused {
            add(
                "volume_sync_refused",
                id.to_string(),
                format!(
                    "replication from {} to {} is stopped to protect {}'s copy: {}",
                    refusal.from, refusal.to, refusal.to, refusal.reason
                ),
            );
        }
        if volume.looks_empty {
            add(
                "volume_empty",
                id.to_string(),
                "holds no files. Delete it if that is intended, or restore it from a backup"
                    .to_string(),
            );
        }
        if !state.nodes.contains_key(&volume.primary) {
            add(
                "volume",
                id.to_string(),
                format!(
                    "its primary {} is not a member of the cluster",
                    volume.primary
                ),
            );
        } else if state.unreachable.contains(&volume.primary) {
            add(
                "volume",
                id.to_string(),
                format!(
                    "its primary {} is unreachable, so nothing can write to it",
                    volume.primary
                ),
            );
        }
        for (node, generation) in &volume.replicas {
            if *generation < volume.generation {
                add(
                    "volume",
                    id.to_string(),
                    format!(
                        "the replica on {node} is at generation {generation}, {} behind the primary",
                        volume.generation - generation
                    ),
                );
            }
        }
    }

    let mut latest: std::collections::BTreeMap<&str, &cl_raft::state::BackupRun> =
        Default::default();
    for run in state.restic_runs.values() {
        let slot = latest.entry(run.target_id.as_str()).or_insert(run);
        if run.started_at > slot.started_at {
            *slot = run;
        }
    }
    for (target, run) in latest {
        match &run.outcome {
            BackupRunOutcome::Failed { error } => add(
                "backup",
                target.to_string(),
                format!(
                    "backup of {} to {} FAILED: {error}",
                    run.drive, run.destination_id
                ),
            ),
            BackupRunOutcome::Succeeded {
                warning: Some(w), ..
            } => add(
                "backup",
                target.to_string(),
                format!("backup succeeded with warning: {w}"),
            ),
            _ => {}
        }
    }
    for request in state.restore_requests.values() {
        match &request.status {
            RestoreStatus::Failed { error } => add(
                "restore",
                request.id.clone(),
                format!("restore ({:?}) FAILED: {error}", request.scope),
            ),
            RestoreStatus::Pending | RestoreStatus::Running => add(
                "restore",
                request.id.clone(),
                format!("restore ({:?}) is {:?}", request.scope, request.status),
            ),
            RestoreStatus::Succeeded => {}
        }
    }

    for (kind, statuses) in [("cert", &state.cert_status), ("dns", &state.dns_status)] {
        for (name, status) in statuses {
            match status {
                ReconcileStatus::Pending => {
                    add(kind, name.clone(), "pending".to_string());
                }
                ReconcileStatus::Error { message, at } => add(
                    kind,
                    name.clone(),
                    format!("ERROR {}s ago: {message}", (now - at).max(0)),
                ),
            }
        }
    }
    out
}

/// Splits problems into those for `focus` (or all, when nothing is focused),
/// the cluster-wide ones, and a count for everything else.
pub(crate) struct Scoped<'a> {
    pub shown: Vec<&'a Problem>,
    pub cluster: Vec<&'a Problem>,
    pub hidden_elsewhere: usize,
}

pub(crate) fn scope<'a>(problems: &'a [Problem], focus: Option<&str>) -> Scoped<'a> {
    let mut scoped = Scoped {
        shown: Vec::new(),
        cluster: Vec::new(),
        hidden_elsewhere: 0,
    };
    for p in problems {
        match (&p.stack, focus) {
            (None, _) => scoped.cluster.push(p),
            (Some(_), None) => scoped.shown.push(p),
            (Some(s), Some(f)) if s == f => scoped.shown.push(p),
            (Some(_), Some(_)) => scoped.hidden_elsewhere += 1,
        }
    }
    scoped
}

/// The text a tool shows for a scoped set of problems.
pub(crate) fn render(problems: &[Problem], focus: Option<&str>) -> String {
    let scoped = scope(problems, focus);
    let mut out = String::new();
    match focus {
        Some(f) if scoped.shown.is_empty() => {
            out.push_str(&format!("stack {f}: no problems\n"));
        }
        Some(f) => {
            out.push_str(&format!("stack {f}: {} problem(s)\n", scoped.shown.len()));
        }
        None if scoped.shown.is_empty() => out.push_str("no instance problems\n"),
        None => out.push_str(&format!("{} instance problem(s)\n", scoped.shown.len())),
    }
    for p in &scoped.shown {
        out.push_str(&format!("  {}\n", p.line()));
    }
    if !scoped.cluster.is_empty() {
        out.push_str(&format!("cluster: {} issue(s)\n", scoped.cluster.len()));
        for p in &scoped.cluster {
            out.push_str(&format!("  {}\n", p.line()));
        }
    }
    if scoped.hidden_elsewhere > 0 {
        out.push_str(&format!(
            "{} problem(s) in other stacks (hidden while a stack is focused; clear_focus to see them)\n",
            scoped.hidden_elsewhere
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str, desired: &str, state: &str, health: &str) -> InstanceSummary {
        serde_json::from_value(serde_json::json!({
            "id": id, "node": "n0", "desired": desired, "state": state,
            "running": state == "running", "health": health,
            "image": null, "image_digest": null, "started_at": null, "migration": null,
        }))
        .unwrap()
    }

    #[test]
    fn a_running_healthy_instance_has_no_problem() {
        let s = summary("web.app.0", "running", "running", "healthy");
        assert!(instance_problems(&ClusterState::default(), &s).is_empty());
    }

    #[test]
    fn a_crash_and_a_deliberate_stop_are_told_apart() {
        let crashed = summary("web.app.0", "running", "exited", "none");
        let p = instance_problems(&ClusterState::default(), &crashed);
        assert_eq!(p[0].kind, "exited");
        let stopped = summary("web.app.0", "stopped", "exited", "none");
        let p = instance_problems(&ClusterState::default(), &stopped);
        assert_eq!(p[0].kind, "stopped");
    }

    #[test]
    fn pending_placement_carries_its_reason() {
        let mut s = summary("web.app.0", "running", "pending_placement", "none");
        s.placement_error = Some("no node has drive fast".into());
        let p = instance_problems(&ClusterState::default(), &s);
        assert_eq!(p[0].kind, "pending_placement");
        assert!(p[0].detail.contains("drive fast"));
        assert_eq!(p[0].stack.as_deref(), Some("web"));
    }

    #[test]
    fn focus_hides_other_stacks_but_counts_them() {
        let p = |stack: &str| Problem {
            stack: Some(stack.into()),
            subject: format!("{stack}.s.0"),
            kind: "exited",
            detail: "x".into(),
        };
        let all = vec![p("a"), p("b"), p("b")];
        let text = render(&all, Some("a"));
        assert!(text.contains("a.s.0"));
        assert!(!text.contains("b.s.0"));
        assert!(text.contains("2 problem(s) in other stacks"));
        assert!(render(&all, None).contains("b.s.0"));
    }
}
