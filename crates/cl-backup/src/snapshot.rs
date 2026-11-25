//! What changed between two exports, for a descriptive commit message.
//!
//! Only the four mirrored fields (`cluster_config`, `stacks`, `configs`,
//! `secrets`) are diffed: the rest of `ClusterState` churns on unrelated
//! activity and would make noisy commits.
//!
//! `configs` excludes this feature's reserved bookkeeping entries. Publishing
//! an export's status bumps `LAST_EXPORT_CONFIG` right after the export, so
//! including it would make the next snapshot never equal `last_snapshot` and
//! force a new commit on every poll.

use std::collections::BTreeMap;

use cl_raft::state::ClusterState;
use serde::{Deserialize, Serialize};

use crate::deploy_key::DEPLOY_KEY_PUBLIC_CONFIG;
use crate::status::{LAST_EXPORT_CONFIG, LAST_PULL_CONFIG};

const RUN_CONFIG: &str = "_clouduler_self_update_run";

/// Reserved config entries this feature writes for bookkeeping, not user
/// content: excluded from change detection, from `tree::build`, and from the
/// reverse sync in `cl-api::routes::git_backup`.
pub fn is_bookkeeping_config(name: &str) -> bool {
    name == LAST_EXPORT_CONFIG
        || name == DEPLOY_KEY_PUBLIC_CONFIG
        || name == LAST_PULL_CONFIG
        || name == MCP_CLIENTS_CONFIG
        || name == UPDATE_CHECK_CONFIG
        || name == RUN_CONFIG
}

/// The self-update release check (`cl_update::plan::CHECK_CONFIG`), rewritten
/// on every periodic check; it is observation, not configuration, so it stays
/// out of git.
const UPDATE_CHECK_CONFIG: &str = "_clouduler_self_update_check";

/// The MCP oauth client store (`cl-api::auth::oauth::MCP_CLIENTS_CONFIG`).
/// Kept out of git because every token refresh rewrites it, which would turn
/// the export into a commit per client per hour; clients simply re-approve
/// after a restore.
const MCP_CLIENTS_CONFIG: &str = "_clouduler_mcp_clients";

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub cluster_config_generation: Option<u64>,
    pub stacks: BTreeMap<String, u64>,
    pub configs: BTreeMap<String, u64>,
    pub secrets: BTreeMap<String, u64>,
    /// `"stack/service"` -> module revision.
    #[serde(default)]
    pub terraform: BTreeMap<String, u64>,
    /// `"stack/service"` -> state serial.
    #[serde(default)]
    pub terraform_states: BTreeMap<String, u64>,
    /// The last successful self-update (`.clouduler-version.json`), so finishing one makes the
    /// next export commit even when nothing else changed.
    #[serde(default)]
    pub upgrade: Option<crate::upgrade::UpgradeRecord>,
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
            terraform: state
                .terraform_modules
                .iter()
                .flat_map(|(stack, mods)| {
                    mods.iter().map(move |(service, m)| {
                        (
                            cl_raft::terraform::terraform_key(stack, service),
                            m.revision,
                        )
                    })
                })
                .collect(),
            terraform_states: state
                .terraform_states
                .iter()
                .map(|(key, st)| (key.clone(), st.serial))
                .collect(),
            upgrade: crate::upgrade::successful_upgrade(state),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Upgraded(String),
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
    TerraformAdded(String),
    TerraformUpdated(String),
    TerraformRemoved(String),
    TerraformStateUpdated(String),
}

