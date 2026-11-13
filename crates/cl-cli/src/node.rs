//! The `clouduler node` commands that change live cluster state. These talk
//! to a running daemon over the node RPC channel, the same way the `stack`
//! commands do, and mirror what the web ui's `/api/nodes` routes do to the
//! replicated roster and the config file.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::{Availability, NodeState, Request, Role};
use tracing::debug;

use crate::cli::AvailabilityArg;
use cl_init::plan::parse_drive_arg;
use cl_init::prompt::Prompt;
use cl_raft::client::client_for;

pub async fn add(
    cfg: &ClusterConfig,
    name: String,
    role: Role,
    rpc_addr: SocketAddr,
    drives: &[String],
    labels: &[String],
    via: Option<&str>,
) -> Result<()> {
    let drives = drives
        .iter()
        .map(|arg| parse_drive_arg(arg))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let labels = labels
        .iter()
        .map(|arg| parse_label_arg(arg))
        .collect::<Result<BTreeMap<_, _>>>()?;

    let client = client_for(cfg, via)?;
    debug!(
        name = name.as_str(),
        ?role,
        %rpc_addr,
        drives = drives.len(),
        labels = labels.len(),
        via = via.unwrap_or("<any manager>"),
        "submitting UpsertNode"
    );
    let response = client
        .submit(Request::UpsertNode(NodeState {
            name: name.clone(),
            id: uuid::Uuid::new_v4(),
            role,
            rpc_addr,
            drives,
            availability: Availability::Active,
            labels,
            last_resort: false,
            egress_networks: Default::default(),
            networks: Default::default(),
            open_ports: Default::default(),
            arch: String::new(),
            binary_hash: String::new(),
            node_pubkey: None,
            memory_total_bytes: 0,
            gpus: Default::default(),
            version: Default::default(),
        }))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the node: {error}");
    }

    println!(
        "added {name} to the live roster. This does not touch the config file: edit it and run \
         `clouduler config reload`, or a restarted manager will forget about it"
    );
    Ok(())
}

fn parse_label_arg(arg: &str) -> Result<(String, String)> {
    let Some((key, value)) = arg.split_once('=') else {
        bail!("--label takes key=value, got {arg}");
    };
    let key = key.trim();
    if key.is_empty() {
        bail!("--label {arg} has an empty key");
    }
    Ok((key.to_string(), value.trim().to_string()))
}

