//! `clouduler restore`: disaster recovery for lost *content* (cluster
//! config, stack specs, cluster configs, secrets), read back from the git
//! repo `backup:` mirrors it to. See `cl_raft::node::restore` for what
//! actually changes, and `crate::node::emergency` for the sibling command
//! this mirrors that recovers *quorum* instead of content.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::{ClusterConfigEntry, ConfigEntry, SecretEntry, StackState};
use cl_secret::{ClusterKey, Sealed};

use crate::init::prompt::Prompt;

pub struct RestoreArgs<'a> {
    pub name: &'a str,
    pub source: &'a str,
    pub age_identity_file: Option<&'a Path>,
    pub age_identity_stdin: bool,
    pub branch: &'a str,
    pub data_dir_override: Option<PathBuf>,
    pub yes: bool,
}

/// Unlike every other command in this module, this never talks to a running
/// daemon - it edits `name`'s own data directory directly, so its daemon
/// must be stopped first.
pub async fn run(cfg: &ClusterConfig, args: RestoreArgs<'_>) -> Result<()> {
    let RestoreArgs {
        name,
        source,
        age_identity_file,
        age_identity_stdin,
        branch,
        data_dir_override,
        yes,
    } = args;

    let node = cfg
        .node(name)
        .ok_or_else(|| anyhow!("node {name} is not declared in the config"))?;
    let data_dir = data_dir_override
        .unwrap_or_else(|| node.data_dir(cfg).to_path_buf())
        .join("raft");

    let identity_text = read_identity(age_identity_file, age_identity_stdin)?;
    let identity = cl_backup::age::parse_identity_file(&identity_text)
        .context("the age identity could not be parsed")?;

    let local = local_checkout(source, branch)?;
    let parsed = cl_backup::tree::parse_dir(local.path())
        .with_context(|| format!("reading the backup tree at {}", local.path().display()))?;

    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "restoring secrets needs this node's own cluster key at {}. Run this from a node that holds it",
            cfg.secrets.key_file.display()
        )
    })?;

    let RestoredState {
        cluster_config,
        stacks,
        configs,
        secrets,
        skipped,
    } = build_restored_state(parsed, &identity, &key)?;

    let mut prompt = Prompt::new(std::io::stdin().lock(), std::io::stdout(), !yes);
    prompt.say(&format!(
        "this replaces {name}'s local cluster config, stack specs, configs and secrets with what \
         is in {source}, directly rewriting {} - it does not touch raft membership or the log, so \
         run `clouduler emergency` first if quorum itself is also lost. make sure this node's own \
         daemon is stopped before continuing.",
        data_dir.join("raft.redb").display()
    ))?;
    prompt.say(&format!(
        "about to restore: cluster config {}, {} stack(s), {} config(s), {} secret(s){}.",
        if cluster_config.is_some() { "present" } else { "absent" },
        stacks.len(),
        configs.len(),
        secrets.len(),
        if skipped.is_empty() {
            String::new()
        } else {
            format!(
                " ({} secret(s) skipped, could not be decrypted with the given identity: {})",
                skipped.len(),
                skipped.join(", ")
            )
        }
    ))?;
    if !prompt.confirm(&format!("replace {name}'s local content with this backup?"), yes)? {
        bail!("aborted");
    }

    cl_raft::node::restore(&data_dir, cluster_config, stacks, configs, secrets)
        .await
        .map_err(|e| anyhow!("{e}"))?;

    println!(
        "{name}'s local content has been restored from {source}. Start its daemon; raft membership \
         is unchanged, so this only works once the node can reach quorum normally."
    );
    Ok(())
}

struct RestoredState {
    cluster_config: Option<ClusterConfigEntry>,
    stacks: BTreeMap<String, StackState>,
    configs: BTreeMap<String, ConfigEntry>,
    secrets: BTreeMap<String, SecretEntry>,
    /// Secrets present in the tree but not decryptable with the given
    /// identity - reported to the operator, not fatal, since one bad secret
    /// must not block restoring everything else.
    skipped: Vec<String>,
}

/// Converts a parsed git tree into the four fields `Store::restore_state`
/// takes. Revision/generation numbers are reset to a fresh sequence (git
/// history's own numbers are meaningless to a freshly rebuilt state
/// machine); ownership metadata is not reproduced, since `cl_backup::tree`
/// never exported it in the first place.
fn build_restored_state(
    parsed: cl_backup::tree::ParsedTree,
    identity: &age::x25519::Identity,
    key: &ClusterKey,
) -> Result<RestoredState> {
    let cluster_config = parsed.cluster_yaml.map(|yaml| ClusterConfigEntry { yaml, generation: 1 });

    let mut stacks = BTreeMap::new();
    for (stack_name, spec) in &parsed.stacks {
        stacks.insert(
            stack_name.clone(),
            StackState {
                name: stack_name.clone(),
                spec: spec.clone(),
                revision: 1,
                configs: Default::default(),
                secrets: Default::default(),
                used_configs: Default::default(),
                used_secrets: Default::default(),
            },
        );
    }

    let mut configs = BTreeMap::new();
    for (config_name, bytes) in &parsed.configs {
        configs.insert(
            config_name.clone(),
            ConfigEntry {
                name: config_name.clone(),
                content: cl_backup::tree::encode_config_content(bytes),
                revision: 1,
                owner: None,
            },
        );
    }

    let mut secrets = BTreeMap::new();
    let mut skipped = Vec::new();
    for (secret_name, ciphertext) in &parsed.secrets {
        match cl_backup::age::decrypt(identity, ciphertext) {
            Ok(plaintext) => {
                let sealed = Sealed::seal(key, secret_name, &plaintext)
                    .with_context(|| format!("resealing secret {secret_name}"))?;
                secrets.insert(
                    secret_name.clone(),
                    SecretEntry {
                        name: secret_name.clone(),
                        sealed: sealed.blob,
                        revision: 1,
                        key_fingerprint: key.fingerprint(),
                        owner: None,
                    },
                );
            }
            Err(_) => skipped.push(secret_name.clone()),
        }
    }

    Ok(RestoredState {
        cluster_config,
        stacks,
        configs,
        secrets,
        skipped,
    })
}

