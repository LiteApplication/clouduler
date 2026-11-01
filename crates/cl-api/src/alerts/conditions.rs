//! The standing conditions of the cluster, as [`Observed`] problems.
//!
//! Instance and update conditions come from the same classifier the MCP
//! `list_problems` tool uses. Backups, restores, certificates and DNS are read
//! from the state here, so their text is stable between passes (a detail that
//! counted seconds would rewrite the alert on every pass).

use std::collections::{BTreeMap, HashMap, HashSet};

use cl_alert::catalog;
use cl_alert::AlertSettings;
use cl_config::AlertSeverity;
use cl_raft::alerts::AlertLink;
use cl_raft::state::{BackupRunOutcome, ReconcileStatus, RestoreScope, RestoreStatus};
use cl_raft::{ClusterState, InstanceId, Role};

use super::lifecycle::Observed;
use crate::mcp::problems::{self, Problem};
use crate::routes::instances::InstanceSummary;

/// Events the engine derives from state every pass and resolves by itself
/// when they stop holding. The rest are opened and closed by signals.
const DERIVED: &[&str] = &[
    "cluster.emergency_mode",
    "node.unreachable",
    "instance.crash_loop",
    "instance.exited",
    "instance.unhealthy",
    "instance.unplaceable",
    "instance.unreachable_by_proxy",
    "instance.pin_drift",
    "volume.sync_refused",
    "volume.migration_failed",
    "volume.empty",
    "volume.replica_error",
    "restore.failed",
    "backup.failed",
    "backup.overdue",
    "cert.expiring",
    "cert.issuance_failed",
    "dns.reconcile_failed",
    "update.failed",
    "update.stuck",
];

pub fn is_derived(event: &str) -> bool {
    DERIVED.contains(&event)
}

/// A replica must lag this long before it is worth an alert: replicas are
/// behind for a whole sync interval after every write, by design.
const REPLICA_LAG_SECS: i64 = 30 * 60;
const FAILED_RESTORE_KEEP_SECS: i64 = 3 * 24 * 3600;
const BACKUP_CRITICAL_SECS: i64 = 24 * 3600;

#[derive(Default)]
pub struct Derived {
    pub observed: Vec<Observed>,
    /// Instances whose live state could not be read this pass.
    pub held_instances: HashSet<String>,
    /// Dedupes that only count once they have held this many seconds.
    pub min_age: HashMap<String, i64>,
}

fn link(label: &str, path: String) -> AlertLink {
    AlertLink {
        label: label.to_string(),
        path,
    }
}

fn instance_links(id: &str) -> Vec<AlertLink> {
    vec![
        link("Open instance", format!("/instances/{id}")),
        link("Logs", format!("/logs?instance={id}")),
    ]
}

struct Builder<'a> {
    out: BTreeMap<String, Observed>,
    min_age: HashMap<String, i64>,
    _state: &'a ClusterState,
}

impl Builder<'_> {
    fn add(&mut self, mut o: Observed) {
        if catalog::find(&o.event).is_none() {
            return;
        }
        o.dedupe = format!("{}:{}", o.event, o.subject);
        match self.out.get_mut(&o.dedupe) {
            Some(existing) => {
                if !existing.detail.contains(&o.detail) {
                    existing.detail = format!("{}; {}", existing.detail, o.detail);
                }
                existing.severity = existing.severity.max(o.severity);
            }
            None => {
                self.out.insert(o.dedupe.clone(), o);
            }
        }
    }
}

fn base(event: &str, subject: &str, title: String, detail: String) -> Observed {
    let def = catalog::find(event);
    Observed {
        event: event.to_string(),
        dedupe: String::new(),
        severity: def
            .map(|d| super::sev_back(d.severity))
            .unwrap_or(AlertSeverity::Warning),
        title,
        detail,
        subject: subject.to_string(),
        stack: None,
        instance: None,
        node: None,
        links: Vec::new(),
        excerpt: Vec::new(),
    }
}

