//! The `clouduler stack` commands. These talk to a running daemon over the node
//! RPC channel; the stack and its files end up in the replicated database, so
//! every manager holds them and any node can run the containers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackBundle};
use cl_raft::client::Client;
use cl_raft::state::{base64_encode, DesiredState, Request, MAX_OBJECT_BYTES};
use cl_secret::{ClusterKey, Sealed};
use tracing::debug;

/// Pick a node to talk to. Any node will do for a write, since it forwards to
/// the leader, so prefer the one asked for and otherwise take the first manager.
pub use cl_raft::client::client_for;

pub async fn deploy(
    cfg: &ClusterConfig,
    file: &Path,
    name: Option<String>,
    via: Option<&str>,
) -> Result<()> {
    let bundle = StackBundle::load(file).with_context(|| format!("reading {}", file.display()))?;
    let name = name.unwrap_or_else(|| bundle.name.clone());
    deploy_bundle(cfg, name, bundle, via).await
}

/// The part of deploying shared by `stack deploy` (bundle read from a file
/// named on the command line) and `stack source set` (bundle read from a
/// file or stdin, name taken from the existing stack). Creates the stack if
/// it doesn't exist yet, otherwise bumps its revision - see
/// `Request::DeployStack` in cl-raft.
async fn deploy_bundle(cfg: &ClusterConfig, name: String, bundle: StackBundle, via: Option<&str>) -> Result<()> {
    debug!(
        name,
        services = bundle.parsed.services.len(),
        files = bundle.files.len(),
        "loaded bundle"
    );

    // Catch what the cluster would reject anyway, but locally.
    bundle.parsed.validate(cfg).context("validating stack")?;

    for (name, f) in &bundle.files {
        if f.content.len() > MAX_OBJECT_BYTES {
            bail!(
                "{name} is {} bytes, over the {MAX_OBJECT_BYTES} byte limit",
                f.content.len()
            );
        }
    }

    let client = client_for(cfg, via)?;
    debug!(name, "submitting DeployStack");
    let response = client
        .submit(Request::DeployStack {
            name: name.clone(),
            spec: bundle.spec.clone(),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if let Some(error) = response.error {
        bail!("cluster rejected the stack: {error}");
    }

    debug!(files = bundle.files.len(), "publishing bundled config and secret objects");
    publish_objects(cfg, &client, &name, &bundle).await?;

    let stacks = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let revision = stacks.stacks.get(&name).map(|s| s.revision).unwrap_or(0);
    println!(
        "deployed {name} revision {revision}, {} services, {} bundled files",
        bundle.parsed.services.len(),
        bundle.files.len()
    );
    Ok(())
}

/// Print a stack's raw source: the yaml exactly as bundled at deploy time,
/// which is what `stack source set` (and a redeploy from a saved copy of
/// this output) would take back in. Same content `stack show` prints,
/// without the `--files` mode.
pub async fn source_get(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let stack = state
        .stacks
        .get(name)
        .ok_or_else(|| anyhow!("no stack named {name}"))?;
    print!("{}", stack.spec);
    Ok(())
}

/// Replace a stack's source, read from `file` if given or from stdin
/// otherwise, the same as redeploying it with a new copy of its file. Unlike
/// `stack deploy`, this is edit-in-place: it refuses a name with no existing
/// stack rather than silently creating one, since `stack source` is for
/// editing, not creating.
pub async fn source_set(cfg: &ClusterConfig, name: &str, file: Option<&Path>, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if !state.stacks.contains_key(name) {
        bail!("no stack named {name}; use `clouduler stack deploy` to create one");
    }

    let (spec, base) = match file {
        Some(file) => (
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?,
            file.parent().unwrap_or(Path::new(".")).to_path_buf(),
        ),
        None => {
            use std::io::Read;
            let mut spec = String::new();
            std::io::stdin()
                .read_to_string(&mut spec)
                .context("reading stdin")?;
            (spec, std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        }
    };

    let bundle = cl_config::StackBundle::from_spec(name, &spec, &base).context("parsing stack source")?;
    deploy_bundle(cfg, name.to_string(), bundle, via).await
}

/// Turn the stack's bundled files into cluster config and secret objects, so
/// the containers can mount them wherever they end up running.
///
/// Secrets are sealed here rather than by the cluster, because plaintext must
/// never travel through the raft log. That means this command needs the cluster
/// key, which is fine on a node and is the reason the error below says where to
/// go instead when it is missing.
async fn publish_objects(
    cfg: &ClusterConfig,
    client: &Client,
    stack: &str,
    bundle: &StackBundle,
) -> Result<()> {
    let mut configs = BTreeSet::new();
    let mut secrets = BTreeSet::new();

    let needs_key = bundle.files.values().any(|f| f.secret);
    let key = if needs_key {
        Some(
            ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
                format!(
                    "this stack has secrets, so the cluster key at {} is needed. Deploy from a node that holds it, or use the web ui",
                    cfg.secrets.key_file.display()
                )
            })?,
        )
    } else {
        None
    };

    for (name, file) in &bundle.files {
        debug!(name, secret = file.secret, bytes = file.content.len(), "publishing object");
        let request = if file.secret {
            let key = key.as_ref().expect("checked above");
            let sealed = Sealed::seal(key, name, &file.content)
                .with_context(|| format!("sealing secret {name}"))?;
            secrets.insert(name.clone());
            Request::PutSecret {
                name: name.clone(),
                sealed: sealed.blob,
                key_fingerprint: key.fingerprint(),
                owner: Some(stack.to_string()),
            }
        } else {
            configs.insert(name.clone());
            Request::PutConfig {
                name: name.clone(),
                content: base64_encode(&file.content),
                owner: Some(stack.to_string()),
            }
        };

        let response = client.submit(request).await.map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("cluster rejected {name}: {error}");
        }
    }

    let requirements = bundle.parsed.requirements();

    // Recorded last, so a failure halfway leaves objects the stack does not yet
    // claim rather than claims with nothing behind them.
    debug!(configs = configs.len(), secrets = secrets.len(), "submitting SetStackObjects");
    let response = client
        .submit(Request::SetStackObjects {
            name: stack.to_string(),
            configs,
            secrets,
            used_configs: requirements.configs,
            used_secrets: requirements.secrets,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the object list: {error}");
    }
    Ok(())
}

pub async fn list(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if state.stacks.is_empty() {
        println!("no stacks deployed");
        return Ok(());
    }

    for (name, stack) in &state.stacks {
        let running = state
            .assignments
            .keys()
            .filter(|id| &id.stack == name)
            .count();
        println!(
            "{name}\trevision={}\tconfigs={}\tsecrets={}\tinstances={running}",
            stack.revision,
            stack.configs.len(),
            stack.secrets.len()
        );
    }
    Ok(())
}

pub async fn show(cfg: &ClusterConfig, name: &str, files: bool, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let stack = state
        .stacks
        .get(name)
        .ok_or_else(|| anyhow!("no stack named {name}"))?;

    if files {
        for name in &stack.configs {
            println!("{name}\tconfig");
        }
        // Secret values never leave the node, so only the name shows.
        for name in &stack.secrets {
            println!("{name}\tsecret");
        }
        return Ok(());
    }

    print!("{}", stack.spec);
    Ok(())
}

pub async fn remove(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    debug!(name, "submitting RemoveStack");
    let response = client_for(cfg, via)?
        .submit(Request::RemoveStack {
            name: name.to_string(),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if let Some(error) = response.error {
        bail!("cluster rejected the removal: {error}");
    }
    println!("removed {name}");
    Ok(())
}

pub async fn stop(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    set_all_desired(cfg, name, via, DesiredState::Stopped).await
}

pub async fn start(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    set_all_desired(cfg, name, via, DesiredState::Running).await
}

async fn set_all_desired(cfg: &ClusterConfig, name: &str, via: Option<&str>, desired: DesiredState) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let instances = crate::logs::resolve_target(&state, name)?;

    for assignment in &instances {
        debug!(instance = %assignment.instance, ?desired, "submitting SetDesiredState");
        let response = client
            .submit(Request::SetDesiredState { instance: assignment.instance.clone(), desired })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("could not set {} to {desired:?}: {error}", assignment.instance);
        }
    }

    let verb = match desired {
        DesiredState::Running => "started",
        DesiredState::Stopped => "stopped",
    };
    println!("{verb} {} instance(s) of {name}", instances.len());
    Ok(())
}

/// Unassigns every currently assigned instance of the stack rather than
/// restarting it in place, so the scheduler's next placement pass picks each
/// one up as freshly desired: it lands back on the same node if that node is
/// still the best fit, or on another one if it no longer is (unreachable,
/// drained, missing a needed drive or network). An in-place restart would
/// instead retry forever on whatever node it already happened to be on, even
/// one that can no longer host it.
pub async fn restart(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let instances = crate::logs::resolve_target(&state, name)?;

    let mut failed = Vec::new();
    for assignment in &instances {
        debug!(instance = %assignment.instance, "submitting Unassign");
        let response = client
            .submit(Request::Unassign {
                instance: assignment.instance.clone(),
            })
            .await
            .map_err(|e| anyhow!("{e}"));
        match response {
            Ok(response) if response.error.is_none() => {}
            Ok(response) => {
                eprintln!("{}: {}", assignment.instance, response.error.unwrap());
                failed.push(assignment.instance.to_string());
            }
            Err(e) => {
                eprintln!("{}: {e}", assignment.instance);
                failed.push(assignment.instance.to_string());
            }
        }
    }

    if !failed.is_empty() {
        bail!("failed to restart: {}", failed.join(", "));
    }
    println!("restarted {} instance(s) of {name}", instances.len());
    Ok(())
}
