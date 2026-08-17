//! The `clouduler stack` commands. These talk to a running daemon over the node
//! RPC channel; the stack and its files end up in the replicated database, so
//! every manager holds them and any node can run the containers.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackBundle};
use cl_raft::client::Client;
use cl_raft::state::{base64_encode, Request, MAX_OBJECT_BYTES};
use cl_secret::{ClusterKey, Sealed};

/// Pick a node to talk to. Any node will do for a write, since it forwards to
/// the leader, so prefer the one asked for and otherwise take the first manager.
pub fn client_for(cfg: &ClusterConfig, via: Option<&str>) -> Result<Client> {
    let (name, node) = match via {
        Some(name) => (
            name,
            cfg.node(name)
                .ok_or_else(|| anyhow!("no node named {name} in the config"))?,
        ),
        None => {
            let (name, node) = cfg
                .managers()
                .next()
                .ok_or_else(|| anyhow!("config has no manager to talk to"))?;
            (name.as_str(), node)
        }
    };

    let addr = cfg
        .rpc_addr(node)
        .ok_or_else(|| anyhow!("node {name} has no address on the internal network"))?;
    Ok(Client::connect(addr.to_string()))
}

pub async fn deploy(
    cfg: &ClusterConfig,
    file: &Path,
    name: Option<String>,
    via: Option<&str>,
) -> Result<()> {
    let bundle = StackBundle::load(file).with_context(|| format!("reading {}", file.display()))?;
    let name = name.unwrap_or_else(|| bundle.name.clone());

    // Catch what the cluster would reject anyway, but locally and with a
    // message that names the file.
    bundle
        .parsed
        .validate(cfg)
        .with_context(|| format!("validating {}", file.display()))?;

    for (name, f) in &bundle.files {
        if f.content.len() > MAX_OBJECT_BYTES {
            bail!(
                "{name} is {} bytes, over the {MAX_OBJECT_BYTES} byte limit",
                f.content.len()
            );
        }
    }

    let client = client_for(cfg, via)?;
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

    // Recorded last, so a failure halfway leaves objects the stack does not yet
    // claim rather than claims with nothing behind them.
    let response = client
        .submit(Request::SetStackObjects {
            name: stack.to_string(),
            configs,
            secrets,
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
