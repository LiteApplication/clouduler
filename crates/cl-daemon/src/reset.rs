//! The node's side of a configuration reset to a git commit (`cl_update::reset`).
//!
//! [`start_watcher`] runs on every node: once the replicated plan says `Go` and its instant has
//! come, it writes the marker and asks the daemon to shut down. [`apply_pending`] then runs the
//! offline rewrite, with raft not running, in this binary. The daemon calls it first thing at boot,
//! so the restart that follows the shutdown does it, and so does a node that died midway.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cl_raft::ClusterNode;
use cl_update::reset::{self, Marker, NodePaths, ResetPhase, Stage};
use tracing::{error, info, warn};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where the staged binary and the marker live (`<data>/self-update`).
pub fn update_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("self-update")
}

pub fn start_watcher(
    cluster: Arc<ClusterNode>,
    data_dir: PathBuf,
    shutdown: tokio::sync::watch::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let update_dir = update_dir(&data_dir);
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let state = cluster.snapshot().await;
            let Some(plan) = reset::load(&state) else {
                continue;
            };
            let ResetPhase::Go { at, .. } = plan.phase else {
                continue;
            };
            if unix_now() < at {
                continue;
            }
            info!(commit = %plan.commit, "configuration reset: stopping to rewrite the raft store");
            let marker = Marker {
                stage: Stage::Pending,
                plan,
            };
            if let Err(e) = reset::write_marker(&update_dir, &marker) {
                error!(error = %e, "configuration reset: cannot write the marker, not stopping");
                continue;
            }
            let _ = shutdown.send(());
            return;
        }
    })
}

/// Carries out a pending marker (see [`reset::apply_marker`]). `Ok(true)` means the store was
/// rewritten and the process should exit now so it is started again.
pub async fn apply_pending(
    data_dir: &Path,
    config_path: &Path,
    artifact: &Path,
) -> anyhow::Result<bool> {
    let update_dir = update_dir(data_dir);
    if reset::read_marker(&update_dir).is_none() {
        return Ok(false);
    }
    let current_exe = std::env::current_exe()?;
    let paths = NodePaths {
        data_dir,
        update_dir: &update_dir,
        artifact,
        config_path,
        current_exe: &current_exe,
    };
    match reset::apply_marker(&paths, unix_now()).await {
        Ok(applied) => {
            if applied {
                warn!("configuration reset applied, restarting");
            }
            Ok(applied)
        }
        Err(e) => Err(anyhow::anyhow!("configuration reset: {e}")),
    }
}
