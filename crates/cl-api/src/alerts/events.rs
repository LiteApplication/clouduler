//! One-shot events, found by comparing the replicated state with the
//! previous pass. The first snapshot after becoming leader is taken silently:
//! whatever happened before is not news.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use cl_config::AlertSeverity;
use cl_raft::alerts::AlertLink;
use cl_raft::state::{AutoUpdateOutcome, BackupRunOutcome, MigrationPhase, UpdatePhase};
use cl_raft::{ClusterState, InstanceId};

use super::lifecycle::Observed;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    nodes: BTreeSet<String>,
    stacks: BTreeMap<String, u64>,
    /// instance -> (started_at, finished successfully)
    updates: BTreeMap<InstanceId, (i64, bool)>,
    /// instance -> migration target, while a move is in flight
    migrations: BTreeMap<InstanceId, String>,
    /// instance -> digest an `auto_update: notify` alert was raised for
    notified: BTreeMap<InstanceId, String>,
    secrets: BTreeMap<String, u64>,
    api_keys: BTreeMap<String, Option<String>>,
    warned_runs: BTreeSet<String>,
}

impl Snapshot {
    pub fn take(state: &ClusterState) -> Self {
        Snapshot {
            nodes: state.nodes.keys().cloned().collect(),
            stacks: state
                .stacks
                .iter()
                .map(|(k, s)| (k.clone(), s.revision))
                .collect(),
            updates: state
                .instance_updates
                .iter()
                .map(|(i, u)| (i.clone(), (u.started_at, u.phase == UpdatePhase::Started)))
                .collect(),
            migrations: state
                .assignments
                .values()
                .filter_map(|a| {
                    let m = a.migration.as_ref()?;
                    (!matches!(m.phase, MigrationPhase::Failed { .. })
                        && m.phase != MigrationPhase::Done)
                        .then(|| (a.instance.clone(), m.to.clone()))
                })
                .collect(),
            notified: state
                .auto_updates
                .iter()
                .filter(|(_, r)| r.outcome == AutoUpdateOutcome::Notified)
                .map(|(i, r)| (i.clone(), r.digest.clone()))
                .collect(),
            secrets: state
                .secrets
                .iter()
                .filter(|(n, s)| !n.starts_with("_clouduler_") && s.owner.is_none())
                .map(|(n, s)| (n.clone(), s.revision))
                .collect(),
            api_keys: crate::auth::keys::ApiKeyStore::load(state)
                .keys
                .into_iter()
                .map(|k| (k.id, k.name))
                .collect(),
            warned_runs: state
                .restic_runs
                .values()
                .filter(|r| {
                    matches!(
                        r.outcome,
                        BackupRunOutcome::Succeeded {
                            warning: Some(_),
                            ..
                        }
                    )
                })
                .map(|r| r.id.clone())
                .collect(),
        }
    }
}

