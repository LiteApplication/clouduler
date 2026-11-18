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
//! terraform/<stack>/<service>.tf
//! terraform/<stack>/<service>.tfstate.age
//! ```
//!
//! `build` is a full rebuild every tick, so there are no stale files after a rename or delete.
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
pub struct SecretCache(pub(crate) BTreeMap<String, (u64, Vec<u8>)>);

impl SecretCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a ciphertext already committed to the repo for `revision`.
    /// The repo is the truth for what is committed, so this overrides any
    /// local entry: a node that was leader earlier may hold an older one.
    pub fn seed(&mut self, name: String, revision: u64, ciphertext: Vec<u8>) {
        self.0.insert(name, (revision, ciphertext));
    }

    /// Like `seed`, for a terraform state (`key` is `"stack/service"`, `serial` its
    /// raft serial). Kept in the same map under a prefix no secret name can
    /// have, so the persisted cache format is unchanged.
    pub fn seed_tfstate(&mut self, key: String, serial: u64, ciphertext: Vec<u8>) {
        self.0
            .insert(format!("{TFSTATE_PREFIX}{key}"), (serial, ciphertext));
    }
}

/// Cache-key prefix for terraform states inside `SecretCache`.
const TFSTATE_PREFIX: &str = "/tfstate/";

/// Root-level file recording the raft serial each
/// `terraform/<stack>/<service>.tfstate.age` was encrypted from
/// (`{"stack/service": serial}`); the terraform twin of `SECRET_REVISIONS_PATH`.
pub const TERRAFORM_REVISIONS_PATH: &str = ".clouduler-terraform-revisions.json";

/// Root-level file recording the raft revision each `secrets/<name>.age` was
/// encrypted from (`{"name": revision}`). It is what lets a node that just
/// became leader, with no local `SecretCache`, reuse the committed
/// ciphertexts instead of re-encrypting every secret into a spurious commit.
pub const SECRET_REVISIONS_PATH: &str = ".clouduler-secret-revisions.json";

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
        // `stacks/a/b.yaml` is not part of the flat layout; exporting it any
        // other way would land on another stack's file. Skipped, not fatal:
        // one oddly named stack must not block every other change.
        if name.contains('/') {
            tracing::error!(stack = %name, "git backup: skipping stack whose name contains '/'");
            continue;
        }
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
    cache
        .0
        .retain(|name, _| name.starts_with(TFSTATE_PREFIX) || state.secrets.contains_key(name));

    let revisions: BTreeMap<&str, u64> = state
        .secrets
        .keys()
        .filter(|name| {
            files
                .iter()
                .any(|f| f.path == format!("secrets/{name}.age"))
        })
        .filter_map(|name| cache.0.get(name).map(|(rev, _)| (name.as_str(), *rev)))
        .collect();
    if !revisions.is_empty() {
        files.push(TreeFile {
            path: SECRET_REVISIONS_PATH.to_string(),
            contents: format!("{:#}", serde_json::Value::from_iter(revisions)).into_bytes(),
        });
    }

    export_terraform(state, key, recipient, cache, &mut files);

    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Adds `terraform/<stack>/<service>.tf` (plain) and `.tfstate.age` (the state
/// unsealed with the cluster key, age-encrypted to `recipient`, reused from
/// `cache` while the serial has not moved), plus the serial bookkeeping file.
fn export_terraform(
    state: &ClusterState,
    key: &ClusterKey,
    recipient: &age::x25519::Recipient,
    cache: &mut SecretCache,
    files: &mut Vec<TreeFile>,
) {
    let mut serials: BTreeMap<String, u64> = BTreeMap::new();
    let mut live = std::collections::BTreeSet::new();
    for (stack, modules) in &state.terraform_modules {
        if stack.contains('/') || stack.is_empty() {
            tracing::error!(stack = %stack, "git backup: skipping terraform modules of a stack whose name contains '/'");
            continue;
        }
        for (service, module) in modules {
            if service.contains('/') || service.is_empty() {
                continue;
            }
            files.push(TreeFile {
                path: format!("terraform/{stack}/{service}.tf"),
                contents: module.source.clone().into_bytes(),
            });
            let tf_key = cl_raft::terraform::terraform_key(stack, service);
            let Some(entry) = state.terraform_states.get(&tf_key) else {
                continue;
            };
            let cache_key = format!("{TFSTATE_PREFIX}{tf_key}");
            let ciphertext = match cache.0.get(&cache_key) {
                Some((serial, ciphertext)) if *serial == entry.serial => ciphertext.clone(),
                _ => {
                    let plaintext = match Sealed::from_blob(entry.sealed.clone()).open(key, &tf_key)
                    {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(terraform = %tf_key, error = %e, "git backup: skipping undecryptable terraform state");
                            continue;
                        }
                    };
                    match crate::age::encrypt(recipient, &plaintext) {
                        Ok(ciphertext) => {
                            cache
                                .0
                                .insert(cache_key.clone(), (entry.serial, ciphertext.clone()));
                            ciphertext
                        }
                        Err(e) => {
                            tracing::warn!(terraform = %tf_key, error = %e, "git backup: skipping terraform state that failed to re-encrypt");
                            continue;
                        }
                    }
                }
            };
            live.insert(cache_key);
            serials.insert(tf_key, entry.serial);
            files.push(TreeFile {
                path: format!("terraform/{stack}/{service}.tfstate.age"),
                contents: ciphertext,
            });
        }
    }
    cache
        .0
        .retain(|name, _| !name.starts_with(TFSTATE_PREFIX) || live.contains(name));
    if !serials.is_empty() {
        files.push(TreeFile {
            path: TERRAFORM_REVISIONS_PATH.to_string(),
            contents: format!("{:#}", serde_json::Value::from_iter(serials)).into_bytes(),
        });
    }
}

