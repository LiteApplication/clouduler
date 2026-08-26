//! `clouduler stack unhealthy` — an at-a-glance list of stacks that currently
//! have a container worth looking at: crashed, restart-looping, failing its
//! healthcheck, stuck migrating, on an unreachable node, or wanted but unable
//! to land anywhere.
//!
//! Health and liveness only exist on the node running (or trying to run) a
//! container, so this reaches every node the same way `clouduler logs` and
//! `clouduler stack restart` do: the internal api, HMAC-signed over the
//! shared cluster key rather than a session, one cluster member asking
//! another about its own local docker state.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::ClusterState;
use cl_raft::Assignment;
use cl_secret::ClusterKey;
use cl_update::internal_client::AUTH_HEADER;
use futures::StreamExt;
use serde::Deserialize;

use crate::stack::client_for;

pub struct ShowFlags {
    pub node: bool,
    pub uptime: bool,
    pub placement: bool,
    pub logs: Option<u32>,
}

/// One instance worth showing: either an assignment whose reported state or
/// health is bad, or a desired instance that has no assignment at all.
struct Row {
    instance: String,
    node: Option<String>,
    state: String,
    health: String,
    started_at: Option<String>,
    reason: Option<String>,
}

impl Row {
    fn unhealthy(&self) -> bool {
        self.reason.is_some()
            || self.health == "unhealthy"
            || matches!(self.state.as_str(), "exited" | "restarting" | "unknown" | "pending_placement")
    }
}

pub async fn run(cfg: &ClusterConfig, via: Option<&str>, flags: ShowFlags) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    let key = Arc::new(ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {} to authenticate to the node(s) running this",
            cfg.secrets.key_file.display()
        )
    })?);
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the http client")?;

    let mut rows: BTreeMap<String, Vec<Row>> = BTreeMap::new();

    let fetched = futures::future::join_all(
        state
            .assignments
            .values()
            .map(|a| row_for_assignment(cfg, &http, &key, &state, a)),
    )
    .await;
    for (stack, row) in fetched {
        rows.entry(stack).or_default().push(row);
    }

    for (stack, row) in pending_rows(cfg, &state) {
        rows.entry(stack).or_default().push(row);
    }

    let mut any = false;
    for (stack, mut instances) in rows {
        instances.retain(Row::unhealthy);
        if instances.is_empty() {
            continue;
        }
        any = true;
        instances.sort_by(|a, b| a.instance.cmp(&b.instance));
        println!("{stack}\t{} unhealthy instance(s)", instances.len());
        for row in &instances {
            print_row(row, &flags);
            if let Some(n) = flags.logs {
                if n > 0 {
                    if let Some(node) = &row.node {
                        print_log_tail(cfg, &http, &key, node, &row.instance, n).await;
                    }
                }
            }
        }
    }

    if !any {
        println!("no unhealthy instances");
    }
    Ok(())
}

fn print_row(row: &Row, flags: &ShowFlags) {
    let mut line = format!("  {}\tstate={}\thealth={}", row.instance, row.state, row.health);
    if flags.node {
        line.push_str(&format!("\tnode={}", row.node.as_deref().unwrap_or("-")));
    }
    if flags.uptime {
        line.push_str(&format!("\tuptime={}", uptime_of(row.started_at.as_deref())));
    }
    if flags.placement {
        if let Some(reason) = &row.reason {
            line.push_str(&format!("\treason={reason}"));
        }
    }
    println!("{line}");
}

fn uptime_of(started_at: Option<&str>) -> String {
    let Some(started_at) = started_at else {
        return "-".to_string();
    };
    let Ok(started) = humantime::parse_rfc3339(started_at) else {
        return "-".to_string();
    };
    match SystemTime::now().duration_since(started) {
        Ok(elapsed) => humantime::format_duration(Duration::from_secs(elapsed.as_secs())).to_string(),
        Err(_) => "-".to_string(),
    }
}

/// The subset of `cl-api`'s `InstanceSummary` this command needs, deserialized
/// straight off the same internal endpoint `clouduler logs`/`exec` use to
/// reach another node's docker.
#[derive(Deserialize)]
struct InstanceSummary {
    state: String,
    health: String,
    started_at: Option<String>,
}

