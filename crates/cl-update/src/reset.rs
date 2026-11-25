//! "Reset the configuration to a git commit": the replicated plan, the leader's decision to go, and
//! the offline step every node runs, in the current binary, before the older release is installed.
//!
//! The plan lives in the reserved config `_clouduler_reset_plan` (it rides the config CAS, like the
//! self-update run), so every node sees the same one and a new leader finds it. Phases:
//!
//! 1. `Preparing`: a self-update run in `hold` mode makes every node download the older release.
//! 2. `Go`: the leader commits the instant (`at`) and the synthetic raft log id (`term`, `index`,
//!    `leader`) every node will stamp, so the result is identical on all of them.
//! 3. Each node's daemon sees `Go`, waits for `at`, writes a marker file and shuts down cleanly;
//!    [`apply_marker`] then rewrites its raft store (`cl_raft::Store::reset_to`), writes the config
//!    file and installs the staged binary, and the process exits so systemd starts it again. The
//!    marker drives a resume if the node dies midway. The plan stored by that rewrite is `Done`.
//! 4. `Failed`: the run failed or was cancelled before `Go`; nothing was touched on any node.

use std::path::{Path, PathBuf};

use cl_raft::state::{ClusterState, Request};
use cl_raft::store::{ResetConfig, ResetContent, ResetGo};
use cl_raft::ClusterNode;
use serde::{Deserialize, Serialize};

/// Reserved config name the plan is stored under. Never exposed through `/api/configs`.
pub const RESET_CONFIG: &str = "_clouduler_reset_plan";

/// Seconds between committing `Go` and the nodes acting on it, so they stop together.
pub const GO_DELAY_SECS: i64 = 20;

/// How far past the leader's last log index the synthetic entry sits; any entry a node applies
/// after `Go` still has a lower index, so log ids only ever move forward.
const INDEX_MARGIN: u64 = 100_000;

/// Same for the term.
const TERM_MARGIN: u64 = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetPhase {
    Preparing,
    Go {
        /// Unix seconds before which no node acts.
        at: i64,
        term: u64,
        index: u64,
        /// Raft id of the leader that decided; the restarted nodes' committed vote points at it.
        leader: u64,
    },
    Done,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetPlan {
    pub id: String,
    /// Full sha of the commit the configuration goes back to.
    pub commit: String,
    /// Where the previous tip was preserved.
    pub branch: String,
    /// The release every node installs; `None` keeps the current binary.
    pub release_tag: Option<String>,
    /// The hold-mode self-update run that downloads `release_tag`.
    pub run_id: Option<String>,
    pub started_at: i64,
    pub phase: ResetPhase,
    pub content: ResetContent,
}

impl ResetPlan {
    /// Whether the cluster is between the force push and the end of the reboot.
    pub fn in_flight(&self) -> bool {
        matches!(self.phase, ResetPhase::Preparing | ResetPhase::Go { .. })
    }

    /// The `Go` phase for a leader at `now`, from its raft metrics.
    pub fn go_phase(cluster: &ClusterNode, now: i64) -> ResetPhase {
        let metrics = cluster.raft().metrics().borrow().clone();
        let applied = metrics.last_applied.map(|id| id.index).unwrap_or(0);
        let last = metrics.last_log_index.unwrap_or(0).max(applied);
        ResetPhase::Go {
            at: now + GO_DELAY_SECS,
            term: metrics.current_term + TERM_MARGIN,
            index: last + INDEX_MARGIN,
            leader: cluster.id,
        }
    }

    /// The raft write storing this plan, conditional on the object still being at `revision`.
    pub fn put_request(&self, revision: u64) -> Result<Request, String> {
        Ok(Request::PutConfigCas {
            name: RESET_CONFIG.to_string(),
            content: self.encode()?,
            owner: None,
            expect_revision: revision,
        })
    }

    /// Base64 of the json, as raft configs hold it.
    pub fn encode(&self) -> Result<String, String> {
        Ok(cl_raft::base64_encode(
            &serde_json::to_vec(self).map_err(|e| format!("encoding the reset plan: {e}"))?,
        ))
    }
}

/// The stored plan and its config revision.
pub fn load_revision(state: &ClusterState) -> Option<(ResetPlan, u64)> {
    let entry = state.configs.get(RESET_CONFIG)?;
    let raw = entry.decode().ok()?;
    Some((serde_json::from_slice(&raw).ok()?, entry.revision))
}

pub fn load(state: &ClusterState) -> Option<ResetPlan> {
    load_revision(state).map(|(plan, _)| plan)
}

/// A reset is pending: exports, other self-updates and a second reset wait for it.
pub fn in_flight(state: &ClusterState) -> bool {
    load(state).is_some_and(|plan| plan.in_flight())
}

// -- the node's offline step ------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Nothing written yet.
    Pending,
    /// The raft store holds the reset; the config file and binary are what is left.
    StoreDone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub stage: Stage,
    pub plan: ResetPlan,
}

