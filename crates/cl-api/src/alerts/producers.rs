//! Node-side producers: what only the node itself can see. They push signals
//! into raft; the leader's engine decides what to do with them. Thresholds
//! are read from the replicated settings on every pass.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cl_alert::Severity;
use cl_config::AlertSeverity;
use cl_docker::ContainerEvent;
use cl_raft::alerts::SignalAction;
use futures::StreamExt;

use super::{new_signal, push_signal, settings, sev_back, unix_now};
use crate::server::AppState;

const DISK_EVERY: Duration = Duration::from_secs(60);
const MEMORY_EVERY: Duration = Duration::from_secs(15);
/// A reading this far under the threshold closes the alert, so a value
/// hovering around the line does not open and close it all day.
const DISK_HYSTERESIS: u8 = 2;
const MEMORY_HYSTERESIS: u8 = 3;

/// Start the disk, memory and out-of-memory producers of this node.
pub fn spawn(state: AppState) {
    tokio::spawn(disk_loop(state.clone()));
    tokio::spawn(memory_loop(state.clone()));
    tokio::spawn(oom_loop(state));
}

async fn thresholds(state: &AppState) -> cl_alert::Thresholds {
    state.cluster.read(|st| settings::load(st).thresholds).await
}

/// Which band a reading falls in, given where it was before.
pub fn disk_band(
    used_percent: f64,
    previous: Option<AlertSeverity>,
    t: &cl_alert::Thresholds,
) -> Option<AlertSeverity> {
    let warn = t.disk_warning_percent as f64;
    let crit = t.disk_critical_percent as f64;
    let h = DISK_HYSTERESIS as f64;
    let held = |level: Option<AlertSeverity>, at: f64| level == previous && used_percent >= at - h;
    if used_percent >= crit || held(Some(AlertSeverity::Critical), crit) {
        Some(AlertSeverity::Critical)
    } else if used_percent >= warn
        || held(Some(AlertSeverity::Warning), warn)
        || (previous == Some(AlertSeverity::Critical) && used_percent >= warn - h)
    {
        Some(AlertSeverity::Warning)
    } else {
        None
    }
}

async fn disk_loop(state: AppState) {
    let mut bands: BTreeMap<String, Option<AlertSeverity>> = BTreeMap::new();
    let mut first = true;
    loop {
        let drives = local_drives(&state).await;
        let t = thresholds(&state).await;
        let spaces = tokio::task::spawn_blocking(move || crate::node_metrics::drive_space(&drives))
            .await
            .unwrap_or_default();
        for space in spaces.into_iter().filter(|s| s.total_bytes > 0) {
            let used = 100.0 * (space.total_bytes - space.free_bytes.min(space.total_bytes)) as f64
                / space.total_bytes as f64;
            let prev = bands.get(&space.drive).copied().flatten();
            let band = disk_band(used, prev, &t);
            let known = bands.contains_key(&space.drive);
            if known && band == prev {
                continue;
            }
            bands.insert(space.drive.clone(), band);
            let dedupe = format!("node.disk_low:{}:{}", state.node_name, space.drive);
            match band {
                Some(sev) => {
                    let mut s = new_signal("node.disk_low", dedupe, SignalAction::Open);
                    s.severity = Some(sev);
                    s.title = format!(
                        "Drive {} on {} is {used:.0}% full",
                        space.drive, state.node_name
                    );
                    s.detail = format!(
                        "{} free of {} on {}",
                        bytes(space.free_bytes),
                        bytes(space.total_bytes),
                        space.path
                    );
                    s.subject = format!("{}/{}", state.node_name, space.drive);
                    s.node = Some(state.node_name.clone());
                    push_signal(&state.cluster, s).await;
                }
                // Right after start, clear anything a previous run left open.
                None if first || known => {
                    let s = new_signal("node.disk_low", dedupe, SignalAction::Resolve);
                    push_signal(&state.cluster, s).await;
                }
                None => {}
            }
        }
        first = false;
        tokio::time::sleep(DISK_EVERY).await;
    }
}