pub(crate) fn derive(
    state: &ClusterState,
    summaries: &[InstanceSummary],
    now: i64,
    settings: &AlertSettings,
) -> Derived {
    let mut b = Builder {
        out: BTreeMap::new(),
        min_age: HashMap::new(),
        _state: state,
    };
    let mut held = HashSet::new();
    for s in summaries {
        // Unreadable (its node did not answer) is not the same as fine.
        if s.state == "unknown" {
            held.insert(s.id.clone());
        }
    }

    for p in problems::classify(state, summaries, now) {
        from_problem(&mut b, state, p);
    }
    backups(&mut b, state, now);
    restores(&mut b, state, now);
    certs(&mut b, state, now, settings);
    for (fqdn, status) in &state.cert_status {
        if let ReconcileStatus::Error { message, .. } = status {
            let mut o = base(
                "cert.issuance_failed",
                fqdn,
                format!("Certificate for {fqdn} cannot be issued"),
                message.clone(),
            );
            o.links = vec![link("Open DNS and certificates", "/dns".into())];
            b.add(o);
        }
    }
    for (fqdn, status) in &state.dns_status {
        if let ReconcileStatus::Error { message, .. } = status {
            let mut o = base(
                "dns.reconcile_failed",
                fqdn,
                format!("DNS records for {fqdn} cannot be updated"),
                message.clone(),
            );
            o.links = vec![link("Open DNS and certificates", "/dns".into())];
            b.add(o);
        }
    }

    Derived {
        observed: b.out.into_values().collect(),
        held_instances: held,
        min_age: b.min_age,
    }
}

fn from_problem(b: &mut Builder, state: &ClusterState, p: Problem) {
    let stack = p.stack.clone();
    let instance_id = p.subject.parse::<InstanceId>().ok().map(|i| i.to_string());
    let mut o = match p.kind {
        "crash_loop" => base(
            "instance.crash_loop",
            &p.subject,
            format!("{} is crash looping", p.subject),
            p.detail.clone(),
        ),
        "exited" | "no_container" => base(
            "instance.exited",
            &p.subject,
            format!("{} is not running", p.subject),
            p.detail.clone(),
        ),
        "unhealthy" => base(
            "instance.unhealthy",
            &p.subject,
            format!("{} is failing its healthcheck", p.subject),
            p.detail.clone(),
        ),
        "pending_placement" => base(
            "instance.unplaceable",
            &p.subject,
            format!("No node can run {}", p.subject),
            p.detail.clone(),
        ),
        "proxy_check_failed" => base(
            "instance.unreachable_by_proxy",
            &p.subject,
            format!("The proxy cannot reach {}", p.subject),
            p.detail.clone(),
        ),
        "pin_drift" => base(
            "instance.pin_drift",
            &p.subject,
            format!("{} runs another image than its pin", p.subject),
            p.detail.clone(),
        ),
        "migration_failed" => base(
            "volume.migration_failed",
            &p.subject,
            format!("Moving {} failed", p.subject),
            p.detail.clone(),
        ),
        "update_failed" => base(
            "update.failed",
            &p.subject,
            format!("Update of {} failed", p.subject),
            p.detail.clone(),
        ),
        "update_stuck" => base(
            "update.stuck",
            &p.subject,
            format!("Update of {} is stuck", p.subject),
            "The update has made no progress for over 15 minutes".to_string(),
        ),
        // The instance-level duplicate of a node outage is covered by the node.
        "node_unreachable" if p.stack.is_none() => {
            let critical = state
                .nodes
                .get(&p.subject)
                .is_none_or(|n| n.role == Role::Manager);
            let mut o = base(
                "node.unreachable",
                &p.subject,
                format!("Node {} is unreachable", p.subject),
                "The leader has stopped hearing from it. Containers on it are rescheduled once the fence margin passes."
                    .to_string(),
            );
            o.severity = if critical {
                AlertSeverity::Critical
            } else {
                AlertSeverity::Warning
            };
            o.node = Some(p.subject.clone());
            o.links = vec![link("Open node", format!("/nodes/{}", p.subject))];
            o
        }
        "emergency_mode" => {
            let mut o = base(
                "cluster.emergency_mode",
                &p.subject,
                "Emergency mode is on".to_string(),
                p.detail.clone(),
            );
            o.links = vec![link("Open nodes", "/nodes".into())];
            o
        }
        "volume_sync_refused" => {
            let mut o = base(
                "volume.sync_refused",
                &p.subject,
                format!("Sync of {} refused", p.subject),
                p.detail.clone(),
            );
            o.links = vec![link("Open storage", "/storage".into())];
            o
        }
        "volume_empty" => {
            let mut o = base(
                "volume.empty",
                &p.subject,
                format!("Volume {} is empty", p.subject),
                p.detail.clone(),
            );
            o.links = vec![link("Open storage", "/storage".into())];
            o
        }
        "volume" => {
            let mut o = base(
                "volume.replica_error",
                &p.subject,
                format!("Volume {} has a replica problem", p.subject),
                p.detail.clone(),
            );
            o.links = vec![link("Open storage", "/storage".into())];
            if p.detail.contains("behind the primary") {
                b.min_age.insert(
                    format!("volume.replica_error:{}", p.subject),
                    REPLICA_LAG_SECS,
                );
            }
            o
        }
        _ => return,
    };
    if o.stack.is_none() {
        o.stack = stack;
    }
    if o.event.starts_with("instance.")
        || o.event.starts_with("update.")
        || o.event == "volume.migration_failed"
    {
        if let Some(id) = instance_id {
            o.instance = Some(id.clone());
            o.links = instance_links(&id);
            o.node = state
                .assignments
                .get(&id.parse::<InstanceId>().expect("parsed above"))
                .map(|a| a.node.clone());
        }
    }
    b.add(o);
}