/// Where the marker lives: next to the staged binary, in the node's data dir.
pub fn marker_path(update_dir: &Path) -> PathBuf {
    update_dir.join("reset-marker.json")
}

pub fn write_marker(update_dir: &Path, marker: &Marker) -> std::io::Result<()> {
    std::fs::create_dir_all(update_dir)?;
    let path = marker_path(update_dir);
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(marker).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path)
}

pub fn read_marker(update_dir: &Path) -> Option<Marker> {
    serde_json::from_slice(&std::fs::read(marker_path(update_dir)).ok()?).ok()
}

/// What a node needs to carry out the step.
pub struct NodePaths<'a> {
    /// The node's data dir (`raft/` lives in it).
    pub data_dir: &'a Path,
    /// The directory holding the staged binary and the marker.
    pub update_dir: &'a Path,
    /// The staged binary (the release `release_tag` names).
    pub artifact: &'a Path,
    pub config_path: &'a Path,
    pub current_exe: &'a Path,
}

/// The raft rewrite's input: the plan's content plus the plan itself, stored as `Done`, so the
/// restarted nodes do not act on `Go` again. Pure, so every node computes the same thing.
pub fn rewrite_input(plan: &ResetPlan) -> Result<(ResetGo, ResetContent), String> {
    let ResetPhase::Go {
        term,
        index,
        leader,
        ..
    } = plan.phase
    else {
        return Err("the reset plan is not in its go phase".to_string());
    };
    let mut done = plan.clone();
    done.phase = ResetPhase::Done;
    let mut content = plan.content.clone();
    content.configs.retain(|c| c.name != RESET_CONFIG);
    content.configs.push(ResetConfig {
        name: RESET_CONFIG.to_string(),
        content: done.encode()?,
        owner: None,
    });
    Ok((
        ResetGo {
            term,
            index,
            leader,
        },
        content,
    ))
}

/// Carries out a pending marker, if there is one. The raft store must not be open (the daemon has
/// shut down, or has not started yet). Returns whether a reset was applied, in which case the
/// process should exit so it is started again (on the installed release, if any).
///
/// Order matters: the store and the config file are rewritten by THIS binary, which understands
/// the marker; the older binary is installed last, and the marker removed after it.
pub async fn apply_marker(paths: &NodePaths<'_>, now: i64) -> Result<bool, String> {
    let Some(mut marker) = read_marker(paths.update_dir) else {
        return Ok(false);
    };
    if marker.stage == Stage::Pending {
        let (go, content) = rewrite_input(&marker.plan)?;
        cl_raft::node::reset(&paths.data_dir.join("raft"), &go, &content)
            .await
            .map_err(|e| format!("rewriting the raft store: {e}"))?;
        marker.stage = Stage::StoreDone;
        write_marker(paths.update_dir, &marker)
            .map_err(|e| format!("updating the reset marker: {e}"))?;
    }

    if let Some(yaml) = &marker.plan.content.cluster_yaml {
        write_config_file(paths.config_path, yaml)
            .map_err(|e| format!("writing {}: {e}", paths.config_path.display()))?;
    }

    if marker.plan.release_tag.is_some() {
        if let Err(e) = install_release(paths, now) {
            // The state is already reset; the current binary can carry it, the operator can
            // update by hand. Failing here would leave the node unable to start at all.
            tracing::error!(error = %e, "reset: could not install the older release, staying on this binary");
        }
    }

    match std::fs::remove_file(marker_path(paths.update_dir)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("removing the reset marker: {e}")),
    }
    Ok(true)
}