/// Drives this node holds real data for (not the mountpoint of a served drive
/// that lives on another node).
async fn local_drives(state: &AppState) -> BTreeMap<String, PathBuf> {
    let drives = state
        .cluster
        .read(|st| st.nodes.get(&state.node_name).map(|n| n.drives.clone()))
        .await
        .unwrap_or_default();
    let cfg = state.cluster_config.read().await;
    drives
        .into_iter()
        .filter(|(name, _)| match cfg.drives.get(name) {
            Some(d) if d.is_served() => d.served_by.iter().any(|n| n == &state.node_name),
            _ => true,
        })
        .collect()
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

async fn memory_loop(state: AppState) {
    let mut system = sysinfo::System::new();
    let mut above_since: Option<Instant> = None;
    let mut open = false;
    let mut first = true;
    loop {
        tokio::time::sleep(MEMORY_EVERY).await;
        system.refresh_memory();
        let total = system.total_memory();
        if total == 0 {
            continue;
        }
        let used = 100.0 * system.used_memory() as f64 / total as f64;
        let t = thresholds(&state).await;
        let limit = t.memory_percent as f64;
        let dedupe = format!("node.memory_pressure:{}", state.node_name);
        if used >= limit {
            let since = *above_since.get_or_insert_with(Instant::now);
            if !open && since.elapsed() >= t.memory_for {
                open = true;
                let mut s = new_signal("node.memory_pressure", dedupe, SignalAction::Open);
                s.severity = Some(sev_back(Severity::Warning));
                s.title = format!("{} has used {used:.0}% of its memory", state.node_name);
                s.detail = format!(
                    "Above {}% for {}. Containers may get killed for memory.",
                    t.memory_percent,
                    humantime::format_duration(Duration::from_secs(t.memory_for.as_secs()))
                );
                s.subject = state.node_name.clone();
                s.node = Some(state.node_name.clone());
                push_signal(&state.cluster, s).await;
            }
        } else if used < limit - MEMORY_HYSTERESIS as f64 {
            above_since = None;
            if open || first {
                open = false;
                let s = new_signal("node.memory_pressure", dedupe, SignalAction::Resolve);
                push_signal(&state.cluster, s).await;
            }
        }
        first = false;
    }
}

async fn oom_loop(state: AppState) {
    let mut last_seen: Option<(String, Option<String>)> = None;
    loop {
        let docker = state.docker.read().await.clone();
        let Some(docker) = docker else {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        };
        let mut events = Box::pin(docker.events());
        while let Some(event) = events.next().await {
            let id = match event {
                Ok(ContainerEvent::Died { id, .. }) => id,
                Ok(_) => continue,
                Err(_) => break,
            };
            let Ok(Some(c)) = docker.inspect(&id).await else {
                continue;
            };
            if !c.oom_killed {
                continue;
            }
            // `die` can be reported more than once for the same death.
            let marker = (c.id.clone(), c.finished_at.clone());
            if last_seen.as_ref() == Some(&marker) {
                continue;
            }
            last_seen = Some(marker);
            let (Some(instance), stack) = (
                c.labels.get(cl_docker::LABEL_INSTANCE).cloned(),
                c.labels.get(cl_docker::LABEL_STACK).cloned(),
            ) else {
                continue;
            };
            let mut s = new_signal(
                "instance.oom_killed",
                format!("instance.oom_killed:{instance}"),
                SignalAction::Once,
            );
            s.title = format!("{instance} was killed for using too much memory");
            s.detail = match c.memory_limit_bytes {
                Some(limit) => format!(
                    "The container hit its memory limit of {}. Raise deploy.resources.limits.memory or fix the leak.",
                    bytes(limit as u64)
                ),
                None => "The kernel killed it because the node ran out of memory.".to_string(),
            };
            s.subject = instance.clone();
            s.stack = stack;
            s.instance = Some(instance);
            s.node = Some(state.node_name.clone());
            push_signal(&state.cluster, s).await;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Report how this node's last self-update went, once, when it is known.
///
/// `rolled_back` is the reason `cl_update::rollback::take_note` returned at
/// boot. While `probation` holds, the new binary has not yet proven itself;
/// the moment the marker is cleared the update is reported as completed.
pub fn spawn_self_update_report(
    state: AppState,
    update_dir: PathBuf,
    probation: bool,
    rolled_back: Option<String>,
) {
    tokio::spawn(async move {
        if let Some(reason) = rolled_back {
            let mut s = new_signal(
                "self_update.rolled_back",
                format!("self_update.rolled_back:{}", state.node_name),
                SignalAction::Once,
            );
            s.title = format!("{} went back to its previous binary", state.node_name);
            s.detail = format!(
                "The new version could not be trusted ({reason}). This node now runs a different version from the rest of the cluster."
            );
            s.subject = state.node_name.clone();
            s.node = Some(state.node_name.clone());
            retry_push(&state, s).await;
        }
        if probation {
            while cl_update::rollback::is_pending(&update_dir) {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            let version = env!("CARGO_PKG_VERSION");
            let mut s = new_signal(
                "self_update.completed",
                format!("self_update.completed:{}", state.node_name),
                SignalAction::Once,
            );
            s.title = format!("{} updated to {version}", state.node_name);
            s.detail = "The new binary started and rejoined the cluster.".to_string();
            s.subject = state.node_name.clone();
            s.node = Some(state.node_name.clone());
            retry_push(&state, s).await;
        }
    });
}

/// A node that just restarted may not have a leader to talk to yet.
async fn retry_push(state: &AppState, signal: cl_raft::alerts::AlertSignal) {
    for _ in 0..60 {
        if state
            .cluster
            .submit(cl_raft::Request::PushAlertSignal(signal.clone()))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let _ = unix_now();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_bands_use_hysteresis_and_live_thresholds() {
        let mut t = cl_alert::Thresholds::default(); // 90 / 95
        assert_eq!(disk_band(50.0, None, &t), None);
        assert_eq!(disk_band(91.0, None, &t), Some(AlertSeverity::Warning));
        assert_eq!(
            disk_band(96.0, Some(AlertSeverity::Warning), &t),
            Some(AlertSeverity::Critical)
        );
        // Just under critical: stays critical until clearly below.
        assert_eq!(
            disk_band(94.0, Some(AlertSeverity::Critical), &t),
            Some(AlertSeverity::Critical)
        );
        assert_eq!(
            disk_band(92.0, Some(AlertSeverity::Critical), &t),
            Some(AlertSeverity::Warning)
        );
        // Just under warning: stays open; well under: closes.
        assert_eq!(
            disk_band(89.0, Some(AlertSeverity::Warning), &t),
            Some(AlertSeverity::Warning)
        );
        assert_eq!(disk_band(80.0, Some(AlertSeverity::Warning), &t), None);
        // Lower thresholds, read on the next pass, change the verdict.
        t.disk_warning_percent = 60;
        assert_eq!(disk_band(80.0, None, &t), Some(AlertSeverity::Warning));
    }
}
