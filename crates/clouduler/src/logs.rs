//! `clouduler logs` — stream a container's output, or every container in a
//! stack.
//!
//! Container logs only exist on the node that runs the container, so this
//! looks the instance up in the replicated state to find its node and then
//! goes straight to that node's api over the same node-to-node channel
//! `cl-update`'s internal restart call and `cl-api`'s cross-node forwarding
//! use: the normal https api, authenticated by an HMAC signature over the
//! shared cluster key rather than a session or api key, since this is one
//! cluster member reading another's local docker state and no browser is
//! involved. That means it needs this node's cluster key, same as `stack
//! deploy` does for sealing secrets.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::{Assignment, ClusterState, InstanceId};
use cl_secret::ClusterKey;
use cl_update::internal_client::AUTH_HEADER;
use futures::StreamExt;
use serde::Deserialize;

use crate::stack::client_for;

pub struct LogsArgs {
    pub follow: bool,
    pub tail: u32,
}

pub async fn run(cfg: &ClusterConfig, target: &str, args: LogsArgs, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?.state().await.map_err(|e| anyhow!("{e}"))?;
    let instances = resolve_target(&state, target)?;

    let key = Arc::new(
        ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
            format!(
                "loading the cluster key from {} to authenticate to the node(s) running this",
                cfg.secrets.key_file.display()
            )
        })?,
    );
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the http client")?;

    // Only label lines by instance once there is more than one stream to tell
    // apart, so a single container's logs read exactly like `docker logs`.
    let label = instances.len() > 1;

    let mut tasks = instances
        .into_iter()
        .map(|assignment| {
            let cfg = cfg.clone();
            let http = http.clone();
            let key = key.clone();
            let follow = args.follow;
            let tail = args.tail;
            tokio::spawn(async move {
                if let Err(e) = stream_one(&cfg, &http, &key, &assignment, follow, tail, label).await {
                    eprintln!("{}: {e}", assignment.instance);
                }
            })
        })
        .collect::<futures::stream::FuturesUnordered<_>>();

    while tasks.next().await.is_some() {}
    Ok(())
}

/// A single instance id, or every currently assigned instance of a stack.
/// Shared with `clouduler exec`, which additionally requires the result to
/// name exactly one instance.
pub(crate) fn resolve_target(state: &ClusterState, target: &str) -> Result<Vec<Assignment>> {
    if let Ok(id) = target.parse::<InstanceId>() {
        if let Some(assignment) = state.assignments.get(&id) {
            return Ok(vec![assignment.clone()]);
        }
    }

    if state.stacks.contains_key(target) {
        let mut instances: Vec<Assignment> = state
            .assignments
            .values()
            .filter(|a| a.instance.stack == target)
            .cloned()
            .collect();
        if instances.is_empty() {
            bail!("stack {target} has no running instances");
        }
        instances.sort_by(|a, b| a.instance.cmp(&b.instance));
        return Ok(instances);
    }

    bail!("{target} is neither a running instance (stack.service.index) nor a deployed stack");
}

async fn stream_one(
    cfg: &ClusterConfig,
    http: &reqwest::Client,
    key: &ClusterKey,
    assignment: &Assignment,
    follow: bool,
    tail: u32,
    label: bool,
) -> Result<()> {
    let base = cl_update::peers::base_url(cfg, &assignment.node)
        .ok_or_else(|| anyhow!("node {} has no address on the internal network", assignment.node))?;
    let path = format!(
        "/api/instances/internal/{}/logs?follow={follow}&tail={tail}",
        assignment.instance
    );
    let signature = cl_secret::internal_auth::sign(key, &path);

    let resp = http
        .get(format!("{base}{path}"))
        .header(AUTH_HEADER, signature)
        .send()
        .await
        .with_context(|| format!("asking {} for {}'s logs", assignment.node, assignment.instance))?;
    if !resp.status().is_success() {
        bail!("{} answered {}", assignment.node, resp.status());
    }

    let prefix = label.then(|| assignment.instance.to_string());
    print_events(resp, prefix.as_deref()).await
}

#[derive(Deserialize)]
struct LogPayload {
    text: String,
}

/// Parses the `text/event-stream` body a step at a time: an event is
/// `event: <name>\ndata: <payload>\n\n`, and every event this endpoint sends
/// fits in one `data:` line (the server always encodes it through
/// `Event::json_data`/`Event::data`, neither of which folds a `\n` into the
/// wire format), so a line-based scan is enough without a full sse parser.
async fn print_events(resp: reqwest::Response, prefix: Option<&str>) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut event = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the log stream")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);

            if let Some(name) = line.strip_prefix("event: ") {
                event = name.to_string();
            } else if let Some(data) = line.strip_prefix("data: ") {
                emit(&event, data, prefix);
            }
        }
    }
    Ok(())
}

fn emit(event: &str, data: &str, prefix: Option<&str>) {
    let prefix = prefix.map(|p| format!("{p} | ")).unwrap_or_default();
    match event {
        "stdout" | "stderr" => {
            let text = serde_json::from_str::<LogPayload>(data).map(|p| p.text).unwrap_or_else(|_| data.to_string());
            if event == "stderr" {
                eprintln!("{prefix}{text}");
            } else {
                println!("{prefix}{text}");
            }
        }
        "error" => eprintln!("{prefix}error: {data}"),
        // Unrecognized event, e.g. a future addition: show it rather than
        // silently drop it.
        _ => println!("{prefix}{data}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{DesiredState, StackState};
    use std::collections::BTreeSet;

    fn stack_state(name: &str) -> StackState {
        StackState {
            name: name.to_string(),
            spec: String::new(),
            revision: 0,
            configs: BTreeSet::new(),
            secrets: BTreeSet::new(),
            used_configs: BTreeSet::new(),
            used_secrets: BTreeSet::new(),
        }
    }

    fn assignment(stack: &str, service: &str, index: u32, node: &str) -> Assignment {
        Assignment {
            instance: InstanceId::new(stack, service, index),
            node: node.to_string(),
            desired: DesiredState::Running,
            image_digest: None,
            migration: None,
        }
    }

    #[test]
    fn a_single_instance_id_resolves_to_just_that_assignment() {
        let mut state = ClusterState::default();
        state.assignments.insert(InstanceId::new("site", "web", 0), assignment("site", "web", 0, "n1"));
        state.assignments.insert(InstanceId::new("site", "web", 1), assignment("site", "web", 1, "n2"));

        let resolved = resolve_target(&state, "site.web.0").expect("resolves");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].node, "n1");
    }

    #[test]
    fn a_stack_name_resolves_to_every_one_of_its_running_instances() {
        let mut state = ClusterState::default();
        state.stacks.insert("site".to_string(), stack_state("site"));
        state.assignments.insert(InstanceId::new("site", "web", 0), assignment("site", "web", 0, "n1"));
        state.assignments.insert(InstanceId::new("site", "worker", 0), assignment("site", "worker", 0, "n2"));
        state.assignments.insert(InstanceId::new("other", "web", 0), assignment("other", "web", 0, "n3"));

        let resolved = resolve_target(&state, "site").expect("resolves");
        assert_eq!(resolved.len(), 2);
        assert!(resolved.iter().all(|a| a.instance.stack == "site"));
    }

    #[test]
    fn a_stack_with_no_running_instances_is_an_error() {
        let mut state = ClusterState::default();
        state.stacks.insert("site".to_string(), stack_state("site"));

        assert!(resolve_target(&state, "site").is_err());
    }

    #[test]
    fn an_unknown_target_is_an_error() {
        let state = ClusterState::default();
        assert!(resolve_target(&state, "ghost").is_err());
    }
}