/// Diffs `old` against `new`. Empty means nothing this feature cares about
/// changed (e.g. only scheduling activity happened).
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut changes = Vec::new();

    if let Some(record) = &new.upgrade {
        if old.upgrade.as_ref() != Some(record) {
            changes.push(Change::Upgraded(record.version.clone()));
        }
    }

    if old.cluster_config_generation != new.cluster_config_generation {
        changes.push(Change::ClusterConfigChanged);
    }

    diff_map(
        &old.stacks,
        &new.stacks,
        &mut changes,
        Change::StackAdded,
        Change::StackUpdated,
        Change::StackRemoved,
    );
    diff_map(
        &old.configs,
        &new.configs,
        &mut changes,
        Change::ConfigAdded,
        Change::ConfigUpdated,
        Change::ConfigRemoved,
    );
    diff_map(
        &old.secrets,
        &new.secrets,
        &mut changes,
        Change::SecretAdded,
        Change::SecretUpdated,
        Change::SecretRemoved,
    );
    diff_map(
        &old.terraform,
        &new.terraform,
        &mut changes,
        Change::TerraformAdded,
        Change::TerraformUpdated,
        Change::TerraformRemoved,
    );
    // A state appearing, moving or vanishing changes its .tfstate.age.
    for key in old
        .terraform_states
        .keys()
        .chain(new.terraform_states.keys())
    {
        if old.terraform_states.get(key) != new.terraform_states.get(key)
            && !changes.contains(&Change::TerraformStateUpdated(key.clone()))
        {
            changes.push(Change::TerraformStateUpdated(key.clone()));
        }
    }

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
        Change::Upgraded(version) => format!("Upgrade Clouduler to {version}"),
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
        Change::TerraformAdded(key) => format!("add terraform module {key}"),
        Change::TerraformUpdated(key) => format!("update terraform module {key}"),
        Change::TerraformRemoved(key) => format!("remove terraform module {key}"),
        Change::TerraformStateUpdated(key) => format!("update terraform state {key}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_new_upgrade_is_a_change_with_its_own_commit_message() {
        let old = Snapshot::default();
        let new = Snapshot {
            upgrade: Some(crate::upgrade::UpgradeRecord {
                version: "v0.18.0".to_string(),
                upgraded_at: 5,
            }),
            ..Default::default()
        };
        let changes = diff(&old, &new);
        assert_eq!(changes, vec![Change::Upgraded("v0.18.0".to_string())]);
        assert_eq!(commit_message(&changes), "Upgrade Clouduler to v0.18.0");
        // Unchanged record: nothing; record vanishing (emergency): nothing either.
        assert!(diff(&new, &new).is_empty());
        assert!(diff(&new, &old).is_empty());
    }

    #[test]
    fn the_update_check_is_bookkeeping_not_exported_content() {
        assert!(super::is_bookkeeping_config("_clouduler_self_update_check"));
        assert!(super::is_bookkeeping_config("_clouduler_self_update_run"));
    }

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

    /// Publishing an export's status bumps `LAST_EXPORT_CONFIG`'s revision,
    /// and that bump must not look like a tracked change on the next tick, or
    /// the daemon exports forever even with nothing else changed.
    #[test]
    fn publishing_last_export_status_does_not_look_like_a_change() {
        let mut state = ClusterState::default();
        state.configs.insert(
            LAST_EXPORT_CONFIG.to_string(),
            config_entry(LAST_EXPORT_CONFIG, 1),
        );
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

        state.configs.insert(
            DEPLOY_KEY_PUBLIC_CONFIG.to_string(),
            config_entry(DEPLOY_KEY_PUBLIC_CONFIG, 1),
        );
        let after = Snapshot::from_state(&state);

        assert_eq!(before, after);
    }

    fn snap(stacks: &[(&str, u64)], configs: &[(&str, u64)], secrets: &[(&str, u64)]) -> Snapshot {
        Snapshot {
            cluster_config_generation: Some(1),
            stacks: stacks.iter().map(|(n, r)| (n.to_string(), *r)).collect(),
            configs: configs.iter().map(|(n, r)| (n.to_string(), *r)).collect(),
            secrets: secrets.iter().map(|(n, r)| (n.to_string(), *r)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn terraform_module_and_state_changes_are_detected() {
        let a = snap(&[], &[], &[]);
        let mut b = a.clone();
        b.terraform.insert("s/w".into(), 1);
        assert_eq!(diff(&a, &b), vec![Change::TerraformAdded("s/w".into())]);
        let mut c = b.clone();
        c.terraform_states.insert("s/w".into(), 1);
        assert_eq!(
            diff(&b, &c),
            vec![Change::TerraformStateUpdated("s/w".into())]
        );
        let mut d = c.clone();
        d.terraform_states.insert("s/w".into(), 2);
        assert_eq!(
            diff(&c, &d),
            vec![Change::TerraformStateUpdated("s/w".into())]
        );
        assert!(commit_message(&diff(&c, &d)).contains("terraform state"));
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
        let old = snap(
            &[("web", 1), ("db", 1)],
            &[("nginx.conf", 1)],
            &[("db-password", 1)],
        );
        let new = snap(
            &[("web", 2), ("api", 1)],
            &[],
            &[("db-password", 1), ("api-key", 1)],
        );

        let changes = diff(&old, &new);
        assert!(changes.contains(&Change::StackUpdated("web".into())));
        assert!(changes.contains(&Change::StackRemoved("db".into())));
        assert!(changes.contains(&Change::StackAdded("api".into())));
        assert!(changes.contains(&Change::ConfigRemoved("nginx.conf".into())));
        assert!(changes.contains(&Change::SecretAdded("api-key".into())));
        assert!(!changes
            .iter()
            .any(|c| matches!(c, Change::SecretUpdated(_))));
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
            &[
                ("a", 1),
                ("b", 1),
                ("c", 1),
                ("d", 1),
                ("e", 1),
                ("f", 1),
                ("g", 1),
            ],
            &[],
            &[],
        );
        let msg = commit_message(&diff(&old, &new));
        assert!(msg.ends_with("and 1 more change(s)"));
    }
}
