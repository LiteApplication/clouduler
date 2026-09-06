//! What changed between two exports, for a descriptive commit message.
//!
//! Deliberately narrow: only the four fields the git backup mirrors
//! (`cluster_config`, `stacks`, `configs`, `secrets`). `ClusterState` also
//! churns on scheduling/DNS/certificate activity that has nothing to do with
//! this feature, so diffing the whole state would produce noisy commits and
//! misleading messages.
//!
//! `configs` also excludes this feature's own reserved bookkeeping entries
//! (`LAST_EXPORT_CONFIG`, `DEPLOY_KEY_PUBLIC_CONFIG`). Publishing an export's
//! status bumps `LAST_EXPORT_CONFIG`'s revision in raft state right after
//! that export runs — including it here would mean the snapshot taken on the
//! *next* tick never equals `last_snapshot` (which predates that bump),
//! forcing a fresh export forever, each one re-encrypting every secret with
//! a new (age is non-deterministic) ciphertext and producing a real commit
//! every `backup.interval` even when nothing a human changed.

use std::collections::BTreeMap;

use cl_raft::state::ClusterState;
use serde::{Deserialize, Serialize};

use crate::deploy_key::DEPLOY_KEY_PUBLIC_CONFIG;
use crate::status::{LAST_EXPORT_CONFIG, LAST_PULL_CONFIG};

/// Reserved config entries this feature writes for its own bookkeeping, not
/// user content - excluded both from change detection (this module) and
/// from the exported tree itself (`tree::build`), and from the reverse
/// (git -> cluster) sync in `cl-api::routes::git_backup` since they are never
/// mirrored into the repo in the first place.
pub fn is_bookkeeping_config(name: &str) -> bool {
    name == LAST_EXPORT_CONFIG || name == DEPLOY_KEY_PUBLIC_CONFIG || name == LAST_PULL_CONFIG
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub cluster_config_generation: Option<u64>,
    pub stacks: BTreeMap<String, u64>,
    pub configs: BTreeMap<String, u64>,
    pub secrets: BTreeMap<String, u64>,
}

