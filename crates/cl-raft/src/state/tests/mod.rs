//! State machine tests, split per domain. Shared fixtures live here.

use super::*;
use crate::alerts::{AlertState, SignalAction};
use cl_config::AlertSeverity;

mod alerts;
mod backup;
mod migration;
mod node;
mod reconcile;
mod request;
mod secret;
mod session;
mod stack;
mod update;
mod volume;

pub(super) fn sample_signal(id: &str) -> AlertSignal {
    AlertSignal {
        id: id.into(),
        event: "node.disk_low".into(),
        dedupe: "node.disk_low:n1:/data".into(),
        action: SignalAction::Open,
        severity: Some(AlertSeverity::Warning),
        title: "t".into(),
        detail: String::new(),
        subject: "n1".into(),
        stack: None,
        instance: None,
        node: Some("n1".into()),
        at: 1,
        excerpt: Vec::new(),
        count: 1,
    }
}

pub(super) fn sample_alert(id: &str, state: AlertState, last_at: i64) -> AlertRecord {
    AlertRecord {
        id: id.into(),
        event: "node.disk_low".into(),
        dedupe: format!("d-{id}"),
        category: "nodes".into(),
        severity: AlertSeverity::Warning,
        state,
        title: "t".into(),
        detail: String::new(),
        subject: "n1".into(),
        stack: None,
        instance: None,
        node: None,
        opened_at: last_at,
        last_at,
        resolved_at: None,
        count: 1,
        escalated_from: None,
        sent_severity: None,
        resolved_sent: false,
        digested: false,
        links: Vec::new(),
        excerpt: Vec::new(),
        deliveries: Vec::new(),
    }
}

pub(super) fn node(name: &str, drives: &[&str]) -> NodeState {
    NodeState {
        name: name.to_string(),
        id: uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes()),
        role: Role::Manager,
        rpc_addr: "10.10.0.1:7373".parse().unwrap(),
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
        open_all_ports: Default::default(),
        arch: Default::default(),
        binary_hash: Default::default(),
        ui_hash: Default::default(),
        started_at: Default::default(),
        node_pubkey: Default::default(),
        memory_total_bytes: Default::default(),
        gpus: Default::default(),
        images: Default::default(),
        version: Default::default(),
    }
}

pub(super) fn cluster() -> ClusterState {
    let mut state = ClusterState::default();
    state.apply(&Request::UpsertNode(node("n1", &["fast", "bulk"])));
    state.apply(&Request::UpsertNode(node("n2", &["fast"])));
    state.apply(&Request::UpsertNode(node("n3", &[])));
    state
}

pub(super) fn drives(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

pub(super) fn assign(state: &mut ClusterState, node: &str, needs: &[&str]) -> Response {
    state.apply(&Request::Assign {
        instance: InstanceId::new("gitlab", "web", 0),
        node: node.to_string(),
        drives: drives(needs),
    })
}

pub(super) fn wrapped_key_entry(target_pubkey: &str) -> WrappedKeyEntry {
    WrappedKeyEntry {
        ephemeral_public: "eph".into(),
        sealed: "blob".into(),
        key_fingerprint: "fp".into(),
        target_pubkey: target_pubkey.into(),
    }
}

/// A cluster with "fast" declared in the replicated config so `drive_known` sees it.
pub(super) fn cluster_with_known_drive() -> ClusterState {
    let mut state = cluster();
    state.apply(&Request::PutClusterConfig {
        yaml: "drives:\n  fast:\n    mode: replicate\n".into(),
        expect_generation: 0,
    });
    state
}

pub(super) fn node_local_destination(id: &str, node: &str) -> BackupDestination {
    BackupDestination {
        id: id.to_string(),
        name: id.to_string(),
        kind: BackupDestinationKind::NodeLocal {
            node: node.to_string(),
            path: "/srv/backups".into(),
        },
        password_secret: format!("_clouduler_restic_password_{id}"),
        credentials_secret: None,
    }
}

pub(super) fn backup_target(
    id: &str,
    drive: &str,
    destination_id: &str,
    enabled: bool,
) -> DriveBackupTarget {
    DriveBackupTarget {
        id: id.to_string(),
        drive: drive.to_string(),
        destination_id: destination_id.to_string(),
        enabled,
        frequency_secs: 86_400,
        time_of_day: None,
        keep_last: Some(7),
        keep_within_secs: None,
        keep_daily: None,
        keep_weekly: None,
        keep_monthly: None,
        keep_yearly: None,
        stop_containers: None,
        last_run: None,
    }
}

pub(super) fn restore_request(id: &str, drive: &str) -> RestoreRequest {
    RestoreRequest {
        id: id.to_string(),
        scope: RestoreScope::Drive {
            drive: drive.to_string(),
        },
        destination_id: None,
        snapshot: None,
        requested_at: 0,
        finished_at: None,
        status: RestoreStatus::Pending,
        requested_by: "tester".into(),
    }
}

pub(super) fn set_update(state: &mut ClusterState, phase: UpdatePhase, at: i64) -> Response {
    state.apply(&Request::SetInstanceUpdate {
        instance: InstanceId::new("gitlab", "web", 0),
        node: "n1".into(),
        phase,
        error: None,
        at,
    })
}

pub(super) fn running_run(id: &str, started_at: i64) -> BackupRun {
    BackupRun {
        id: id.into(),
        target_id: format!("t-{id}"),
        drive: "fast".into(),
        destination_id: "d1".into(),
        started_at,
        finished_at: None,
        outcome: BackupRunOutcome::Running { percent: None },
        triggered_by: BackupTrigger::Schedule,
    }
}
