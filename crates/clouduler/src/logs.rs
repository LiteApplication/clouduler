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

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::Duration;

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

/// `clouduler stack logs`: every instance's output merged into one stream,
/// ordered by the timestamp docker attached to each line rather than by
/// whichever node's connection happens to answer first.
///
/// Without `--follow` this is exact: every source closes once its tail is
/// sent, so the whole set is collected and sorted before anything prints.
/// With `--follow` the sources never close, so exactness would mean waiting
/// forever; instead, lines are held in a small buffer and flushed in
/// timestamp order every [`FOLLOW_BUFFER`], trading a bounded delay for
/// still-live output that reads in order across nodes.
const FOLLOW_BUFFER: Duration = Duration::from_millis(400);

pub async fn merged(cfg: &ClusterConfig, target: &str, args: LogsArgs, via: Option<&str>) -> Result<()> {
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

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for assignment in instances {
        let cfg = cfg.clone();
        let http = http.clone();
        let key = key.clone();
        let tx = tx.clone();
        let follow = args.follow;
        let tail = args.tail;
        tokio::spawn(async move {
            if let Err(e) = fetch_events(&cfg, &http, &key, &assignment, follow, tail, tx).await {
                eprintln!("{}: {e}", assignment.instance);
            }
        });
    }
    // Drop this end so `rx` closes once every spawned sender above is gone,
    // i.e. once every instance's stream has ended.
    drop(tx);

    if !args.follow {
        let mut buf = Vec::new();
        let mut seq = 0u64;
        while let Some(event) = rx.recv().await {
            buf.push(HeapItem(event.timestamp.clone(), seq, event));
            seq += 1;
        }
        buf.sort();
        for item in buf {
            print_merged(&item.2);
        }
        return Ok(());
    }

    let mut heap: BinaryHeap<Reverse<HeapItem>> = BinaryHeap::new();
    let mut seq = 0u64;
    let mut ticker = tokio::time::interval(FOLLOW_BUFFER);
    ticker.tick().await; // the first tick fires immediately; nothing to flush yet
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        heap.push(Reverse(HeapItem(event.timestamp.clone(), seq, event)));
                        seq += 1;
                    }
                    None => break,
                }
            }
            _ = ticker.tick() => {
                while let Some(Reverse(item)) = heap.pop() {
                    print_merged(&item.2);
                }
            }
        }
    }
    while let Some(Reverse(item)) = heap.pop() {
        print_merged(&item.2);
    }
    Ok(())
}

/// One line pulled off an instance's log stream, kept structured (rather than
/// printed immediately like [`stream_one`] does) so [`merged`] can sort a
/// batch of them across instances before anything reaches the terminal.
struct MergedEvent {
    instance: String,
    timestamp: Option<String>,
    kind: String,
    text: String,
}

/// Sortable by (timestamp, arrival order): docker's timestamps are fixed-width
/// RFC3339 in UTC, so lexicographic order on the string is chronological
/// order. The arrival-order tiebreaker only matters for lines with no
/// timestamp (the synthetic "error" event), keeping those in the order they
/// were received instead of comparing `MergedEvent`'s unrelated fields.
struct HeapItem(Option<String>, u64, MergedEvent);

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        (&self.0, self.1) == (&other.0, other.1)
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.0, self.1).cmp(&(&other.0, other.1))
    }
}

async fn fetch_events(
    cfg: &ClusterConfig,
    http: &reqwest::Client,
    key: &ClusterKey,
    assignment: &Assignment,
    follow: bool,
    tail: u32,
    tx: tokio::sync::mpsc::UnboundedSender<MergedEvent>,
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

    let instance = assignment.instance.to_string();
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
                let (timestamp, text) = match event.as_str() {
                    "stdout" | "stderr" => match serde_json::from_str::<LogPayload>(data) {
                        Ok(p) => (p.timestamp, p.text),
                        Err(_) => (None, data.to_string()),
                    },
                    _ => (None, data.to_string()),
                };
                // The channel's other end goes away only when `merged` has
                // already flushed and returned; nothing left to do with a
                // send that fails at that point.
                let _ = tx.send(MergedEvent { instance: instance.clone(), timestamp, kind: event.clone(), text });
            }
        }
    }
    Ok(())
}

fn print_merged(event: &MergedEvent) {
    let ts = event.timestamp.as_deref().unwrap_or("?");
    let prefix = format!("{ts} {} | ", event.instance);
    match event.kind.as_str() {
        "stderr" => eprintln!("{prefix}{}", event.text),
        "error" => eprintln!("{prefix}error: {}", event.text),
        _ => println!("{prefix}{}", event.text),
    }
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
    #[serde(default)]
    timestamp: Option<String>,
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