/// The reverse of `build`, read from a checked-out working tree. Used only by
/// `clouduler restore`; the hot-path pull uses `git::parse_tree`. Secrets stay
/// age-encrypted since only the admin holds the identity.
pub struct ParsedTree {
    pub cluster_yaml: Option<String>,
    /// Stack name -> raw spec yaml.
    pub stacks: BTreeMap<String, String>,
    /// Config name -> raw content.
    pub configs: BTreeMap<String, Vec<u8>>,
    /// Secret name -> age ciphertext.
    pub secrets: BTreeMap<String, Vec<u8>>,
    /// Stack name -> service name -> module source (`terraform/<stack>/<service>.tf`).
    pub terraform: BTreeMap<String, BTreeMap<String, String>>,
    /// `"stack/service"` -> age ciphertext of the tofu state
    /// (`terraform/<stack>/<service>.tfstate.age`).
    pub terraform_states: BTreeMap<String, Vec<u8>>,
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

    let mut terraform: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut terraform_states = BTreeMap::new();
    let tf_root = root.join("terraform");
    if let Ok(stack_dirs) = std::fs::read_dir(&tf_root) {
        for stack_dir in stack_dirs {
            let stack_dir = stack_dir.map_err(|source| BackupError::Io {
                path: tf_root.clone(),
                source,
            })?;
            let stack_path = stack_dir.path();
            if !stack_path.is_dir() {
                continue;
            }
            let Some(stack) = stack_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            for (name, bytes) in list_dir(&stack_path)? {
                if let Some(service) = name.strip_suffix(".tfstate.age") {
                    terraform_states.insert(format!("{stack}/{service}"), bytes);
                } else if let Some(service) = name.strip_suffix(".tf") {
                    terraform.entry(stack.to_string()).or_default().insert(
                        service.to_string(),
                        String::from_utf8_lossy(&bytes).into_owned(),
                    );
                }
            }
        }
    }