async fn row_for_assignment(
    cfg: &ClusterConfig,
    http: &reqwest::Client,
    key: &ClusterKey,
    state: &ClusterState,
    assignment: &Assignment,
) -> (String, Row) {
    let stack = assignment.instance.stack.clone();
    let instance = assignment.instance.to_string();
    let node = assignment.node.clone();

    if let Some(migration) = &assignment.migration {
        if let cl_raft::state::MigrationPhase::Failed { reason } = &migration.phase {
            return (
                stack,
                Row {
                    instance,
                    node: Some(node),
                    state: "migration_failed".to_string(),
                    health: "none".to_string(),
                    started_at: None,
                    reason: Some(reason.clone()),
                },
            );
        }
    }

    if state.unreachable.contains(&node) {
        return (
            stack,
            Row {
                instance,
                node: Some(node),
                state: "unknown".to_string(),
                health: "none".to_string(),
                started_at: None,
                reason: Some("node is unreachable".to_string()),
            },
        );
    }

    match fetch_summary(cfg, http, key, &node, &assignment.instance).await {
        Ok(summary) => (
            stack,
            Row {
                instance,
                node: Some(node),
                state: summary.state,
                health: summary.health,
                started_at: summary.started_at,
                reason: None,
            },
        ),
        Err(e) => (
            stack,
            Row {
                instance,
                node: Some(node),
                state: "unknown".to_string(),
                health: "none".to_string(),
                started_at: None,
                reason: Some(e.to_string()),
            },
        ),
    }
}

fn internal_summary_path(instance: &cl_raft::ids::InstanceId) -> String {
    format!("/api/instances/internal/{instance}/summary")
}

async fn fetch_summary(
    cfg: &ClusterConfig,
    http: &reqwest::Client,
    key: &ClusterKey,
    node: &str,
    instance: &cl_raft::ids::InstanceId,
) -> Result<InstanceSummary> {
    let base = cl_update::peers::base_url(cfg, node).ok_or_else(|| anyhow!("node {node} has no address on the internal network"))?;
    let path = internal_summary_path(instance);
    let signature = cl_secret::internal_auth::sign(key, &path);

    let resp = http
        .get(format!("{base}{path}"))
        .header(AUTH_HEADER, signature)
        .send()
        .await
        .with_context(|| format!("asking {node} about {instance}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("{node} answered {}", resp.status());
    }
    resp.json().await.with_context(|| format!("parsing {node}'s answer about {instance}"))
}

/// Instances every deployed stack wants but that have no assignment at all,
/// because no node currently satisfies what they need. Mirrors `cl-api`'s
/// `pending_placements`, against this node's own copy of the cluster config
/// rather than the replicated one, the same trade `stack deploy` already
/// makes when it validates locally before submitting.
fn pending_rows(cfg: &ClusterConfig, state: &ClusterState) -> Vec<(String, Row)> {
    let colocate_proxy_hosts = cfg.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        state,
        &cfg.egress_networks(),
        &cfg.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = cfg.served_drives();

    cl_sched::place(state, &desired, &served_drives)
        .unplaceable
        .into_iter()
        .map(|u| {
            (
                u.instance.stack.clone(),
                Row {
                    instance: u.instance.to_string(),
                    node: None,
                    state: "pending_placement".to_string(),
                    health: "none".to_string(),
                    started_at: None,
                    reason: Some(u.reason),
                },
            )
        })
        .collect()
}

fn internal_logs_path(instance: &str) -> String {
    format!("/api/instances/internal/{instance}/logs")
}

/// Best-effort: a log fetch failing must not stop the rest of the report from
/// printing, so this prints its own error line rather than returning one.
async fn print_log_tail(cfg: &ClusterConfig, http: &reqwest::Client, key: &ClusterKey, node: &str, instance: &str, tail: u32) {
    if let Err(e) = fetch_log_tail(cfg, http, key, node, instance, tail).await {
        println!("    (could not fetch logs: {e})");
    }
}

async fn fetch_log_tail(
    cfg: &ClusterConfig,
    http: &reqwest::Client,
    key: &ClusterKey,
    node: &str,
    instance: &str,
    tail: u32,
) -> Result<()> {
    let base = cl_update::peers::base_url(cfg, node).ok_or_else(|| anyhow!("node {node} has no address on the internal network"))?;
    let path = format!("{}?follow=false&tail={tail}", internal_logs_path(instance));
    let signature = cl_secret::internal_auth::sign(key, &path);

    let resp = http
        .get(format!("{base}{path}"))
        .header(AUTH_HEADER, signature)
        .send()
        .await
        .with_context(|| format!("asking {node} for {instance}'s logs"))?;
    if !resp.status().is_success() {
        anyhow::bail!("{node} answered {}", resp.status());
    }

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
                if event == "stdout" || event == "stderr" {
                    let text = serde_json::from_str::<LogPayload>(data).map(|p| p.text).unwrap_or_else(|_| data.to_string());
                    println!("    | {text}");
                }
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct LogPayload {
    text: String,
}
