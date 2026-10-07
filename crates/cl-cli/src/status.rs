//! `clouduler status` — one command that answers "what is wrong with the
//! cluster right now?": raft health, every node, then everything that is off
//! (unhealthy instances with their node, uptime, reason and log tail, stuck or
//! failed updates, failed backups and restores, certificate and DNS errors,
//! lagging volumes), ending in a count of what is running fine.
//!
//! Built so an operator - or an LLM - needs no second command to find out
//! what went wrong; `stack unhealthy` is the instance part of this on its own.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use cl_config::ClusterConfig;
use cl_raft::client::client_for;
use cl_raft::ids::node_id;
use cl_raft::state::{
    Availability, BackupRunOutcome, ClusterState, DesiredState, ReconcileStatus, RestoreStatus,
    UpdatePhase,
};

use crate::health::{self, ShowFlags};

/// Log lines shown under each unhealthy container.
const LOG_TAIL: u32 = 10;
/// Updates not finished after this long are reported as stuck.
const STUCK_UPDATE_SECS: i64 = 15 * 60;

pub async fn run(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let status = client.status().await.map_err(|e| anyhow!("{e}"))?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let now = unix_now();

    println!("cluster {}", cfg.cluster.name);
    let leader = status
        .leader
        .and_then(|id| state.nodes.values().find(|n| node_id(n.id) == id))
        .map(|n| n.name.as_str())
        .unwrap_or("none");
    println!(
        "  raft: leader={leader} quorum={} voters={}/{} state_version={} (answered by {})",
        if status.has_quorum { "yes" } else { "LOST" },
        status.reachable,
        status.voters,
        status.version,
        status.node,
    );
    if let Some(stack) = &state.emergency_stack {
        println!("  EMERGENCY MODE: only stack {stack} is being scheduled");
    }

    println!("nodes");
    for (name, node) in &state.nodes {
        let reach = if state.unreachable.contains(name) {
            "UNREACHABLE"
        } else {
            "up"
        };
        let avail = match node.availability {
            Availability::Active => "active",
            Availability::Drain => "drain",
            Availability::Decommissioned => "decommissioned",
        };
        let instances = state
            .assignments
            .values()
            .filter(|a| &a.node == name)
            .count();
        println!(
            "  {name}\t{:?}\t{reach}\t{avail}\tinstances={instances}\t{}",
            node.role, node.arch
        );
    }

    let total = state.assignments.len();
    let running = state
        .assignments
        .values()
        .filter(|a| a.desired == DesiredState::Running)
        .count();
    println!(
        "stacks {} deployed, {running}/{total} instances assigned to run",
        state.stacks.len()
    );

    let flags = ShowFlags {
        node: true,
        uptime: true,
        placement: true,
        logs: Some(LOG_TAIL),
    };
    let mut any = health::print_problems(cfg, &state, &flags).await?;
    any |= print_updates(&state, now);
    any |= print_backups(&state);
    any |= print_reconcile(&state, now);

    if !any {
        println!("all clear: no unhealthy instances or cluster faults");
    }
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn ago(now: i64, then: i64) -> String {
    humantime::format_duration(std::time::Duration::from_secs((now - then).max(0) as u64))
        .to_string()
}

/// Updates and redeploys that failed or have been going for too long.
/// Finished-ok ones are noise and are skipped; in-flight ones are shown, as
/// whoever is debugging a slow rollout wants to see them.
fn print_updates(state: &ClusterState, now: i64) -> bool {
    let mut lines = Vec::new();
    for (instance, update) in &state.instance_updates {
        let age = ago(now, update.started_at);
        match update.phase {
            UpdatePhase::Started => {}
            UpdatePhase::Failed => lines.push(format!(
                "  {instance}\tupdate FAILED on {} {age} ago: {}",
                update.node,
                update.error.as_deref().unwrap_or("no error recorded")
            )),
            phase => {
                let stuck = now - update.at > STUCK_UPDATE_SECS;
                lines.push(format!(
                    "  {instance}\tupdate {phase:?} on {} for {age}{}",
                    update.node,
                    if stuck {
                        format!(" - no progress for {}, likely stuck", ago(now, update.at))
                    } else {
                        String::new()
                    }
                ));
            }
        }
    }
    print_section("updates", &lines)
}

/// The latest run per backup target if it failed or warned, and restores that
/// failed or are still waiting.
fn print_backups(state: &ClusterState) -> bool {
    let mut lines = Vec::new();

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
            BackupRunOutcome::Failed { error } => lines.push(format!(
                "  backup {target} ({} -> {}) FAILED: {error}",
                run.drive, run.destination_id
            )),
            BackupRunOutcome::Succeeded {
                warning: Some(w), ..
            } => lines.push(format!("  backup {target} succeeded with warning: {w}")),
            _ => {}
        }
    }

    for request in state.restore_requests.values() {
        match &request.status {
            RestoreStatus::Failed { error } => lines.push(format!(
                "  restore {} ({:?}) FAILED: {error}",
                request.id, request.scope
            )),
            RestoreStatus::Pending | RestoreStatus::Running => lines.push(format!(
                "  restore {} ({:?}) is {:?}",
                request.id, request.scope, request.status
            )),
            RestoreStatus::Succeeded => {}
        }
    }
    print_section("backups", &lines)
}

/// Certificate issuance and DNS publishing that is failing or has not
/// completed.
fn print_reconcile(state: &ClusterState, now: i64) -> bool {
    let mut lines = Vec::new();
    for (kind, statuses) in [("cert", &state.cert_status), ("dns", &state.dns_status)] {
        for (name, status) in statuses {
            match status {
                ReconcileStatus::Pending => lines.push(format!("  {kind} {name}: pending")),
                ReconcileStatus::Error { message, at } => lines.push(format!(
                    "  {kind} {name}: ERROR {} ago: {message}",
                    ago(now, *at)
                )),
            }
        }
    }
    print_section("certificates & dns", &lines)
}

fn print_section(title: &str, lines: &[String]) -> bool {
    if lines.is_empty() {
        return false;
    }
    println!("{title}\t{} issue(s)", lines.len());
    for line in lines {
        println!("{line}");
    }
    true
}