    Ok(ParsedTree {
        cluster_yaml,
        stacks,
        configs,
        secrets,
        terraform,
        terraform_states,
    })
}

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
                deployed_at: 0,
                previous_spec: None,
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

    /// A stack named `a/b` used to be exported as `stacks/b.yaml`, silently
    /// overwriting the real stack `b`.
    #[test]
    fn a_nested_stack_name_does_not_overwrite_a_sibling() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let mut state = state_with_everything(&key);
        let mut real = state.stacks["web"].clone();
        real.name = "b".to_string();
        real.spec = "services: {real: {image: a}}\n".to_string();
        state.stacks.insert("b".to_string(), real);
        let mut nested = state.stacks["web"].clone();
        nested.name = "a/b".to_string();
        nested.spec = "services: {nested: {image: a}}\n".to_string();
        state.stacks.insert("a/b".to_string(), nested);

        let files = build(&state, &key, &recipient, &mut SecretCache::new());
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"stacks/web.yaml"),
            "other stacks still export"
        );
        assert!(!paths.iter().any(|p| p.starts_with("stacks/a")));

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = git2::Repository::init(dir.path()).expect("init");
        let oid = crate::git::build_git_tree(&repo, &files).expect("build tree");
        let parsed = crate::git::parse_tree(&repo, oid).expect("parse tree");
        assert!(parsed.stacks["b"].contains("real"));
    }

    fn state_with_terraform(key: &ClusterKey, serial: u64, plaintext: &[u8]) -> ClusterState {
        use cl_raft::terraform::{TerraformModule, TerraformState};
        let mut state = state_with_everything(key);
        state.terraform_modules.insert(
            "web".into(),
            BTreeMap::from([(
                "app".to_string(),
                TerraformModule {
                    source: "variable \"x\" {}\n".into(),
                    revision: 1,
                },
            )]),
        );
        let sealed = Sealed::seal(key, "web/app", plaintext).expect("seal");
        state.terraform_states.insert(
            "web/app".into(),
            TerraformState {
                sealed: sealed.blob,
                serial,
                key_fingerprint: key.fingerprint(),
            },
        );
        state
    }

    #[test]
    fn terraform_exports_module_and_age_state_and_round_trips() {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let state = state_with_terraform(&key, 3, b"{\"serial\":9}");
        let mut cache = SecretCache::new();
        let files = build(&state, &key, &identity.to_public(), &mut cache);
        let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"terraform/web/app.tf"));
        assert!(paths.contains(&"terraform/web/app.tfstate.age"));
        assert!(paths.contains(&TERRAFORM_REVISIONS_PATH));
        let tf = files
            .iter()
            .find(|f| f.path == "terraform/web/app.tf")
            .unwrap();
        assert_eq!(tf.contents, b"variable \"x\" {}\n");

        // git round trip (nested 3-component paths)
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = git2::Repository::init(dir.path()).expect("init");
        let oid = crate::git::build_git_tree(&repo, &files).expect("build tree");
        let parsed = crate::git::parse_tree(&repo, oid).expect("parse tree");
        assert_eq!(parsed.terraform["web"]["app"], "variable \"x\" {}\n");
        let ct = &parsed.terraform_states["web/app"];
        assert_eq!(
            crate::age::decrypt(&identity, ct).expect("decrypt"),
            b"{\"serial\":9}"
        );

        // the same files written to disk parse identically through parse_dir (restore)
        let disk = tempfile::tempdir().expect("tempdir");
        for f in &files {
            let p = disk.path().join(&f.path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, &f.contents).unwrap();
        }
        let parsed = parse_dir(disk.path()).expect("parse_dir");
        assert_eq!(parsed.terraform["web"]["app"], "variable \"x\" {}\n");
        assert_eq!(
            crate::age::decrypt(&identity, &parsed.terraform_states["web/app"]).unwrap(),
            b"{\"serial\":9}"
        );
    }

    #[test]
    fn terraform_state_ciphertext_is_reused_until_the_serial_moves() {
        let key = ClusterKey::generate();
        let recipient = age::x25519::Identity::generate().to_public();
        let state = state_with_terraform(&key, 1, b"one");
        let mut cache = SecretCache::new();
        let ct = |files: &[TreeFile]| {
            files
                .iter()
                .find(|f| f.path == "terraform/web/app.tfstate.age")
                .unwrap()
                .contents
                .clone()
        };
        let first = ct(&build(&state, &key, &recipient, &mut cache));
        assert_eq!(first, ct(&build(&state, &key, &recipient, &mut cache)));
        let moved = state_with_terraform(&key, 2, b"two");
        assert_ne!(first, ct(&build(&moved, &key, &recipient, &mut cache)));

        // A removed module drops its cache entry.
        let mut gone = moved.clone();
        gone.terraform_modules.clear();
        gone.terraform_states.clear();
        build(&gone, &key, &recipient, &mut cache);
        assert!(!cache.0.keys().any(|k| k.starts_with(TFSTATE_PREFIX)));
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
                ".clouduler-secret-revisions.json",
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

    /// `age` encryption is non-deterministic, so without a cache, a secret
    /// whose revision never changed would still get fresh ciphertext (and a
    /// spurious commit) every single export tick.
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
