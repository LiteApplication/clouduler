//! The `clouduler stack` commands. These talk to a running daemon over the node
//! RPC channel; the stack and its files end up in the replicated database, so
//! every manager holds them and any node can run the containers.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackBundle};
use cl_raft::client::Client;
use cl_raft::state::{base64_encode, Attachment, Request, MAX_STACK_FILES_BYTES};

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

    if bundle.files_size() > MAX_STACK_FILES_BYTES {
        bail!(
            "{} carries {} bytes of config and secret files, over the {MAX_STACK_FILES_BYTES} byte limit",
            file.display(),
            bundle.files_size()
        );
    }

    let files: BTreeMap<String, Attachment> = bundle
        .files
        .iter()
        .map(|(key, f)| {
            (
                key.clone(),
                Attachment {
                    content: base64_encode(&f.content),
                    secret: f.secret,
                },
            )
        })
        .collect();

    let client = client_for(cfg, via)?;
    let response = client
        .submit(Request::DeployStack {
            name: name.clone(),
            spec: bundle.spec.clone(),
            files,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if let Some(error) = response.error {
        bail!("cluster rejected the stack: {error}");
    }

    let stacks = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let revision = stacks.stacks.get(&name).map(|s| s.revision).unwrap_or(0);
    println!(
        "deployed {name} revision {revision}, {} services, {} bundled files",
        bundle.parsed.services.len(),
        bundle.files.len()
    );
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
            "{name}\trevision={}\tfiles={}\tinstances={running}",
            stack.revision,
            stack.files.len()
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
        for (key, file) in &stack.files {
            // Secret contents never leave the node, so only the name shows.
            let kind = if file.secret { "secret" } else { "config" };
            println!("{key}\t{kind}");
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