impl Snapshot {
    pub fn from_state(state: &ClusterState) -> Self {
        Self {
            cluster_config_generation: state.cluster_config.as_ref().map(|c| c.generation),
            stacks: state
                .stacks
                .iter()
                .map(|(name, s)| (name.clone(), s.revision))
                .collect(),
            configs: state
                .configs
                .iter()
                .filter(|(name, _)| !is_bookkeeping_config(name))
                .map(|(name, c)| (name.clone(), c.revision))
                .collect(),
            secrets: state
                .secrets
                .iter()
                .map(|(name, s)| (name.clone(), s.revision))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    ClusterConfigChanged,
    StackAdded(String),
    StackUpdated(String),
    StackRemoved(String),
    ConfigAdded(String),
    ConfigUpdated(String),
    ConfigRemoved(String),
    SecretAdded(String),
    SecretUpdated(String),
    SecretRemoved(String),
}

/// Diffs `old` against `new`. Empty means nothing this feature cares about
/// changed (e.g. only scheduling activity happened).
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut changes = Vec::new();

    if old.cluster_config_generation != new.cluster_config_generation {
        changes.push(Change::ClusterConfigChanged);
    }

    diff_map(&old.stacks, &new.stacks, &mut changes, Change::StackAdded, Change::StackUpdated, Change::StackRemoved);
    diff_map(&old.configs, &new.configs, &mut changes, Change::ConfigAdded, Change::ConfigUpdated, Change::ConfigRemoved);
    diff_map(&old.secrets, &new.secrets, &mut changes, Change::SecretAdded, Change::SecretUpdated, Change::SecretRemoved);

    changes
}

fn diff_map(
    old: &BTreeMap<String, u64>,
    new: &BTreeMap<String, u64>,
    changes: &mut Vec<Change>,
    added: fn(String) -> Change,
    updated: fn(String) -> Change,
    removed: fn(String) -> Change,
) {
    for (name, new_rev) in new {
        match old.get(name) {
            None => changes.push(added(name.clone())),
            Some(old_rev) if old_rev != new_rev => changes.push(updated(name.clone())),
            Some(_) => {}
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            changes.push(removed(name.clone()));
        }
    }
}

/// Renders changes into a one-line, git-log-friendly summary, e.g.
/// "update stack web, add secret db-password, remove config nginx.conf".
/// Caps the number of items spelled out so a bulk change doesn't produce an
/// unreadable subject line.
pub fn commit_message(changes: &[Change]) -> String {
    const MAX_ITEMS: usize = 6;

    if changes.is_empty() {
        return "no tracked changes".to_string();
    }

    let mut parts: Vec<String> = changes.iter().take(MAX_ITEMS).map(describe).collect();
    if changes.len() > MAX_ITEMS {
        parts.push(format!("and {} more change(s)", changes.len() - MAX_ITEMS));
    }
    parts.join(", ")
}

fn describe(change: &Change) -> String {
    match change {
        Change::ClusterConfigChanged => "update cluster.yaml".to_string(),
        Change::StackAdded(name) => format!("add stack {name}"),
        Change::StackUpdated(name) => format!("update stack {name}"),
        Change::StackRemoved(name) => format!("remove stack {name}"),
        Change::ConfigAdded(name) => format!("add config {name}"),
        Change::ConfigUpdated(name) => format!("update config {name}"),
        Change::ConfigRemoved(name) => format!("remove config {name}"),
        Change::SecretAdded(name) => format!("add secret {name}"),
        Change::SecretUpdated(name) => format!("update secret {name}"),
        Change::SecretRemoved(name) => format!("remove secret {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::ConfigEntry;

    fn config_entry(name: &str, revision: u64) -> ConfigEntry {
        ConfigEntry {
            name: name.to_string(),
            content: String::new(),
            revision,
            owner: None,
        }
    }

    /// The exact loop this feature hit in production: publishing an export's
    /// status bumps `LAST_EXPORT_CONFIG`'s revision, and that bump must not
    /// look like a tracked change on the next tick, or the daemon exports
    /// forever even with nothing else changed.
    #[test]
    fn publishing_last_export_status_does_not_look_like_a_change() {
        let mut state = ClusterState::default();
        state.configs.insert(LAST_EXPORT_CONFIG.to_string(), config_entry(LAST_EXPORT_CONFIG, 1));
        let before = Snapshot::from_state(&state);

        state.configs.get_mut(LAST_EXPORT_CONFIG).unwrap().revision = 2;
        let after = Snapshot::from_state(&state);

        assert_eq!(before, after);
        assert!(diff(&before, &after).is_empty());
    }

    #[test]
    fn publishing_the_deploy_public_key_does_not_look_like_a_change() {
        let mut state = ClusterState::default();
        let before = Snapshot::from_state(&state);

        state.configs.insert(DEPLOY_KEY_PUBLIC_CONFIG.to_string(), config_entry(DEPLOY_KEY_PUBLIC_CONFIG, 1));
        let after = Snapshot::from_state(&state);

        assert_eq!(before, after);
    }

    fn snap(stacks: &[(&str, u64)], configs: &[(&str, u64)], secrets: &[(&str, u64)]) -> Snapshot {
        Snapshot {
            cluster_config_generation: Some(1),
            stacks: stacks.iter().map(|(n, r)| (n.to_string(), *r)).collect(),
            configs: configs.iter().map(|(n, r)| (n.to_string(), *r)).collect(),
            secrets: secrets.iter().map(|(n, r)| (n.to_string(), *r)).collect(),
        }
    }

    #[test]
    fn no_op_produces_no_changes() {
        let a = snap(&[("web", 1)], &[], &[]);
        let b = a.clone();
        assert!(diff(&a, &b).is_empty());
        assert_eq!(commit_message(&diff(&a, &b)), "no tracked changes");
    }

    #[test]
    fn detects_add_update_remove_per_kind() {
        let old = snap(&[("web", 1), ("db", 1)], &[("nginx.conf", 1)], &[("db-password", 1)]);
        let new = snap(&[("web", 2), ("api", 1)], &[], &[("db-password", 1), ("api-key", 1)]);

        let changes = diff(&old, &new);
        assert!(changes.contains(&Change::StackUpdated("web".into())));
        assert!(changes.contains(&Change::StackRemoved("db".into())));
        assert!(changes.contains(&Change::StackAdded("api".into())));
        assert!(changes.contains(&Change::ConfigRemoved("nginx.conf".into())));
        assert!(changes.contains(&Change::SecretAdded("api-key".into())));
        assert!(!changes.iter().any(|c| matches!(c, Change::SecretUpdated(_))));
    }

    #[test]
    fn cluster_config_generation_change_is_detected() {
        let mut old = snap(&[], &[], &[]);
        old.cluster_config_generation = Some(1);
        let mut new = old.clone();
        new.cluster_config_generation = Some(2);
        assert_eq!(diff(&old, &new), vec![Change::ClusterConfigChanged]);
    }

    #[test]
    fn commit_message_caps_long_lists() {
        let old = snap(&[], &[], &[]);
        let new = snap(
            &[("a", 1), ("b", 1), ("c", 1), ("d", 1), ("e", 1), ("f", 1), ("g", 1)],
            &[],
            &[],
        );
        let msg = commit_message(&diff(&old, &new));
        assert!(msg.ends_with("and 1 more change(s)"));
    }
}
