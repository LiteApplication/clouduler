//! `clouduler clear-errors`: drops the failures the cluster keeps as history
//! (see `Request::ClearErrors`) so they stop being reported as problems.

use anyhow::{anyhow, bail, Result};
use cl_config::ClusterConfig;
use cl_raft::client::client_for;
use cl_raft::state::{
    BackupRunOutcome, MigrationPhase, ReconcileStatus, RestoreStatus, UpdatePhase,
};
use cl_raft::Request;

pub async fn run(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    let counts = [
        (
            "backup runs",
            state
                .restic_runs
                .values()
                .filter(|r| matches!(r.outcome, BackupRunOutcome::Failed { .. }))
                .count(),
        ),
        (
            "restores",
            state
                .restore_requests
                .values()
                .filter(|r| matches!(r.status, RestoreStatus::Failed { .. }))
                .count(),
        ),
        (
            "image updates",
            state
                .instance_updates
                .values()
                .filter(|u| matches!(u.phase, UpdatePhase::Failed))
                .count(),
        ),
        (
            "migrations",
            state
                .assignments
                .values()
                .filter(|a| {
                    a.migration
                        .as_ref()
                        .is_some_and(|m| matches!(m.phase, MigrationPhase::Failed { .. }))
                })
                .count(),
        ),
        (
            "certificate/DNS statuses",
            state
                .cert_status
                .values()
                .chain(state.dns_status.values())
                .filter(|s| matches!(s, ReconcileStatus::Error { .. }))
                .count(),
        ),
        (
            "alert destinations",
            state.alert_destinations.values().filter(|h| !h.ok).count(),
        ),
    ];

    let response = client
        .submit(Request::ClearErrors)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("could not clear errors: {error}");
    }

    let cleared: Vec<String> = counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(what, n)| format!("{n} failed {what}"))
        .collect();
    if cleared.is_empty() {
        println!("no recorded errors to clear");
    } else {
        println!("cleared {}", cleared.join(", "));
    }
    Ok(())
}
