//! Dynamic shell completion.
//!
//! Static clap completion only ever knows about subcommand and flag names -
//! it cannot tell a user which stacks, nodes, drives or running instances
//! actually exist. This module plugs into clap_complete's `unstable-dynamic`
//! engine (see `clap_complete::engine::ArgValueCompleter` on the relevant
//! fields in `cli.rs`) to answer those with live data instead: node and
//! drive names come straight from the config file (cheap, local, and the
//! same source every other command already trusts for those), while
//! anything that only exists in the replicated raft state - stacks,
//! instances, volumes, backup destinations/targets - is asked of a live
//! manager.
//!
//! Every completer here is best-effort: on any failure (no config, no
//! reachable manager, a slow network) it silently returns no candidates
//! rather than erroring or hanging the shell. A completer also runs
//! synchronously from clap_complete's callback, so a state lookup spins up
//! its own throwaway single-threaded runtime on a dedicated thread rather
//! than reusing `main`'s, and bounds it with a short timeout.
//!
//! No API key or special endpoint is needed for any of this: `cluster.yaml`
//! is already group-readable by the `clouduler` group members (see
//! `init::permissions`), and the raft rpc channel a completer's throwaway
//! runtime talks to is the same unauthenticated node-to-node channel any
//! cli invocation already uses - a shell able to read the config and reach
//! the internal network can already do everything `stack list` can.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use clap_complete::engine::CompletionCandidate;
use cl_config::ClusterConfig;
use cl_raft::state::ClusterState;

use crate::stack;

/// A short timeout so a hung or unreachable manager doesn't leave the shell
/// waiting on tab completion; better to complete nothing than to hang.
const QUERY_TIMEOUT: Duration = Duration::from_millis(400);

fn config_path() -> PathBuf {
    std::env::var_os("CLOUDULER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/clouduler/cluster.yaml"))
}

fn load_config() -> Option<ClusterConfig> {
    ClusterConfig::load(config_path()).ok()
}

/// Fetch the live cluster state from any manager in the config, bounded by
/// `QUERY_TIMEOUT` and run on its own throwaway runtime since a completer
/// runs synchronously from clap_complete's callback.
fn live_state() -> Option<ClusterState> {
    let cfg = load_config()?;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        rt.block_on(async {
            let client = stack::client_for(&cfg, None).ok()?;
            tokio::time::timeout(QUERY_TIMEOUT, client.state()).await.ok()?.ok()
        })
    })
    .join()
    .ok()
    .flatten()
}

fn help(text: impl Into<clap::builder::StyledStr>) -> Option<clap::builder::StyledStr> {
    Some(text.into())
}

fn instances_in_stack(state: &ClusterState, name: &str) -> usize {
    state.assignments.keys().filter(|id| id.stack == name).count()
}

fn drive_mode_label(mode: &cl_config::DriveMode) -> &'static str {
    match mode {
        cl_config::DriveMode::Replicated => "replicated",
        cl_config::DriveMode::Served => "served",
    }
}

/// Complete a stack name by asking a live manager for the current stack
/// list, the same call `clouduler stack list` makes. Each candidate is
/// annotated with how many instances it currently has.
pub fn stack_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(state) = live_state() else {
        return Vec::new();
    };

    state
        .stacks
        .keys()
        .filter(|n| n.starts_with(current))
        .map(|name| {
            let count = instances_in_stack(&state, name);
            let noun = if count == 1 { "instance" } else { "instances" };
            CompletionCandidate::new(name.clone()).help(help(format!("stack, {count} {noun}")))
        })
        .collect()
}

/// Complete either a stack name (streams/targets every instance) or a live
/// instance id (`stack.service.index`), for arguments that accept both -
/// `logs`, `exec`, `capture`.
pub fn targets(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(state) = live_state() else {
        return Vec::new();
    };

    let mut out: Vec<CompletionCandidate> = state
        .stacks
        .keys()
        .filter(|n| n.starts_with(current))
        .map(|name| {
            let count = instances_in_stack(&state, name);
            let noun = if count == 1 { "instance" } else { "instances" };
            CompletionCandidate::new(name.clone()).help(help(format!("stack, {count} {noun}")))
        })
        .collect();

    for (id, assignment) in &state.assignments {
        let id = id.to_string();
        if !id.starts_with(current) {
            continue;
        }
        let running = state.container_status.get(&assignment.instance).map(|c| c.running);
        let status = match running {
            Some(true) => "running",
            Some(false) => "stopped",
            None => "unreported",
        };
        out.push(CompletionCandidate::new(id).help(help(format!("on {}, {status}", assignment.node))));
    }

    out
}

