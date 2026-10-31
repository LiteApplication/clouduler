//! Putting the previous binary back when a freshly installed one cannot be
//! trusted.
//!
//! A node cannot ask the cluster to roll it back: the failure this guards
//! against is the node not being in the cluster any more. So the decision is
//! local and survives restarts through a marker file next to the staged
//! artifact:
//!
//! 1. the restart route `arm`s it right after `install_binary`;
//! 2. every start of the daemon calls `on_boot`, which counts the start and
//!    swaps `<binary>.old` back in when the new binary keeps dying before it
//!    is confirmed, or when the deadline has already passed;
//! 3. a daemon that does start runs `watch`, which clears the marker once the
//!    node is back in the cluster and rolls back if it never gets there
//!    (hung, or up but unable to rejoin).
//!
//! In every case the process then exits and the unit's `Restart=always`
//! brings it up again on the old binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cl_raft::ClusterNode;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::install::previous_path;

/// Starts of an unconfirmed binary tolerated before rolling back. A crash
/// loop gets there in about `MAX_BOOTS * RestartSec`.
pub const MAX_BOOTS: u32 = 3;

/// How long after the install the node has to be back in the cluster. Shorter
/// than the orchestrator's own restart timeout, so the rollback is already
/// under way when the run is declared failed.
pub const REJOIN_DEADLINE: Duration = Duration::from_secs(150);

const PENDING: &str = "pending.json";
const NOTE: &str = "rolled-back.json";

/// An installed binary that has not yet proven itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The installed executable; `previous_path(binary)` is what it replaced.
    pub binary: PathBuf,
    /// Unix seconds the new binary was installed.
    pub armed_at: i64,
    /// Starts of the daemon since then.
    #[serde(default)]
    pub boots: u32,
}

/// What `on_boot` decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Boot {
    /// No install is awaiting confirmation.
    Normal,
    /// The binary is on probation: run `watch`.
    Confirming,
    /// The previous binary has been put back; this process must exit so the
    /// service manager starts that one.
    RolledBack(String),
}

fn pending_path(dir: &Path) -> PathBuf {
    dir.join(PENDING)
}

fn load(dir: &Path) -> Option<Pending> {
    serde_json::from_slice(&std::fs::read(pending_path(dir)).ok()?).ok()
}