fn backups(b: &mut Builder, state: &ClusterState, now: i64) {
    let mut by_target: BTreeMap<&str, Vec<&cl_raft::state::BackupRun>> = BTreeMap::new();
    for run in state.restic_runs.values() {
        by_target
            .entry(run.target_id.as_str())
            .or_default()
            .push(run);
    }
    for (target_id, mut runs) in by_target {
        runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
        let finished: Vec<_> = runs
            .iter()
            .filter(|r| !matches!(r.outcome, BackupRunOutcome::Running { .. }))
            .collect();
        let Some(latest) = finished.first() else {
            continue;
        };
        let links = vec![link("Open backups", "/storage/backups".into())];
        if let BackupRunOutcome::Failed { error } = &latest.outcome {
            // Failing since the oldest failure of the unbroken streak.
            let since = finished
                .iter()
                .take_while(|r| matches!(r.outcome, BackupRunOutcome::Failed { .. }))
                .last()
                .map(|r| r.started_at)
                .unwrap_or(latest.started_at);
            let mut o = base(
                "backup.failed",
                target_id,
                format!("Backup of {} failed", latest.drive),
                format!(
                    "Backup of {} to {}: {error}",
                    latest.drive, latest.destination_id
                ),
            );
            if now - since > BACKUP_CRITICAL_SECS {
                o.severity = AlertSeverity::Critical;
            }
            o.links = links.clone();
            b.add(o);
        }
        if let Some(target) = state.restic_targets.get(target_id) {
            let last_ok = finished
                .iter()
                .find(|r| matches!(r.outcome, BackupRunOutcome::Succeeded { .. }))
                .map(|r| r.started_at);
            let failing = matches!(latest.outcome, BackupRunOutcome::Failed { .. });
            let window = (target.frequency_secs as i64)
                .saturating_mul(2)
                .max(2 * 3600);
            if target.enabled && !failing {
                if let Some(last_ok) = last_ok {
                    if now - last_ok > window {
                        let mut o = base(
                            "backup.overdue",
                            target_id,
                            format!("Backup of {} is overdue", target.drive),
                            format!(
                                "No backup of {} to {} succeeded within twice its interval",
                                target.drive, target.destination_id
                            ),
                        );
                        o.links = links;
                        b.add(o);
                    }
                }
            }
        }
    }
}