fn install_release(paths: &NodePaths<'_>, now: i64) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if !paths.artifact.exists() {
        return Err(format!("no staged binary at {}", paths.artifact.display()));
    }
    let hash = |path: &Path| std::fs::read(path).map(|b| blake3::hash(&b));
    // A resume after the install must not shuffle the `.old` safety copy over the real previous binary.
    let staged = hash(paths.artifact).map_err(|e| format!("reading the staged binary: {e}"))?;
    if hash(paths.current_exe).is_ok_and(|current| current == staged) {
        return Ok(());
    }
    std::fs::set_permissions(paths.artifact, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("preparing the staged binary: {e}"))?;
    crate::rollback::verify_runnable(paths.artifact)?;
    crate::install::install_binary(paths.artifact, paths.current_exe)
        .map_err(|e| format!("installing the staged binary: {e}"))?;
    crate::rollback::arm(paths.update_dir, paths.current_exe, now)
        .map_err(|e| format!("arming the rollback safety net: {e}"))
}

/// Replaces the config file in one rename, keeping its permissions.
fn write_config_file(path: &Path, yaml: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("reset-tmp");
    std::fs::write(&tmp, yaml)?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, meta.permissions())?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::store::{ResetStack, Store};

    fn plan(phase: ResetPhase) -> ResetPlan {
        ResetPlan {
            id: "reset-1".into(),
            commit: "a".repeat(40),
            branch: "rolled-back/x".into(),
            release_tag: None,
            run_id: None,
            started_at: 0,
            phase,
            content: ResetContent {
                cluster_yaml: Some("cluster: {}\n".into()),
                generation: 5,
                at: 100,
                stacks: vec![ResetStack {
                    name: "web".into(),
                    spec: "services: {}\n".into(),
                    configs: ["site.conf".to_string()].into(),
                    secrets: Default::default(),
                    used_configs: ["site.conf".to_string()].into(),
                    used_secrets: Default::default(),
                }],
                configs: vec![ResetConfig {
                    name: "site.conf".into(),
                    content: cl_raft::base64_encode(b"server {}"),
                    owner: Some("web".into()),
                }],
                remove_configs: vec!["old.conf".into()],
            },
        }
    }

    fn go() -> ResetPhase {
        ResetPhase::Go {
            at: 10,
            term: 12,
            index: 5000,
            leader: 7,
        }
    }

    #[test]
    fn only_a_go_plan_yields_a_rewrite_and_it_ends_as_done() {
        assert!(rewrite_input(&plan(ResetPhase::Preparing)).is_err());
        let (go_id, content) = rewrite_input(&plan(go())).expect("go");
        assert_eq!(
            go_id,
            ResetGo {
                term: 12,
                index: 5000,
                leader: 7
            }
        );
        let stored = content
            .configs
            .iter()
            .find(|c| c.name == RESET_CONFIG)
            .expect("plan config");
        let raw = cl_raft::base64_decode(&stored.content).expect("base64");
        let stored: ResetPlan = serde_json::from_slice(&raw).expect("json");
        assert_eq!(stored.phase, ResetPhase::Done);
        // Idempotent: feeding the result back does not duplicate the plan config.
        let (_, again) = rewrite_input(&plan(go())).expect("go");
        assert_eq!(content, again);
    }

    async fn seeded(dir: &Path, voters: &[u64]) -> Store {
        use openraft::storage::RaftStateMachine;
        let store = Store::open(dir.join("raft.redb")).expect("open");
        let mut sm = store.state_machine();
        let mut entries = Vec::new();
        for (i, request) in [
            Request::DeployStack {
                name: "web".into(),
                spec: "services:\n  web:\n    image: old\n".into(),
                at: 1,
            },
            Request::DeployStack {
                name: "gone".into(),
                spec: "services: {}\n".into(),
                at: 1,
            },
            Request::PutConfig {
                name: "old.conf".into(),
                content: cl_raft::base64_encode(b"old"),
                owner: None,
            },
            Request::PutConfig {
                name: "_clouduler_keep".into(),
                content: cl_raft::base64_encode(b"keep"),
                owner: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            entries.push(openraft::Entry {
                log_id: openraft::LogId::new(openraft::CommittedLeaderId::new(1, 1), i as u64 + 1),
                payload: openraft::EntryPayload::Normal(request),
            });
        }
        let nodes: std::collections::BTreeMap<u64, cl_raft::types::Node> = voters
            .iter()
            .map(|id| (*id, cl_raft::types::Node::new(format!("10.0.0.{id}:7373"))))
            .collect();
        entries.push(openraft::Entry {
            log_id: openraft::LogId::new(openraft::CommittedLeaderId::new(1, 1), 5),
            payload: openraft::EntryPayload::Membership(openraft::Membership::new(
                vec![voters.iter().copied().collect()],
                nodes,
            )),
        });
        sm.apply(entries).await.expect("apply");
        store
    }

    #[tokio::test]
    async fn two_nodes_rewrite_to_the_identical_state_and_keep_every_voter() {
        let (go_id, content) = rewrite_input(&plan(go())).expect("go");
        let mut results = Vec::new();
        for _ in 0..2 {
            let dir = tempfile::tempdir().expect("dir");
            {
                let store = seeded(dir.path(), &[1, 2, 3]).await;
                store.reset_to(&go_id, &content).await.expect("reset");
                // Idempotent: a resume after a crash changes nothing.
                store.reset_to(&go_id, &content).await.expect("again");
            }
            let store = Store::open(dir.path().join("raft.redb")).expect("reopen");
            let shared = store.shared_state();
            let sm = shared.read().await;
            let voters: Vec<u64> = sm.membership.voter_ids().collect();
            assert_eq!(voters, vec![1, 2, 3]);
            assert_eq!(
                sm.last_applied,
                Some(openraft::LogId::new(
                    openraft::CommittedLeaderId::new(12, 7),
                    5000
                ))
            );
            assert_eq!(
                sm.data.cluster_config.as_ref().map(|c| c.generation),
                Some(5)
            );
            assert!(!sm.data.stacks.contains_key("gone"));
            assert_eq!(sm.data.stacks["web"].spec, "services: {}\n");
            assert_eq!(sm.data.stacks["web"].revision, 2);
            assert!(sm.data.stacks["web"].configs.contains("site.conf"));
            assert!(sm.data.configs.contains_key("site.conf"));
            assert!(!sm.data.configs.contains_key("old.conf"));
            assert!(sm.data.configs.contains_key("_clouduler_keep"));
            let stored = load(&sm.data).expect("plan stored");
            assert_eq!(stored.phase, ResetPhase::Done);
            results.push((
                sm.last_applied,
                sm.membership.clone(),
                sm.data.stacks.clone(),
                sm.data.configs.clone(),
                sm.data.cluster_config.clone(),
            ));
        }
        assert_eq!(results[0], results[1]);
    }

    #[tokio::test]
    async fn the_marker_drives_a_resume_and_is_removed_at_the_end() {
        let dir = tempfile::tempdir().expect("dir");
        let data = dir.path().join("data");
        let update = data.join("self-update");
        std::fs::create_dir_all(data.join("raft")).unwrap();
        drop(seeded(&data.join("raft"), &[1]).await);
        let config = dir.path().join("cluster.yaml");
        std::fs::write(&config, "old").unwrap();
        let exe = dir.path().join("clouduler");
        let paths = NodePaths {
            data_dir: &data,
            update_dir: &update,
            artifact: &update.join("artifact"),
            config_path: &config,
            current_exe: &exe,
        };

        assert!(
            !apply_marker(&paths, 0).await.unwrap(),
            "no marker, nothing to do"
        );
        write_marker(
            &update,
            &Marker {
                stage: Stage::Pending,
                plan: plan(go()),
            },
        )
        .unwrap();
        assert!(apply_marker(&paths, 0).await.unwrap());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "cluster: {}\n");
        assert!(read_marker(&update).is_none());
        assert!(!apply_marker(&paths, 0).await.unwrap(), "already applied");
    }
}
