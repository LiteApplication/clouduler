//! Building the git working tree from cluster state, and parsing one back.
//!
//! Layout mirrors `cluster.yaml`'s own structure (the user's explicit
//! preference over a single bundle file):
//!
//! ```text
//! cluster.yaml
//! stacks/<name>.yaml
//! configs/<name>
//! secrets/<name>.age
//! ```
//!
//! `build` is a full rebuild every tick, not an incremental patch — this
//! crate's four tracked fields are small, and a full rebuild means `git.rs`
//! never has to reason about stale files left over from a rename or delete.
//!
//! Secrets are the one exception: `age` encryption is non-deterministic (a
//! fresh random ephemeral key per call), so re-encrypting an unchanged
//! secret still produces different ciphertext bytes than last time, which
//! `git.rs`'s tree-hash comparison would see as a real change. `SecretCache`
//! remembers each secret's ciphertext alongside the raft revision it was
//! encrypted from, so a secret only gets re-encrypted (and only then shows
//! up in a commit) when its revision actually moves.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use cl_raft::state::ClusterState;
use cl_secret::{ClusterKey, Sealed};
use serde::{Deserialize, Serialize};

use crate::error::{BackupError, Result};
use crate::snapshot::is_bookkeeping_config;

pub struct TreeFile {
    /// Relative path, forward-slash separated.
    pub path: String,
    pub contents: Vec<u8>,
}

/// One secret's last-encrypted ciphertext, keyed by the raft revision it was
/// produced from — reused as-is while that revision hasn't moved, dropped
/// otherwise. Lives in the daemon's `watch_git_backup` loop, across ticks;
/// starting empty after a restart just means the next tick's export
/// re-encrypts everything once, same as today.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretCache(BTreeMap<String, (u64, Vec<u8>)>);

impl SecretCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Builds the full working-tree file set for the current state. A secret
/// this node's `ClusterKey` cannot open is skipped (logged, not fatal) —
/// one bad secret must not block every other change from being backed up.
/// `cache` is updated in place: a reused or freshly-encrypted secret's entry
/// moves forward, and secrets no longer in `state.secrets` are dropped so
/// the cache can't grow unbounded across renames/removals.
pub fn build(
    state: &ClusterState,
    key: &ClusterKey,
    recipient: &age::x25519::Recipient,
    cache: &mut SecretCache,
) -> Vec<TreeFile> {
    let mut files = Vec::new();

    if let Some(cfg) = &state.cluster_config {
        files.push(TreeFile {
            path: "cluster.yaml".to_string(),
            contents: cfg.yaml.clone().into_bytes(),
        });
    }

    for (name, stack) in &state.stacks {
        files.push(TreeFile {
            path: format!("stacks/{name}.yaml"),
            contents: stack.spec.clone().into_bytes(),
        });
    }

    for (name, entry) in &state.configs {
        if is_bookkeeping_config(name) {
            continue;
        }
        match entry.decode() {
            Ok(bytes) => files.push(TreeFile {
                path: format!("configs/{name}"),
                contents: bytes,
            }),
            Err(e) => {
                tracing::warn!(config = %name, error = %e, "git backup: skipping undecodable config")
            }
        }
    }

    for (name, entry) in &state.secrets {
        let ciphertext = match cache.0.get(name) {
            Some((revision, ciphertext)) if *revision == entry.revision => ciphertext.clone(),
            _ => {
                let sealed = Sealed::from_blob(entry.sealed.clone());
                let plaintext = match sealed.open(key, name) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(secret = %name, error = %e, "git backup: skipping undecryptable secret");
                        continue;
                    }
                };
                match crate::age::encrypt(recipient, &plaintext) {
                    Ok(ciphertext) => {
                        cache
                            .0
                            .insert(name.clone(), (entry.revision, ciphertext.clone()));
                        ciphertext
                    }
                    Err(e) => {
                        tracing::warn!(secret = %name, error = %e, "git backup: skipping secret that failed to re-encrypt");
                        continue;
                    }
                }
            }
        };
        files.push(TreeFile {
            path: format!("secrets/{name}.age"),
            contents: ciphertext,
        });
    }
    cache.0.retain(|name, _| state.secrets.contains_key(name));

    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// The reverse of `build`, read back off disk (an already-checked-out git