/// Removes the node from the live roster and, if the config still declares
/// it, from the config too. Mirrors `DELETE /api/nodes/{name}`: the config
/// edit is validated before the roster is touched, so a rejected removal
/// leaves everything untouched rather than tombstoning the node in raft with
/// no way to also drop it from the config.
pub async fn remove(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    let mut config = match &state.cluster_config {
        Some(entry) => {
            ClusterConfig::parse(&entry.yaml).context("parsing the cluster's live config")?
        }
        None => cfg.clone(),
    };
    let expect_generation = state.cluster_config.map(|c| c.generation).unwrap_or(0);

    let had_entry = config.nodes.remove(name).is_some();
    if had_entry {
        config.validate().context("config without this node")?;
    }
    debug!(name, had_entry, "submitting RemoveNode");

    let response = client
        .submit(Request::RemoveNode {
            node: name.to_string(),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the removal: {error}");
    }

    if had_entry {
        let yaml = serde_yaml::to_string(&config).context("re-encoding the config")?;
        debug!(
            expect_generation,
            "submitting PutClusterConfig to drop the node from the config"
        );
        let response = client
            .submit(Request::PutClusterConfig {
                yaml,
                expect_generation,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!(
                "removed {name} from the live roster, but could not drop it from the config: {error}. \
                 Remove it by hand and run `clouduler config reload`"
            );
        }
    }

    println!("removed {name}");
    Ok(())
}

/// Disaster recovery for a cluster that has lost quorum for good: force
/// `name`'s own local raft state to make it the cluster's sole voter, and let
/// only `stacks` (or, with none named, every stack with `x-clouduler.priority:
/// -1`) come back up, trusting this node's own local data for them.
///
/// The rewrite goes straight at this node's own data directory, so a running
/// daemon is stopped through systemd first and started again afterwards; `via`
/// does not apply. Then this waits until the stacks' containers run, saying
/// what holds them back otherwise. See `cl_raft::node::emergency` for what
/// actually changes.
pub async fn emergency(
    cfg: &ClusterConfig,
    name: &str,
    stacks: &[String],
    data_dir_override: Option<PathBuf>,
    yes: bool,
) -> Result<()> {
    let node = cfg
        .node(name)
        .ok_or_else(|| anyhow!("node {name} is not declared in the config"))?;
    let rpc_addr = cfg
        .rpc_addr(node)
        .ok_or_else(|| anyhow!("node {name} has no address on the internal network"))?;
    let data_dir = data_dir_override
        .unwrap_or_else(|| node.data_dir(cfg).to_path_buf())
        .join("raft");

    // The stacks' specs come out of this node's own local raft state. A
    // running daemon holds that file open, so it is read from a copy.
    let was_running = daemon_running();
    let specs = local_stack_specs(cfg, name, &data_dir, was_running).await?;
    let chosen: BTreeSet<String> = if stacks.is_empty() {
        specs
            .iter()
            .filter(|(_, spec)| {
                cl_config::StackFile::parse(spec).is_ok_and(|file| file.priority() == -1)
            })
            .map(|(stack, _)| stack.clone())
            .collect()
    } else {
        stacks.iter().cloned().collect()
    };
    if chosen.is_empty() {
        bail!("no stack named and none has `x-clouduler.priority: -1`");
    }
    let mut stack_volumes = BTreeSet::new();
    for stack in &chosen {
        let spec = specs.get(stack).ok_or_else(|| {
            anyhow!("{stack} is not a stack this node's own local state knows about")
        })?;
        let file = cl_config::StackFile::parse(spec).map_err(|e| anyhow!("{e}"))?;
        for service in file.services.values() {
            for mount in &service.volumes {
                if let (Some(drive), Some(volume)) = (mount.drive.as_ref(), mount.volume.as_ref()) {
                    stack_volumes.insert(cl_raft::ids::VolumeId::new(drive, volume));
                }
            }
        }
    }
    let list = chosen.iter().cloned().collect::<Vec<_>>().join(", ");

    let mut prompt = Prompt::new(std::io::stdin().lock(), std::io::stdout(), !yes);
    if !prompt.confirm(
        &format!(
            "make {name} the sole voter, bypassing consensus, and start only {list} on its local data?"
        ),
        yes,
    )? {
        bail!("aborted");
    }

    if was_running {
        systemctl(&["stop", DAEMON_UNIT])?;
    }
    let rewrite =
        cl_raft::node::emergency(&data_dir, node.id, rpc_addr, name, &chosen, &stack_volumes)
            .await
            .map_err(|e| anyhow!("{e}"));
    // Back up even when the rewrite failed, so a failed attempt does not
    // leave the node down.
    let started = if was_running || rewrite.is_ok() {
        systemctl(&["start", DAEMON_UNIT])
    } else {
        Ok(())
    };
    rewrite?;
    println!("{name} is the sole voter; only {list} will start");
    if let Err(e) = started {
        bail!("{e}; start the daemon by hand");
    }
    wait_until_up(cfg, name, &chosen).await
}

const DAEMON_UNIT: &str = "clouduler.service";

fn daemon_running() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", DAEMON_UNIT])
        .status()
        .is_ok_and(|s| s.success())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .with_context(|| format!("running systemctl {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Every stack spec in this node's own raft state: asked of the running
/// daemon, which answers reads from its own copy even without quorum, or read
/// from the database when it is stopped.
async fn local_stack_specs(
    cfg: &ClusterConfig,
    name: &str,
    data_dir: &std::path::Path,
    daemon_running: bool,
) -> Result<BTreeMap<String, String>> {
    if daemon_running {
        let state = client_for(cfg, Some(name))?
            .state()
            .await
            .map_err(|e| anyhow!("cannot read this node's state from its daemon: {e}"))?;
        return Ok(state
            .stacks
            .into_iter()
            .map(|(name, entry)| (name, entry.spec))
            .collect());
    }
    let store =
        cl_raft::store::Store::open(data_dir.join("raft.redb")).map_err(|e| anyhow!("{e}"))?;
    let shared = store.shared_state();
    let guard = shared.read().await;
    Ok(guard
        .data
        .stacks
        .iter()
        .map(|(name, entry)| (name.clone(), entry.spec.clone()))
        .collect())
}

/// Polls until every wanted instance of `stacks` runs. Ctrl+C only stops the
/// waiting: emergency mode is already applied and stays on.
async fn wait_until_up(cfg: &ClusterConfig, name: &str, stacks: &BTreeSet<String>) -> Result<()> {
    let mut last: BTreeMap<String, String> = BTreeMap::new();
    let mut note = |key: &str, line: String| {
        if last.get(key) != Some(&line) {
            println!("{line}");
            last.insert(key.to_string(), line);
        }
    };
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => {
                println!("stopped waiting; emergency mode stays on");
                return Ok(());
            }
            _ = ticker.tick() => {}
        }

        let docker = match cl_docker::Docker::connect() {
            Ok(docker) => docker.ping().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let state = match client_for(cfg, Some(name)) {
            Ok(client) => client.state().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let state = match state {
            Ok(state) => state,
            Err(e) => {
                note("", format!("waiting for the daemon: {e}"));
                continue;
            }
        };
        let rows = match crate::health::stack_status(cfg, &state, stacks).await {
            Ok(rows) => rows,
            Err(e) => {
                note("", format!("cannot read status: {e}"));
                continue;
            }
        };

        if let Err(e) = &docker {
            note("docker", format!("docker daemon unreachable: {e}"));
        }
        if rows.is_empty() {
            note("", "waiting for placement".to_string());
            continue;
        }
        for row in &rows {
            let line = match (&row.reason, row.is_up()) {
                (_, true) => format!("  {} running", row.instance),
                (Some(reason), false) => format!("  {} {}: {reason}", row.instance, row.state),
                (None, false) if docker.is_err() => {
                    format!("  {} {}: docker daemon is down", row.instance, row.state)
                }
                (None, false) => format!("  {} {}", row.instance, row.state),
            };
            note(&row.instance, line);
        }
        if rows.iter().all(|r| r.is_up()) {
            println!("up");
            return Ok(());
        }
    }
}

pub async fn set_availability(
    cfg: &ClusterConfig,
    name: &str,
    availability: AvailabilityArg,
    via: Option<&str>,
) -> Result<()> {
    let availability = match availability {
        AvailabilityArg::Active => Availability::Active,
        AvailabilityArg::Drain => Availability::Drain,
    };

    debug!(name, ?availability, "submitting SetAvailability");
    let response = client_for(cfg, via)?
        .submit(Request::SetAvailability {
            node: name.to_string(),
            availability,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the change: {error}");
    }

    println!("{name} is now {availability:?}");
    Ok(())
}