fn event(kind: &str, subject: &str, title: String, detail: String) -> Observed {
    let def = cl_alert::catalog::find(kind);
    Observed {
        event: kind.to_string(),
        dedupe: format!("{kind}:{subject}"),
        severity: def
            .map(|d| super::sev_back(d.severity))
            .unwrap_or(AlertSeverity::Info),
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

fn link(label: &str, path: String) -> Vec<AlertLink> {
    vec![AlertLink {
        label: label.into(),
        path,
    }]
}

/// Events between `prev` and now.
pub fn diff(prev: &Snapshot, state: &ClusterState) -> Vec<Observed> {
    let now = Snapshot::take(state);
    let mut out = Vec::new();

    for n in now.nodes.difference(&prev.nodes) {
        let mut e = event(
            "cluster.node_joined",
            n,
            format!("Node {n} joined"),
            "It is now part of the cluster.".into(),
        );
        e.node = Some(n.clone());
        e.links = link("Open node", format!("/nodes/{n}"));
        out.push(e);
    }
    for n in prev.nodes.difference(&now.nodes) {
        out.push(event(
            "cluster.node_removed",
            n,
            format!("Node {n} was removed"),
            "It is no longer part of the cluster.".into(),
        ));
    }
    for (name, rev) in &now.stacks {
        let detail = match prev.stacks.get(name) {
            None => "First deploy.".to_string(),
            Some(old) if old != rev => format!("Redeployed, revision {rev}."),
            _ => continue,
        };
        let mut e = event(
            "stack.deployed",
            &format!("{name}:{rev}"),
            format!("Stack {name} deployed"),
            detail,
        );
        e.stack = Some(name.clone());
        e.links = link("Open stack", format!("/stacks/{name}"));
        out.push(e);
    }
    for name in prev.stacks.keys().filter(|n| !now.stacks.contains_key(*n)) {
        let mut e = event(
            "stack.removed",
            name,
            format!("Stack {name} removed"),
            "Its containers are being stopped.".into(),
        );
        e.stack = Some(name.clone());
        out.push(e);
    }
    for (inst, (started, ok)) in &now.updates {
        if *ok && prev.updates.get(inst) != Some(&(*started, true)) {
            let mut e = event(
                "update.completed",
                &format!("{inst}:{started}"),
                format!("{inst} updated"),
                "It restarted on its new image.".into(),
            );
            e.stack = Some(inst.stack.clone());
            e.instance = Some(inst.to_string());
            e.links = link("Open instance", format!("/instances/{inst}"));
            out.push(e);
        }
    }
    for (inst, digest) in &now.notified {
        if prev.notified.get(inst) != Some(digest) {
            let mut e = event(
                "update.available",
                &format!("{inst}:{digest}"),
                format!("A newer image is available for {inst}"),
                "Its stack sets auto_update to notify, so nothing was changed. Update it from the Updates page."
                    .into(),
            );
            e.stack = Some(inst.stack.clone());
            e.instance = Some(inst.to_string());
            e.links = link("Open updates", "/updates".into());
            out.push(e);
        }
    }
    for (inst, to) in &prev.migrations {
        if now.migrations.contains_key(inst) {
            continue;
        }
        let landed = state.assignments.get(inst).is_some_and(|a| {
            &a.node == to
                && a.migration
                    .as_ref()
                    .is_none_or(|m| m.phase == MigrationPhase::Done)
        });
        if landed {
            let mut e = event(
                "volume.migration_completed",
                &format!("{inst}:{to}"),
                format!("{inst} moved to {to}"),
                "Its volumes were copied and it is running on the new node.".into(),
            );
            e.stack = Some(inst.stack.clone());
            e.instance = Some(inst.to_string());
            e.node = Some(to.clone());
            e.links = link("Open instance", format!("/instances/{inst}"));
            out.push(e);
        }
    }
    for (name, rev) in &now.secrets {
        if prev.secrets.get(name).is_some_and(|old| old < rev) {
            out.push(event(
                "secret.rotated",
                &format!("{name}:{rev}"),
                format!("Secret {name} was overwritten"),
                "Containers using it keep the old value until they restart.".into(),
            ));
        }
    }
    for (id, name) in &now.api_keys {
        if !prev.api_keys.contains_key(id) {
            out.push(event(
                "auth.api_key_created",
                id,
                "An API key was created".into(),
                format!("Key {}", name.as_deref().unwrap_or(id)),
            ));
        }
    }
    for (id, name) in &prev.api_keys {
        if !now.api_keys.contains_key(id) {
            out.push(event(
                "auth.api_key_revoked",
                id,
                "An API key was revoked".into(),
                format!("Key {}", name.as_deref().unwrap_or(id)),
            ));
        }
    }
    for run_id in now.warned_runs.difference(&prev.warned_runs) {
        if let Some(run) = state.restic_runs.get(run_id) {
            if let BackupRunOutcome::Succeeded {
                warning: Some(w), ..
            } = &run.outcome
            {
                let mut e = event(
                    "backup.succeeded_with_warning",
                    run_id,
                    format!("Backup of {} succeeded with a warning", run.drive),
                    w.clone(),
                );
                e.links = link("Open backups", "/storage/backups".into());
                out.push(e);
            }
        }
    }
    out
}

/// Leader changes seen by this node, to tell a flapping cluster from a
/// single failover.
#[derive(Default)]
pub struct LeaderChanges {
    seen: VecDeque<i64>,
}

impl LeaderChanges {
    /// Changes seen in the last 10 minutes.
    pub fn recent(&self, now: i64) -> usize {
        self.seen.iter().filter(|t| now - **t <= 600).count()
    }

    /// Record a change at `now`; returns the changes in the last 10 minutes.
    pub fn record(&mut self, now: i64) -> usize {
        self.seen.push_back(now);
        while self.seen.front().is_some_and(|t| now - t > 600) {
            self.seen.pop_front();
        }
        self.seen.len()
    }
}

pub fn leader_changed(node: &str, recent: usize) -> Observed {
    let mut e = event(
        "cluster.leader_changed",
        &format!("{node}:{recent}"),
        format!("{node} is the new raft leader"),
        if recent > 3 {
            format!("The leader changed {recent} times in 10 minutes. Check the network between the managers.")
        } else {
            "A new leader was elected.".to_string()
        },
    );
    if recent > 3 {
        e.severity = AlertSeverity::Warning;
    }
    e.node = Some(node.to_string());
    e.links = link("Open nodes", "/nodes".into());
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::Request;

    #[test]
    fn stack_deploys_and_removals_are_events_but_the_first_snapshot_is_silent() {
        let mut st = ClusterState::default();
        st.apply(&Request::DeployStack {
            name: "web".into(),
            spec: "services: {}".into(),
            at: 1,
        });
        let first = Snapshot::take(&st);
        assert!(diff(&first, &st).is_empty());
        st.apply(&Request::DeployStack {
            name: "web".into(),
            spec: "services: {a: {image: x}}".into(),
            at: 2,
        });
        let events = diff(&first, &st);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "stack.deployed");
        let second = Snapshot::take(&st);
        st.apply(&Request::RemoveStack { name: "web".into() });
        assert_eq!(diff(&second, &st)[0].event, "stack.removed");
    }

    #[test]
    fn notify_mode_raises_update_available_once_per_digest() {
        use cl_raft::state::AutoUpdateRecord;
        let inst = InstanceId::new("web", "app", 0);
        let mut st = ClusterState::default();
        let rec = |digest: &str| AutoUpdateRecord {
            digest: digest.into(),
            detected_at: 1,
            outcome: AutoUpdateOutcome::Notified,
            at: 1,
            error: None,
        };
        let before = Snapshot::take(&st);
        st.auto_updates.insert(inst.clone(), rec("sha256:a"));
        let ev = diff(&before, &st);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event, "update.available");
        let same = Snapshot::take(&st);
        assert!(diff(&same, &st).is_empty());
        st.auto_updates.insert(inst, rec("sha256:b"));
        assert_eq!(diff(&same, &st).len(), 1);
    }

    #[test]
    fn user_secret_overwrites_are_events_internal_ones_are_not() {
        let mut st = ClusterState::default();
        let mut n = 0;
        let mut put = |st: &mut ClusterState, name: &str| {
            n += 1;
            st.apply(&Request::PutSecret {
                name: name.into(),
                sealed: if n % 2 == 0 {
                    "eA==".into()
                } else {
                    "eQ==".into()
                },
                key_fingerprint: "f".into(),
                owner: None,
            });
        };
        put(&mut st, "db_password");
        put(&mut st, "_clouduler_alert_x_url");
        let before = Snapshot::take(&st);
        put(&mut st, "db_password");
        put(&mut st, "_clouduler_alert_x_url");
        let ev = diff(&before, &st);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event, "secret.rotated");
    }

    #[test]
    fn leader_flapping_is_a_warning() {
        let mut c = LeaderChanges::default();
        for t in 0..3 {
            c.record(t);
        }
        assert_eq!(
            leader_changed("n1", c.record(3)).severity,
            AlertSeverity::Warning
        );
        assert_eq!(c.record(1000), 1);
        assert_eq!(leader_changed("n1", 1).severity, AlertSeverity::Info);
    }
}
