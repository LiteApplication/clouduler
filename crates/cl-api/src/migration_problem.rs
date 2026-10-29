//! Turns a failed move into something an operator can act on.
//!
//! Raft records only `Failed { reason }`, the `Display` text of the
//! `DriveError` that stopped the driver. This module parses that text back
//! into a structured problem (what went wrong, which files differ, who else
//! uses the volume) and proposes the actions that make sense for it in the
//! current cluster state. Records already in the log are plain strings, so
//! the parsing is deliberately tolerant: anything it does not recognise
//! becomes `Other` with the raw text intact.

use std::collections::BTreeSet;

use cl_raft::state::{Migration, MigrationPhase};
use cl_raft::{ClusterState, DesiredState, InstanceId};
use serde::{Deserialize, Serialize};

/// Most paths returned per list; the total is reported separately.
const MAX_FILES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// The target's copy differed from the source after the sync.
    VerifyMismatch,
    /// The sync refused to run because it would have wiped data.
    SyncRefused,
    /// The copy itself failed (network, protocol, missing volume, io).
    SyncFailed,
    /// The container on the source would not stop.
    StopFailed,
    /// The container would not start on the target, after the switch.
    StartFailed,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoWriter {
    pub instance: String,
    pub node: String,
    /// The cluster wants it running (it is not stopped or mid-move).
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProblemAction {
    /// `retry`, `dismiss`, `restart`, `move_elsewhere`, `view_logs` or
    /// `view_volume`. The last three are navigation the ui performs itself.
    pub id: String,
    pub label: String,
    pub description: String,
    pub primary: bool,
    pub danger: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationProblem {
    pub kind: ProblemKind,
    pub title: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Differing paths across all three lists before the cap.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub files_total: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub co_writers: Vec<CoWriter>,
    pub from: String,
    pub to: String,
    pub raw: String,
    pub actions: Vec<ProblemAction>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// What the failure text says, before the cluster state is consulted.
#[derive(Debug, Clone, PartialEq)]
enum Parsed {
    Verify {
        volume: Option<String>,
        added: Vec<String>,
        changed: Vec<String>,
        removed: Vec<String>,
    },
    Refused {
        volume: Option<String>,
        why: String,
    },
    Stop,
    Start,
    Transfer(String),
    Other,
}

fn parse(reason: &str) -> Parsed {
    if let Some(rest) = reason.strip_prefix("verification failed: ") {
        return parse_verify(rest);
    }
    if let Some(rest) = reason.strip_prefix("refusing to sync ") {
        return match rest.split_once(": ") {
            Some((volume, why)) => Parsed::Refused {
                volume: Some(volume.to_string()),
                why: why.to_string(),
            },
            None => Parsed::Refused {
                volume: None,
                why: rest.to_string(),
            },
        };
    }
    if let Some(rest) = reason.strip_prefix("transfer failed: ") {
        // The docker control wraps these as "stopping <id>: ..." /
        // "starting <id>: ...".
        if rest.starts_with("stopping ") {
            return Parsed::Stop;
        }
        if rest.starts_with("starting ") {
            return Parsed::Start;
        }
        return Parsed::Transfer(rest.to_string());
    }
    if reason.starts_with("volume ") && reason.ends_with(" not found")
        || reason.starts_with("io error at ")
        || reason.starts_with("path ") && reason.ends_with(" escapes the drive root")
    {
        return Parsed::Transfer(reason.to_string());
    }
    Parsed::Other
}

fn parse_verify(rest: &str) -> Parsed {
    const MARK: &str = ": target differs after sync (";
    let Some((volume, lists)) = rest.split_once(MARK) else {
        return Parsed::Verify {
            volume: None,
            added: vec![],
            changed: vec![],
            removed: vec![],
        };
    };
    let lists = lists.strip_suffix(')').unwrap_or(lists);
    let mut it = Lists::new(lists);
    let added = it.list("added=");
    let changed = it.list("changed=");
    let removed = it.list("removed=");
    Parsed::Verify {
        volume: Some(volume.to_string()),
        added,
        changed,
        removed,
    }
}

/// Reads the `{:?}` rendering of several `Vec<String>`s in order.
struct Lists<'a> {
    s: &'a str,
}

impl<'a> Lists<'a> {
    fn new(s: &'a str) -> Self {
        Self { s }
    }

    fn list(&mut self, key: &str) -> Vec<String> {
        let Some(at) = self.s.find(key) else {
            return vec![];
        };
        self.s = &self.s[at + key.len()..];
        let Some(mut rest) = self.s.strip_prefix('[') else {
            return vec![];
        };
        let mut out = Vec::new();
        loop {
            rest = rest.trim_start_matches([',', ' ']);
            if let Some(after) = rest.strip_prefix(']') {
                self.s = after;
                return out;
            }
            let Some((value, after)) = debug_string(rest) else {
                self.s = "";
                return out;
            };
            out.push(value);
            rest = after;
        }
    }
}

/// Parses one `{:?}` string literal off the front of `s`.
fn debug_string(s: &str) -> Option<(String, &str)> {
    let mut chars = s.strip_prefix('"')?.char_indices();
    let body = &s[1..];
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &body[i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                'u' => {
                    if chars.next()?.1 != '{' {
                        return None;
                    }
                    let mut hex = String::new();
                    loop {
                        let (_, h) = chars.next()?;
                        if h == '}' {
                            break;
                        }
                        hex.push(h);
                    }
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// Instances other than `instance` whose service mounts any of `volumes`.
fn co_writers(
    state: &ClusterState,
    instance: &InstanceId,
    volumes: &BTreeSet<String>,
) -> Vec<CoWriter> {
    if volumes.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for (id, a) in &state.assignments {
        if id == instance {
            continue;
        }
        let mounts = state
            .stacks
            .get(&id.stack)
            .and_then(|s| cl_config::StackFile::parse(&s.spec).ok())
            .and_then(|f| f.service_requirements().remove(&id.service))
            .map(|r| r.volumes)
            .unwrap_or_default();
        if mounts.is_disjoint(volumes) {
            continue;
        }
        out.push(CoWriter {
            instance: id.to_string(),
            node: a.node.clone(),
            running: a.desired == DesiredState::Running
                && a.migration.as_ref().is_none_or(|m| m.phase.is_terminal()),
        });
    }
    out
}

/// Volumes `instance`'s service mounts, as `drive/volume`.
fn volumes_mounted(state: &ClusterState, instance: &InstanceId) -> BTreeSet<String> {
    state
        .stacks
        .get(&instance.stack)
        .and_then(|s| cl_config::StackFile::parse(&s.spec).ok())
        .and_then(|f| f.service_requirements().remove(&instance.service))
        .map(|r| r.volumes)
        .unwrap_or_default()
}

fn short(id: &str, own_stack: &str) -> String {
    id.strip_prefix(&format!("{own_stack}."))
        .unwrap_or(id)
        .to_string()
}

fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn action(
    id: &str,
    label: &str,
    description: &str,
    primary: bool,
    danger: bool,
    confirm: Option<&str>,
) -> ProblemAction {
    ProblemAction {
        id: id.into(),
        label: label.into(),
        description: description.into(),
        primary,
        danger,
        confirm: confirm.map(Into::into),
    }
}

/// The problem for `instance`'s failed move, or `None` when it has none.
pub fn classify(state: &ClusterState, instance: &InstanceId) -> Option<MigrationProblem> {
    let assignment = state.assignments.get(instance)?;
    let migration = assignment.migration.as_ref()?;
    let MigrationPhase::Failed { reason } = &migration.phase else {
        return None;
    };
    Some(classify_failed(
        state,
        instance,
        migration,
        &assignment.node,
        reason,
    ))
}

fn classify_failed(
    state: &ClusterState,
    instance: &InstanceId,
    migration: &Migration,
    assigned_node: &str,
    reason: &str,
) -> MigrationProblem {
    let (from, to) = (migration.from.as_str(), migration.to.as_str());
    // `Switching` repoints the assignment, so this is where the container is.
    let switched = assigned_node == to;
    let name = short(&instance.to_string(), &instance.stack);
    let parsed = if switched {
        // Past the point of no return the only thing left to fail is the
        // start; whatever the text says, it is about the target.
        Parsed::Start
    } else {
        parse(reason)
    };

    let mounted = volumes_mounted(state, instance);
    let can_retry = !switched && state.nodes.contains_key(to);
    let retry_label = format!("Try moving to {to} again");

    let mut p = MigrationProblem {
        kind: ProblemKind::Other,
        title: String::new(),
        detail: String::new(),
        volume: None,
        added: vec![],
        changed: vec![],
        removed: vec![],
        files_total: 0,
        co_writers: vec![],
        from: from.to_string(),
        to: to.to_string(),
        raw: reason.to_string(),
        actions: vec![],
    };
    let on_source = format!("{name} is still running on {from}.");
    let dismiss = |label: &str, description: &str, primary: bool| {
        action("dismiss", label, description, primary, false, None)
    };
    let move_elsewhere = action(
        "move_elsewhere",
        "Move to another node",
        "Pick a different node and start a new move.",
        false,
        false,
        None,
    );

    match parsed {
        Parsed::Verify {
            volume,
            added,
            changed,
            removed,
        } => {
            p.kind = ProblemKind::VerifyMismatch;
            p.files_total = added.len() + changed.len() + removed.len();
            p.added = added.into_iter().take(MAX_FILES).collect();
            p.changed = changed.into_iter().take(MAX_FILES).collect();
            p.removed = removed.into_iter().take(MAX_FILES).collect();
            let vols: BTreeSet<String> = match &volume {
                Some(v) => BTreeSet::from([v.clone()]),
                None => mounted.clone(),
            };
            p.co_writers = co_writers(state, instance, &vols);
            p.volume = volume;
            p.title = format!("The copy on {to} didn't match after the move");
            let running: Vec<String> = p
                .co_writers
                .iter()
                .filter(|c| c.running)
                .map(|c| short(&c.instance, &instance.stack))
                .collect();
            let vol = p.volume.as_deref().unwrap_or("the volume");
            p.detail = if running.is_empty() {
                format!(
                    "Files in {vol} changed while they were being copied, so nothing was switched. {on_source}"
                )
            } else {
                format!(
                    "Files in {vol} changed while they were being copied. {} also uses this volume and kept running, so it most likely wrote to it. Nothing was switched. {on_source}",
                    join_names(&running)
                )
            };
            if can_retry {
                let description = if running.is_empty() {
                    "Copies the volume again and checks it.".to_string()
                } else {
                    format!(
                        "Copies the volume again. It may fail the same way while {} keeps writing to it; stopping {} first gives it the best chance.",
                        join_names(&running),
                        if running.len() == 1 { "it" } else { "them" }
                    )
                };
                p.actions.push(action(
                    "retry",
                    &retry_label,
                    &description,
                    true,
                    false,
                    None,
                ));
            }
            p.actions.push(move_elsewhere);
            p.actions.push(dismiss(
                &format!("Keep it on {from}"),
                "Clears this message and leaves the container where it is.",
                !can_retry,
            ));
        }
        Parsed::Refused { volume, why } => {
            p.kind = ProblemKind::SyncRefused;
            p.title = format!("The move to {to} was refused to protect your data");
            p.detail = format!(
                "{why}. Nothing was copied. {on_source} Check the volume before trying again."
            );
            p.volume = volume;
            p.actions.push(action(
                "view_volume",
                "Open the volume",
                "See which copy is current and whether it is empty.",
                true,
                false,
                None,
            ));
            if can_retry {
                p.actions.push(action(
                    "retry",
                    &retry_label,
                    "Only worth it once the volume has been checked; it will be refused again otherwise.",
                    false,
                    false,
                    None,
                ));
            }
            p.actions.push(dismiss(
                &format!("Keep it on {from}"),
                "Clears this message and leaves the container where it is.",
                false,
            ));
        }
        Parsed::Transfer(msg) => {
            p.kind = ProblemKind::SyncFailed;
            p.title = format!("Copying the volume to {to} failed");
            p.detail = format!("{msg}. Nothing was switched. {on_source}");
            if can_retry {
                p.actions.push(action(
                    "retry",
                    &retry_label,
                    "Starts the move from the beginning. Often enough if the node was briefly unreachable.",
                    true,
                    false,
                    None,
                ));
            }
            p.actions.push(move_elsewhere);
            p.actions.push(dismiss(
                &format!("Keep it on {from}"),
                "Clears this message and leaves the container where it is.",
                !can_retry,
            ));
        }
        Parsed::Stop => {
            p.kind = ProblemKind::StopFailed;
            p.title = format!("{name} wouldn't stop on {from}");
            p.detail = format!(
                "The move needs the container stopped so its data stops changing. Nothing was copied. {on_source}"
            );
            if can_retry {
                p.actions.push(action(
                    "retry",
                    &retry_label,
                    "Tries to stop it again, then copies the data.",
                    true,
                    false,
                    None,
                ));
            }
            p.actions.push(action(
                "view_logs",
                "Open the logs",
                "See why it is not shutting down.",
                false,
                false,
                None,
            ));
            p.actions.push(dismiss(
                &format!("Keep it on {from}"),
                "Clears this message and leaves the container where it is.",
                !can_retry,
            ));
        }
        Parsed::Start => {
            p.kind = ProblemKind::StartFailed;
            p.title = format!("{name} didn't start on {to}");
            p.detail = if switched {
                format!(
                    "The data was copied and checked, and {name} now belongs to {to}, but its container would not start there. It is not running on {from} anymore."
                )
            } else {
                format!("The container would not start on {to}.")
            };
            if switched {
                p.actions.push(action(
                    "restart",
                    &format!("Restart on {to}"),
                    "Tries to start the container on its new node again.",
                    true,
                    false,
                    None,
                ));
            }
            p.actions.push(action(
                "view_logs",
                "Open the logs",
                "See why it did not start.",
                !switched,
                false,
                None,
            ));
            p.actions.push(dismiss(
                "Clear this message",
                "Removes the failed-move record. The container stays where it is.",
                false,
            ));
        }
        Parsed::Other => {
            p.title = format!("The move to {to} failed");
            p.detail = format!(
                "Nothing was switched. {on_source} The technical details have the exact error."
            );
            if can_retry {
                p.actions.push(action(
                    "retry",
                    &retry_label,
                    "Starts the move again.",
                    true,
                    false,
                    None,
                ));
            }
            p.actions.push(move_elsewhere);
            p.actions.push(dismiss(
                &format!("Keep it on {from}"),
                "Clears this message and leaves the container where it is.",
                !can_retry,
            ));
        }
    }
    p
}

/// One line for lists, the cli and the MCP report.
pub fn one_line(p: &MigrationProblem) -> String {
    let running: Vec<&str> = p
        .co_writers
        .iter()
        .filter(|c| c.running)
        .map(|c| c.instance.as_str())
        .collect();
    if p.kind == ProblemKind::VerifyMismatch && !running.is_empty() {
        format!(
            "{} (likely because {} kept writing to the same volume)",
            p.title,
            running.join(", ")
        )
    } else {
        p.title.clone()
    }
}

/// The same line straight from raft state, for callers without a summary.
pub fn one_line_for(state: &ClusterState, instance: &InstanceId) -> Option<String> {
    classify(state, instance).map(|p| one_line(&p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_drive::DriveError;
    use cl_raft::state::{Assignment, StackState};
    use cl_raft::VolumeId;

    fn state_with(
        phase: MigrationPhase,
        assigned: &str,
        spec: &str,
        others: &[(&str, &str)],
    ) -> (ClusterState, InstanceId) {
        let mut state = ClusterState::default();
        let id = InstanceId::new("nc", "app", 0);
        state.stacks.insert(
            "nc".into(),
            StackState {
                name: "nc".into(),
                spec: spec.into(),
                revision: 1,
                configs: Default::default(),
                secrets: Default::default(),
                used_configs: Default::default(),
                used_secrets: Default::default(),
                deployed_at: 0,
                previous_spec: None,
            },
        );
        state.assignments.insert(
            id.clone(),
            Assignment {
                instance: id.clone(),
                node: assigned.into(),
                desired: DesiredState::Running,
                image_digest: None,
                migration: Some(Migration {
                    from: "atlas-01".into(),
                    to: "atlas-02".into(),
                    phase,
                }),
            },
        );
        for (service, node) in others {
            let other = InstanceId::new("nc", *service, 0);
            state.assignments.insert(
                other.clone(),
                Assignment {
                    instance: other,
                    node: (*node).into(),
                    desired: DesiredState::Running,
                    image_digest: None,
                    migration: None,
                },
            );
        }
        state.nodes.insert(
            "atlas-02".into(),
            cl_raft::state::NodeState {
                name: "atlas-02".into(),
                id: Default::default(),
                role: cl_raft::state::Role::Manager,
                rpc_addr: "10.0.0.2:7373".parse().unwrap(),
                drives: Default::default(),
                availability: cl_raft::state::Availability::Active,
                labels: Default::default(),
                last_resort: false,
                egress_networks: Default::default(),
                networks: Default::default(),
                open_ports: Default::default(),
                arch: Default::default(),
                binary_hash: Default::default(),
                node_pubkey: Default::default(),
                memory_total_bytes: Default::default(),
                version: Default::default(),
            },
        );
        (state, id)
    }

    const SPEC: &str = "services:\n  app:\n    image: a\n    volumes:\n      - veryfast/html:/var/www/html\n  push:\n    image: b\n    volumes:\n      - veryfast/html:/var/www/html\n  other:\n    image: c\n    volumes:\n      - veryfast/else:/data\n";

    fn failed(err: DriveError) -> MigrationPhase {
        MigrationPhase::Failed {
            reason: err.to_string(),
        }
    }

    fn verify_error() -> DriveError {
        DriveError::Verify {
            message: format!(
                "{}: target differs after sync (added={:?} changed={:?} removed={:?})",
                VolumeId::new("veryfast", "html"),
                Vec::<String>::new(),
                vec!["apps/a \"q\".json", "custom/b.js"],
                vec!["gone.txt"]
            ),
        }
    }

    #[test]
    fn verify_mismatch_parses_paths_and_names_co_writers() {
        let (state, id) = state_with(
            failed(verify_error()),
            "atlas-01",
            SPEC,
            &[("push", "atlas-01"), ("other", "atlas-01")],
        );
        let p = classify(&state, &id).unwrap();
        assert_eq!(p.kind, ProblemKind::VerifyMismatch);
        assert_eq!(p.volume.as_deref(), Some("veryfast/html"));
        assert_eq!(p.changed, vec!["apps/a \"q\".json", "custom/b.js"]);
        assert_eq!(p.removed, vec!["gone.txt"]);
        assert!(p.added.is_empty());
        assert_eq!(p.files_total, 3);
        assert_eq!(p.co_writers.len(), 1);
        assert_eq!(p.co_writers[0].instance, "nc.push.0");
        assert!(p.co_writers[0].running);
        assert!(p.detail.contains("push.0"), "{}", p.detail);
        assert!(p.title.contains("atlas-02"));
        let ids: Vec<_> = p.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["retry", "move_elsewhere", "dismiss"]);
        assert_eq!(p.actions.iter().filter(|a| a.primary).count(), 1);
        assert!(p.actions[0].description.contains("may fail"));
    }

    #[test]
    fn verify_without_co_writers_and_with_capped_lists() {
        let many: Vec<String> = (0..50).map(|i| format!("f{i}")).collect();
        let err = DriveError::Verify {
            message: format!(
                "veryfast/html: target differs after sync (added={:?} changed={:?} removed={:?})",
                many,
                Vec::<String>::new(),
                Vec::<String>::new()
            ),
        };
        let (state, id) = state_with(
            failed(err),
            "atlas-01",
            "services:\n  app:\n    image: a\n    volumes:\n      - veryfast/html:/x\n",
            &[],
        );
        let p = classify(&state, &id).unwrap();
        assert!(p.co_writers.is_empty());
        assert_eq!(p.added.len(), MAX_FILES);
        assert_eq!(p.files_total, 50);
        assert!(!p.actions[0].description.contains("may fail"));
    }

    #[test]
    fn refused_transfer_stop_and_start_are_told_apart() {
        let cases = [
            (
                DriveError::DestructiveSync {
                    volume: VolumeId::new("veryfast", "html"),
                    reason: "the source is empty".into(),
                },
                ProblemKind::SyncRefused,
            ),
            (
                DriveError::Transfer {
                    message: "connect: refused".into(),
                },
                ProblemKind::SyncFailed,
            ),
            (
                DriveError::VolumeNotFound {
                    volume: VolumeId::new("veryfast", "html"),
                },
                ProblemKind::SyncFailed,
            ),
            (
                DriveError::Transfer {
                    message: "stopping nc.app.0: timeout".into(),
                },
                ProblemKind::StopFailed,
            ),
            (DriveError::Config("nope".into()), ProblemKind::Other),
        ];
        for (err, kind) in cases {
            let (state, id) = state_with(failed(err), "atlas-01", SPEC, &[]);
            let p = classify(&state, &id).unwrap();
            assert_eq!(p.kind, kind, "{}", p.raw);
            assert!(p.actions.iter().filter(|a| a.primary).count() <= 1);
        }
        let (state, id) = state_with(
            failed(DriveError::DestructiveSync {
                volume: VolumeId::new("veryfast", "html"),
                reason: "empty".into(),
            }),
            "atlas-01",
            SPEC,
            &[],
        );
        let p = classify(&state, &id).unwrap();
        assert_eq!(p.volume.as_deref(), Some("veryfast/html"));
        assert_eq!(p.actions[0].id, "view_volume");
    }

    #[test]
    fn failure_after_the_switch_is_a_start_failure_on_the_target() {
        let (state, id) = state_with(
            failed(DriveError::Transfer {
                message: "starting nc.app.0: boom".into(),
            }),
            "atlas-02",
            SPEC,
            &[],
        );
        let p = classify(&state, &id).unwrap();
        assert_eq!(p.kind, ProblemKind::StartFailed);
        let ids: Vec<_> = p.actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["restart", "view_logs", "dismiss"]);
        assert!(p.detail.contains("belongs to atlas-02"));
    }

    #[test]
    fn retry_is_withheld_when_the_target_node_is_gone() {
        let (mut state, id) = state_with(failed(verify_error()), "atlas-01", SPEC, &[]);
        state.nodes.clear();
        let p = classify(&state, &id).unwrap();
        assert!(p.actions.iter().all(|a| a.id != "retry"));
        assert_eq!(p.actions.iter().filter(|a| a.primary).count(), 1);
    }

    #[test]
    fn nothing_for_an_active_or_absent_move() {
        let (state, id) = state_with(MigrationPhase::Syncing, "atlas-01", SPEC, &[]);
        assert!(classify(&state, &id).is_none());
    }

    #[test]
    fn debug_strings_round_trip() {
        let original = vec!["a\"b", "c\\d", "é\n", "tab\t"];
        let rendered = format!("{original:?}");
        let text = format!("x={rendered}");
        let mut lists = Lists::new(&text);
        assert_eq!(lists.list("x="), original);
    }
}