fn restores(b: &mut Builder, state: &ClusterState, now: i64) {
    for r in state.restore_requests.values() {
        if let RestoreStatus::Failed { error } = &r.status {
            let at = r.finished_at.unwrap_or(r.requested_at);
            if now - at > FAILED_RESTORE_KEEP_SECS {
                continue;
            }
            let what = match &r.scope {
                RestoreScope::Drive { drive } => format!("drive {drive}"),
                RestoreScope::Volume { volume } => format!("volume {volume}"),
                RestoreScope::Service { stack, service } => format!("service {stack}.{service}"),
            };
            let mut o = base(
                "restore.failed",
                &r.id,
                format!("Restore of {what} failed"),
                error.clone(),
            );
            o.links = vec![link("Open backups", "/storage/backups".into())];
            b.add(o);
        }
    }
}

fn certs(b: &mut Builder, state: &ClusterState, now: i64, settings: &AlertSettings) {
    let t = &settings.thresholds;
    for (fqdn, cert) in &state.certificates {
        let secs_left = cert.not_after - now;
        let days = secs_left.div_euclid(86_400);
        let severity = if days <= t.cert_critical_days as i64 {
            AlertSeverity::Critical
        } else if days <= t.cert_warning_days as i64 {
            AlertSeverity::Warning
        } else {
            continue;
        };
        let detail = if secs_left <= 0 {
            "The certificate has expired".to_string()
        } else {
            format!("The certificate expires in {days} days")
        };
        let mut o = base(
            "cert.expiring",
            fqdn,
            format!("Certificate for {fqdn} expires soon"),
            detail,
        );
        o.severity = severity;
        o.links = vec![link("Open DNS and certificates", "/dns".into())];
        b.add(o);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{BackupRun, BackupTrigger, CertificateState};
    use cl_raft::Request;

    #[test]
    fn expiring_certificates_escalate_with_the_thresholds() {
        let mut st = ClusterState::default();
        let mut put = |fqdn: &str, not_after: i64| {
            st.apply(&Request::StoreCertificate(CertificateState {
                fqdn: fqdn.into(),
                chain_pem: String::new(),
                key_pem: String::new(),
                not_after,
                issued_at: 0,
            }));
        };
        let day = 86_400;
        put("far.example.com", 60 * day);
        put("soon.example.com", 10 * day + 5);
        put("critical.example.com", 3 * day);
        let d = derive(&st, &[], 0, &AlertSettings::default());
        let get = |s: &str| d.observed.iter().find(|o| o.subject == s);
        assert!(get("far.example.com").is_none());
        assert_eq!(
            get("soon.example.com").unwrap().severity,
            AlertSeverity::Warning
        );
        assert_eq!(
            get("critical.example.com").unwrap().severity,
            AlertSeverity::Critical
        );
    }

    #[test]
    fn a_long_failing_backup_turns_critical_and_recovery_clears_it() {
        let mut st = ClusterState::default();
        let run = |id: &str, at: i64, ok: bool| BackupRun {
            id: id.into(),
            target_id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            started_at: at,
            finished_at: Some(at + 1),
            outcome: if ok {
                BackupRunOutcome::Succeeded {
                    snapshot_id: "s".into(),
                    bytes: 1,
                    warning: None,
                }
            } else {
                BackupRunOutcome::Failed {
                    error: "boom".into(),
                }
            },
            triggered_by: BackupTrigger::Schedule,
        };
        let day = 86_400;
        for r in [
            run("a", 0, true),
            run("b", day, false),
            run("c", 3 * day / 2, false),
        ] {
            st.apply(&Request::RecordBackupRun(r));
        }
        let s = AlertSettings::default();
        let d = derive(&st, &[], day + 100, &s);
        assert_eq!(d.observed.len(), 1);
        assert_eq!(d.observed[0].severity, AlertSeverity::Warning);
        let d = derive(&st, &[], 2 * day + 100, &s);
        assert_eq!(d.observed[0].severity, AlertSeverity::Critical);
        st.apply(&Request::RecordBackupRun(run("d", 2 * day, true)));
        assert!(derive(&st, &[], 2 * day + 100, &s).observed.is_empty());
    }
}