/// Complete a node name from the config file. Nodes are declared there
/// before they can be reached at all, so this needs no network round trip.
/// Annotated with role and internal address.
pub fn node_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(cfg) = load_config() else {
        return Vec::new();
    };

    cfg.nodes
        .iter()
        .filter(|(n, _)| n.starts_with(current))
        .map(|(name, node)| {
            let addr = cfg
                .rpc_addr(node)
                .map(|a| a.to_string())
                .unwrap_or_else(|| "no internal address".to_string());
            CompletionCandidate::new(name.clone()).help(help(format!("{}, {addr}", node.role)))
        })
        .collect()
}

/// Complete a drive name from the config file.
pub fn drive_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(cfg) = load_config() else {
        return Vec::new();
    };

    cfg.drives
        .iter()
        .filter(|(n, _)| n.starts_with(current))
        .map(|(name, drive)| {
            CompletionCandidate::new(name.clone()).help(help(format!("{} drive", drive_mode_label(&drive.mode))))
        })
        .collect()
}

/// Complete a `drive/volume` id from live volume state, e.g. for
/// `backup restore-volume`.
pub fn volume_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(state) = live_state() else {
        return Vec::new();
    };

    state
        .volumes
        .values()
        .filter(|v| v.id.to_string().starts_with(current))
        .map(|v| CompletionCandidate::new(v.id.to_string()).help(help(format!("primary on {}", v.primary))))
        .collect()
}

/// Complete a service name seen anywhere in the live assignments. Not
/// scoped to a particular stack - clap_complete's dynamic completers only
/// see the value being typed, not sibling arguments already parsed - so
/// this is best-effort across the whole cluster rather than per-stack.
pub fn service_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(state) = live_state() else {
        return Vec::new();
    };

    let mut seen = std::collections::BTreeSet::new();
    for id in state.assignments.keys() {
        if id.service.starts_with(current) {
            seen.insert(id.service.clone());
        }
    }
    seen.into_iter().map(CompletionCandidate::new).collect()
}

/// Complete a backup destination id from live state.
pub fn backup_destination_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(state) = live_state() else {
        return Vec::new();
    };

    state
        .restic_destinations
        .values()
        .filter(|d| d.id.starts_with(current))
        .map(|d| CompletionCandidate::new(d.id.clone()).help(help(d.name.clone())))
        .collect()
}

/// Complete a backup target id from live state.
pub fn backup_target_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(state) = live_state() else {
        return Vec::new();
    };

    state
        .restic_targets
        .values()
        .filter(|t| t.id.starts_with(current))
        .map(|t| CompletionCandidate::new(t.id.clone()).help(help(format!("{} -> {}", t.drive, t.destination_id))))
        .collect()
}

/// Complete `clouduler folder-import`'s packed `<drive>/<volume>[/path]`
/// target: drive names with no slash typed yet, then known volumes under
/// the drive once one is chosen. A path segment past the volume is left
/// uncompleted, since that is a path inside the volume rather than
/// anything this process can list.
pub fn folder_import_target(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };

    match current.split_once('/') {
        None => drive_names(OsStr::new(current))
            .into_iter()
            .map(|c| CompletionCandidate::new(format!("{}/", c.get_value().to_string_lossy())).help(c.get_help().cloned()))
            .collect(),
        Some((drive, rest)) if !rest.contains('/') => {
            let Some(state) = live_state() else {
                return Vec::new();
            };
            state
                .volumes
                .keys()
                .filter(|id| id.drive == drive && id.volume.starts_with(rest))
                .map(|id| CompletionCandidate::new(format!("{drive}/{}", id.volume)))
                .collect()
        }
        Some(_) => Vec::new(),
    }
}