/// working tree). Used only by `clouduler restore`, which clones a real
/// repo — the hot-path pull reads directly from the odb via
/// `git::parse_tree` instead. Secrets are returned still age-encrypted —
/// decrypting them needs the admin's identity, which only the `restore`
/// CLI command holds.
pub struct ParsedTree {
    pub cluster_yaml: Option<String>,
    /// Stack name -> raw spec yaml.
    pub stacks: BTreeMap<String, String>,
    /// Config name -> raw content.
    pub configs: BTreeMap<String, Vec<u8>>,
    /// Secret name -> age ciphertext.
    pub secrets: BTreeMap<String, Vec<u8>>,
}

pub fn parse_dir(root: &Path) -> Result<ParsedTree> {
    let cluster_yaml = read_optional(&root.join("cluster.yaml"))?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());

    let mut stacks = BTreeMap::new();
    for (name, bytes) in list_dir(&root.join("stacks"))? {
        let Some(name) = name.strip_suffix(".yaml") else {
            continue;
        };
        stacks.insert(
            name.to_string(),
            String::from_utf8_lossy(&bytes).into_owned(),
        );
    }

    let mut configs = BTreeMap::new();
    for (name, bytes) in list_dir(&root.join("configs"))? {
        configs.insert(name, bytes);
    }

    let mut secrets = BTreeMap::new();
    for (name, bytes) in list_dir(&root.join("secrets"))? {
        let Some(name) = name.strip_suffix(".age") else {
            continue;
        };
        secrets.insert(name.to_string(), bytes);
    }

    Ok(ParsedTree {
        cluster_yaml,
        stacks,
        configs,
        secrets,
    })
}