fn store(dir: &Path, pending: &Pending) -> std::io::Result<()> {
    let tmp = dir.join(format!("{PENDING}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(pending)?)?;
    std::fs::rename(tmp, pending_path(dir))
}

/// Whether an unconfirmed install is on record in `dir`.
pub fn is_pending(dir: &Path) -> bool {
    pending_path(dir).exists()
}

/// Record that `binary` was just installed over a previous one. Without a
/// `.old` there is nothing to go back to, so no marker is kept.
pub fn arm(dir: &Path, binary: &Path, now: i64) -> std::io::Result<()> {
    if !previous_path(binary).exists() {
        return confirm(dir);
    }
    std::fs::create_dir_all(dir)?;
    store(
        dir,
        &Pending {
            binary: binary.to_path_buf(),
            armed_at: now,
            boots: 0,
        },
    )
}

/// The new binary is healthy: stop watching it.
pub fn confirm(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(pending_path(dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Called first thing on every daemon start.
pub fn on_boot(dir: &Path, now: i64) -> Boot {
    let Some(mut pending) = load(dir) else {
        return Boot::Normal;
    };
    pending.boots += 1;
    let reason = if pending.boots > MAX_BOOTS {
        Some(format!(
            "the new binary was started {} times without joining the cluster",
            pending.boots - 1
        ))
    } else if now - pending.armed_at > REJOIN_DEADLINE.as_secs() as i64 {
        Some("the new binary did not rejoin the cluster in time".to_string())
    } else {
        None
    };
    match reason {
        None => {
            if let Err(e) = store(dir, &pending) {
                warn!(error = %e, "cannot count this start of an unconfirmed binary");
            }
            Boot::Confirming
        }
        Some(reason) => match roll_back(dir, &pending, &reason, now) {
            Ok(()) => Boot::RolledBack(reason),
            Err(e) => {
                error!(error = %e, "cannot roll back to the previous binary");
                let _ = confirm(dir);
                Boot::Normal
            }
        },
    }
}

fn roll_back(dir: &Path, pending: &Pending, reason: &str, now: i64) -> std::io::Result<()> {
    // A rename over the (possibly running) executable swaps the directory
    // entry, like the install did.
    std::fs::rename(previous_path(&pending.binary), &pending.binary)?;
    let _ = std::fs::write(
        dir.join(NOTE),
        serde_json::to_vec(&serde_json::json!({ "at": now, "reason": reason }))?,
    );
    confirm(dir)
}

/// The reason of the last rollback, once: for the daemon that comes up on the
/// old binary to report it.
pub fn take_note(dir: &Path) -> Option<String> {
    let path = dir.join(NOTE);
    let raw = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(path);
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    Some(value.get("reason")?.as_str()?.to_string())
}

/// Refuse a binary that cannot even print its version, before it replaces
/// one that works: a wrong architecture or a truncated download never gets
/// far enough to run any of the code above.
pub fn verify_runnable(path: &Path) -> Result<(), String> {
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("the downloaded binary cannot be executed: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("the downloaded binary exited with {status}")),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                return Err("the downloaded binary did not answer --version".into());
            }
            Err(e) => return Err(format!("waiting for the downloaded binary: {e}")),
        }
    }
}

/// Whether this node is back as a working member: on the roster, not marked
/// unreachable, and a leader is known.
async fn rejoined(cluster: &ClusterNode, node: &str) -> bool {
    let state = cluster.state().await;
    state.nodes.contains_key(node)
        && !state.unreachable.contains(node)
        && cluster.quorum().leader().is_some()
}

/// Runs in a daemon that started on an unconfirmed binary. Confirms it once
/// the node has been back in the cluster across two checks, or rolls back
/// and asks the daemon to shut down when the deadline passes first.
pub async fn watch(
    cluster: Arc<ClusterNode>,
    node: String,
    dir: PathBuf,
    shutdown: tokio::sync::watch::Sender<()>,
    now: impl Fn() -> i64,
) {
    const CHECK: Duration = Duration::from_secs(5);
    let mut good_checks = 0;
    loop {
        tokio::time::sleep(CHECK).await;
        let Some(pending) = load(&dir) else {
            return;
        };
        if rejoined(&cluster, &node).await {
            good_checks += 1;
            if good_checks >= 2 {
                match confirm(&dir) {
                    Ok(()) => info!("the updated binary rejoined the cluster, keeping it"),
                    Err(e) => warn!(error = %e, "cannot clear the update marker"),
                }
                return;
            }
            continue;
        }
        good_checks = 0;
        let now = now();
        if now - pending.armed_at <= REJOIN_DEADLINE.as_secs() as i64 {
            continue;
        }
        let reason = "the new binary did not rejoin the cluster in time";
        match roll_back(&dir, &pending, reason, now) {
            Ok(()) => {
                error!("{reason}: rolled back to the previous binary, restarting");
                let _ = shutdown.send(());
                // A hung daemon may not shut down cleanly; the unit restarts
                // it either way.
                tokio::time::sleep(Duration::from_secs(25)).await;
                std::process::exit(1);
            }
            Err(e) => {
                error!(error = %e, "cannot roll back to the previous binary");
                let _ = confirm(&dir);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory holding `clouduler` (the new one) and `clouduler.old`.
    fn installed() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let binary = root.path().join("clouduler");
        std::fs::write(&binary, b"new").expect("write");
        std::fs::write(previous_path(&binary), b"old").expect("write old");
        let dir = root.path().join("self-update");
        (root, binary, dir)
    }

    #[test]
    fn without_a_marker_the_boot_is_normal() {
        let (_root, _binary, dir) = installed();
        assert_eq!(on_boot(&dir, 0), Boot::Normal);
    }

    #[test]
    fn a_first_install_with_nothing_to_go_back_to_is_not_watched() {
        let (_root, binary, dir) = installed();
        std::fs::remove_file(previous_path(&binary)).expect("rm");
        arm(&dir, &binary, 0).expect("arm");
        assert!(!is_pending(&dir));
    }

    #[test]
    fn a_binary_that_keeps_dying_is_rolled_back_after_max_boots() {
        let (_root, binary, dir) = installed();
        arm(&dir, &binary, 100).expect("arm");
        for _ in 0..MAX_BOOTS {
            assert_eq!(on_boot(&dir, 101), Boot::Confirming);
        }
        assert!(matches!(on_boot(&dir, 102), Boot::RolledBack(_)));
        assert_eq!(std::fs::read(&binary).expect("read"), b"old");
        assert!(!is_pending(&dir));
        assert!(take_note(&dir).is_some());
        assert!(take_note(&dir).is_none());
        assert_eq!(on_boot(&dir, 103), Boot::Normal);
    }

    #[test]
    fn a_start_after_the_deadline_rolls_back() {
        let (_root, binary, dir) = installed();
        arm(&dir, &binary, 100).expect("arm");
        let late = 100 + REJOIN_DEADLINE.as_secs() as i64 + 1;
        assert!(matches!(on_boot(&dir, late), Boot::RolledBack(_)));
        assert_eq!(std::fs::read(&binary).expect("read"), b"old");
    }

    #[test]
    fn a_confirmed_binary_is_kept() {
        let (_root, binary, dir) = installed();
        arm(&dir, &binary, 100).expect("arm");
        assert_eq!(on_boot(&dir, 101), Boot::Confirming);
        confirm(&dir).expect("confirm");
        assert_eq!(on_boot(&dir, 102), Boot::Normal);
        assert_eq!(std::fs::read(&binary).expect("read"), b"new");
    }

    #[test]
    fn a_binary_that_cannot_run_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let junk = dir.path().join("junk");
        std::fs::write(&junk, b"not an executable").expect("write");
        std::fs::set_permissions(&junk, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        assert!(verify_runnable(&junk).is_err());
        assert!(verify_runnable(&dir.path().join("missing")).is_err());
    }
}