fn read_identity(file: Option<&Path>, stdin: bool) -> Result<String> {
    match (file, stdin) {
        (Some(path), _) => {
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
        }
        (None, true) => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading the age identity from stdin")?;
            Ok(buf)
        }
        (None, false) => bail!("pass the admin's age identity with --age-identity-file or --age-identity-stdin"),
    }
}

/// A cloned working copy of the backup repo, held only so its temp directory
/// (if any) survives until the caller is done reading from it.
enum LocalCheckout {
    Existing(PathBuf),
    Cloned(tempfile::TempDir),
}

impl LocalCheckout {
    fn path(&self) -> &Path {
        match self {
            LocalCheckout::Existing(p) => p,
            LocalCheckout::Cloned(dir) => dir.path(),
        }
    }
}

/// `source` is used as-is if it names an existing local directory; otherwise
/// it is treated as a url and cloned into a temp dir with the ambient ssh
/// agent, since a fresh restore cannot rely on clouduler's own deploy key
/// being available yet - that key is itself part of the state being
/// recovered.
fn local_checkout(source: &str, branch: &str) -> Result<LocalCheckout> {
    let path = Path::new(source);
    if path.is_dir() {
        return Ok(LocalCheckout::Existing(path.to_path_buf()));
    }

    let dir = tempfile::tempdir().context("creating a temp directory to clone the backup repo into")?;
    cl_backup::git::clone_with_agent(source, branch, dir.path())
        .with_context(|| format!("cloning {source} (branch {branch})"))?;

    Ok(LocalCheckout::Cloned(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offline round-trip: hand-write a git working tree the way
    /// `cl_backup::tree::build` would have, run it through the conversion
    /// this command applies, and check the result is directly what
    /// `Store::restore_state` expects - a secret readable by this node's own
    /// cluster key, not the age identity it arrived encrypted for.
    #[test]
    fn a_hand_written_tree_round_trips_into_restorable_state() -> Result<()> {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();

        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("cluster.yaml"), "nodes: []\n")?;
        std::fs::create_dir(dir.path().join("stacks"))?;
        std::fs::write(dir.path().join("stacks/web.yaml"), "services: {}\n")?;
        std::fs::create_dir(dir.path().join("configs"))?;
        std::fs::write(dir.path().join("configs/nginx.conf"), b"server {}")?;
        std::fs::create_dir(dir.path().join("secrets"))?;
        let ciphertext = cl_backup::age::encrypt(&recipient, b"hunter2")?;
        std::fs::write(dir.path().join("secrets/db-password.age"), &ciphertext)?;

        let parsed = cl_backup::tree::parse_dir(dir.path())?;
        let restored = build_restored_state(parsed, &identity, &key)?;

        assert_eq!(restored.cluster_config.map(|c| c.yaml), Some("nodes: []\n".to_string()));
        assert_eq!(restored.stacks.get("web").map(|s| s.spec.as_str()), Some("services: {}\n"));
        assert_eq!(restored.stacks.get("web").map(|s| s.revision), Some(1));
        assert_eq!(
            restored.configs.get("nginx.conf").map(|c| c.decode().expect("decode")),
            Some(b"server {}".to_vec())
        );
        assert!(restored.skipped.is_empty());

        let secret = restored.secrets.get("db-password").expect("secret present");
        assert_eq!(secret.key_fingerprint, key.fingerprint());
        let sealed = Sealed::from_blob(secret.sealed.clone());
        assert_eq!(sealed.open(&key, "db-password").expect("open with this node's own key"), b"hunter2");

        Ok(())
    }

    /// A secret this admin identity cannot open (e.g. re-encrypted for a
    /// rotated age key after this backup was taken) must not block restoring
    /// everything else - it is reported via `skipped`, not fatal.
    #[test]
    fn a_secret_the_identity_cannot_open_is_skipped_not_fatal() -> Result<()> {
        let key = ClusterKey::generate();
        let identity = age::x25519::Identity::generate();
        let wrong_recipient = age::x25519::Identity::generate().to_public();

        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("cluster.yaml"), "nodes: []\n")?;
        std::fs::create_dir(dir.path().join("secrets"))?;
        let ciphertext = cl_backup::age::encrypt(&wrong_recipient, b"hunter2")?;
        std::fs::write(dir.path().join("secrets/db-password.age"), &ciphertext)?;

        let parsed = cl_backup::tree::parse_dir(dir.path())?;
        let restored = build_restored_state(parsed, &identity, &key)?;

        assert!(restored.secrets.is_empty());
        assert_eq!(restored.skipped, vec!["db-password".to_string()]);
        assert!(restored.cluster_config.is_some());

        Ok(())
    }

    #[test]
    fn local_checkout_uses_an_existing_directory_as_is() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("cluster.yaml"), "nodes: []\n")?;

        let checkout = local_checkout(dir.path().to_str().expect("utf8 path"), "main")?;
        assert_eq!(checkout.path(), dir.path());
        Ok(())
    }
}