/// The base64 a `ConfigEntry`/state-machine json expects.
pub fn encode_config_content(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(BackupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn list_dir(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(BackupError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| BackupError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let bytes = std::fs::read(&path).map_err(|source| BackupError::Io {
            path: path.clone(),
            source,
        })?;
        out.push((name.to_string(), bytes));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{ClusterConfigEntry, ConfigEntry, SecretEntry, StackState};

    fn state_with_everything(key: &ClusterKey) -> ClusterState {
        let mut state = ClusterState {
            cluster_config: Some(ClusterConfigEntry {
                yaml: "nodes: []\n".to_string(),
                generation: 1,
            }),
            ..Default::default()
        };
        state.stacks.insert(
            "web".to_string(),
            StackState {
                name: "web".to_string(),
                spec: "services: {}\n".to_string(),
                revision: 1,
                configs: Default::default(),
                secrets: Default::default(),
                used_configs: Default::default(),
                used_secrets: Default::default(),
            },
        );
        state.configs.insert(
            "nginx.conf".to_string(),
            ConfigEntry {
                name: "nginx.conf".to_string(),
                content: encode_config_content(b"server {}"),
                revision: 1,
                owner: None,
            },
        );
        let sealed = Sealed::seal(key, "db-password", b"hunter2").expect("seal");
        state.secrets.insert(
            "db-password".to_string(),
            SecretEntry {
                name: "db-password".to_string(),
                sealed: sealed.blob,
                revision: 1,
                key_fingerprint: key.fingerprint(),
                owner: None,
            },
        );
        state
    }

    #[test]
    fn builds_one_file_per_tracked_object() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let state = state_with_everything(&key);

        let files = build(&state, &key, &recipient, &mut SecretCache::new());
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "cluster.yaml",
                "configs/nginx.conf",
                "secrets/db-password.age",
                "stacks/web.yaml"
            ]
        );
    }

    /// The reserved bookkeeping configs must never show up in the exported
    /// tree - they're this feature's own status/key-publishing plumbing, not
    /// content the admin's repo should mirror.
    #[test]
    fn bookkeeping_configs_are_excluded_from_the_tree() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let mut state = state_with_everything(&key);
        state.configs.insert(
            crate::status::LAST_EXPORT_CONFIG.to_string(),
            ConfigEntry {
                name: crate::status::LAST_EXPORT_CONFIG.to_string(),
                content: encode_config_content(b"{}"),
                revision: 1,
                owner: None,
            },
        );
        state.configs.insert(
            crate::deploy_key::DEPLOY_KEY_PUBLIC_CONFIG.to_string(),
            ConfigEntry {
                name: crate::deploy_key::DEPLOY_KEY_PUBLIC_CONFIG.to_string(),
                content: encode_config_content(b"ssh-ed25519 AAAA"),
                revision: 1,
                owner: None,
            },
        );

        let files = build(&state, &key, &recipient, &mut SecretCache::new());
        assert!(!files
            .iter()
            .any(|f| f.path.contains("_clouduler_git_backup_last_export")));
        assert!(!files
            .iter()
            .any(|f| f.path.contains("_clouduler_git_deploy_key_pub")));
        assert!(files.iter().any(|f| f.path == "configs/nginx.conf"));
    }

    #[test]
    fn an_undecryptable_secret_is_skipped_not_fatal() {
        let key = ClusterKey::generate();
        let wrong_key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let state = state_with_everything(&wrong_key);

        let files = build(&state, &key, &recipient, &mut SecretCache::new());
        assert!(!files.iter().any(|f| f.path.starts_with("secrets/")));
        assert!(files.iter().any(|f| f.path == "cluster.yaml"));
    }

    #[test]
    fn round_trips_through_disk() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let state = state_with_everything(&key);
        let files = build(&state, &key, &recipient, &mut SecretCache::new());

        let dir = tempfile::tempdir().expect("tempdir");
        for f in &files {
            let path = dir.path().join(&f.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &f.contents).unwrap();
        }

        let parsed = parse_dir(dir.path()).expect("parse");
        assert_eq!(parsed.cluster_yaml.as_deref(), Some("nodes: []\n"));
        assert_eq!(
            parsed.stacks.get("web").map(String::as_str),
            Some("services: {}\n")
        );
        assert_eq!(
            parsed.configs.get("nginx.conf").map(Vec::as_slice),
            Some(b"server {}".as_slice())
        );

        let ciphertext = parsed.secrets.get("db-password").expect("secret present");
        let plaintext = crate::age::decrypt(&identity, ciphertext).expect("decrypt");
        assert_eq!(plaintext, b"hunter2");
    }

    /// The exact bug this feature hit in production: `age` encryption is
    /// non-deterministic, so without a cache, a secret whose revision never
    /// changed would still get fresh ciphertext (and a spurious commit)
    /// every single export tick.
    #[test]
    fn an_unchanged_secret_reuses_its_previous_ciphertext_across_ticks() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let state = state_with_everything(&key);
        let mut cache = SecretCache::new();

        let first = build(&state, &key, &recipient, &mut cache);
        let second = build(&state, &key, &recipient, &mut cache);

        let first_secret = &first
            .iter()
            .find(|f| f.path == "secrets/db-password.age")
            .unwrap()
            .contents;
        let second_secret = &second
            .iter()
            .find(|f| f.path == "secrets/db-password.age")
            .unwrap()
            .contents;
        assert_eq!(first_secret, second_secret);
    }

    /// A real change (a bumped revision) must still produce new ciphertext
    /// - the cache must not paper over an actual secret rotation.
    #[test]
    fn a_secret_with_a_bumped_revision_is_re_encrypted() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let mut state = state_with_everything(&key);
        let mut cache = SecretCache::new();

        let first = build(&state, &key, &recipient, &mut cache);
        let first_secret = first
            .iter()
            .find(|f| f.path == "secrets/db-password.age")
            .unwrap()
            .contents
            .clone();

        let sealed = Sealed::seal(&key, "db-password", b"new-password").expect("seal");
        let entry = state.secrets.get_mut("db-password").unwrap();
        entry.sealed = sealed.blob;
        entry.revision = 2;

        let second = build(&state, &key, &recipient, &mut cache);
        let second_secret = &second
            .iter()
            .find(|f| f.path == "secrets/db-password.age")
            .unwrap()
            .contents;
        assert_ne!(&first_secret, second_secret);
        let plaintext = crate::age::decrypt(&identity, second_secret).expect("decrypt");
        assert_eq!(plaintext, b"new-password");
    }

    /// A removed secret must not linger in the cache forever.
    #[test]
    fn a_removed_secret_is_dropped_from_the_cache() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let mut state = state_with_everything(&key);
        let mut cache = SecretCache::new();

        build(&state, &key, &recipient, &mut cache);
        assert!(cache.0.contains_key("db-password"));

        state.secrets.remove("db-password");
        build(&state, &key, &recipient, &mut cache);
        assert!(!cache.0.contains_key("db-password"));
    }

    #[test]
    fn parse_dir_on_a_missing_directory_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let parsed = parse_dir(dir.path()).expect("parse");
        assert!(parsed.cluster_yaml.is_none());
        assert!(parsed.stacks.is_empty());
        assert!(parsed.configs.is_empty());
        assert!(parsed.secrets.is_empty());
    }
}
