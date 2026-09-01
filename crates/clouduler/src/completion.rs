//! Dynamic shell completion.
//!
//! Static clap completion only ever knows about subcommand and flag names -
//! it cannot tell a user which stacks or nodes actually exist. This module
//! plugs into clap_complete's `unstable-dynamic` engine (see
//! `clap_complete::engine::ArgValueCompleter` on the relevant fields in
//! `cli.rs`) to answer those with live data instead: node and drive names
//! come straight from the config file (cheap, local, and the same source
//! every other command already trusts for those), while stack names are
//! asked of a live manager, since stacks only exist in the replicated raft
//! state and not in the config at all.
//!
//! Every completer here is best-effort: on any failure (no config, no
//! reachable manager, a slow network) it silently returns no candidates
//! rather than erroring or hanging the shell. A completer also runs
//! synchronously from clap_complete's callback, so a stack-name lookup
//! spins up its own throwaway single-threaded runtime on a dedicated
//! thread rather than reusing `main`'s, and bounds it with a short timeout.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use clap_complete::engine::CompletionCandidate;
use cl_config::ClusterConfig;

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

/// Complete a stack name by asking a live manager for the current stack
/// list, the same call `clouduler stack list` makes.
pub fn stack_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(cfg) = load_config() else {
        return Vec::new();
    };

    let names = std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return Vec::new();
        };
        rt.block_on(async {
            let Ok(client) = stack::client_for(&cfg, None) else {
                return Vec::new();
            };
            match tokio::time::timeout(QUERY_TIMEOUT, client.state()).await {
                Ok(Ok(state)) => state.stacks.into_keys().collect::<Vec<_>>(),
                _ => Vec::new(),
            }
        })
    })
    .join()
    .unwrap_or_default();

    names
        .into_iter()
        .filter(|n| n.starts_with(current))
        .map(CompletionCandidate::new)
        .collect()
}

/// Complete a node name from the config file. Nodes are declared there
/// before they can be reached at all, so this needs no network round trip.
pub fn node_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Some(cfg) = load_config() else {
        return Vec::new();
    };

    cfg.nodes
        .keys()
        .filter(|n| n.starts_with(current))
        .map(|n| CompletionCandidate::new(n.clone()))
        .collect()
}

// No CLI argument takes a bare drive name today (`--drive` is a repeatable
// `NAME=PATH` pair, and `folder-import`'s target is `<drive>/<volume>[/path]`
// packed into one string), so there is nothing to hang a drive-name
// completer off yet. `node_names` and `stack_names` above show the pattern
// to follow - reading `cfg.drives` - if one is added.
