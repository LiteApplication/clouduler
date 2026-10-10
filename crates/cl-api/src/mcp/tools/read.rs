//! Cluster-wide read tools.

use std::fmt::Write;

use cl_raft::ClusterState;

use super::{api, Ctx, ToolResult};
use crate::mcp::problems::{self, Problem};
use crate::routes::instances::{self, InstanceSummary};
use crate::routes::{cluster, nodes};

/// The state, every instance's live summary, and the problems they add up to.
pub(super) async fn gather(ctx: &Ctx) -> (ClusterState, Vec<InstanceSummary>, Vec<Problem>) {
    let state = ctx.state.cluster.state().await;
    let summaries = instances::list_all(&ctx.state).await;
    let found = problems::classify(&state, &summaries, problems::unix_now());
    (state, summaries, found)
}

pub(super) async fn list_problems(ctx: &Ctx) -> ToolResult {
    let (_, _, found) = gather(ctx).await;
    Ok(problems::render(&found, ctx.focus().as_deref()))
}

pub(super) async fn cluster_overview(ctx: &Ctx) -> ToolResult {
    let status = cluster::cluster_status(&ctx.state).await;
    let node_list = nodes::list_nodes(&ctx.state).await;
    let (state, summaries, found) = gather(ctx).await;
    let focus = ctx.focus();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "leader={} quorum={} voters={}/{} managers={}/{} version={} state_version={} (answered by {})",
        status.leader.as_deref().unwrap_or("none"),
        if status.has_quorum { "yes" } else { "LOST" },
        status.reachable,
        status.voters,
        status.managers_reachable,
        status.managers_total,
        status.version,
        status.state_version,
        status.node,
    );
    out.push_str("nodes:\n");
    for n in &node_list {
        let _ = writeln!(
            out,
            "  {}  role={}  availability={}  {}  instances={}",
            n.name,
            n.role,
            n.availability,
            if n.reachable {
                "reachable"
            } else {
                "UNREACHABLE"
            },
            n.container_count
        );
    }
    let running = summaries.iter().filter(|s| s.state == "running").count();
    let _ = writeln!(
        out,
        "stacks: {} deployed, {} instances ({running} running)",
        state.stacks.len(),
        summaries.len()
    );
    match &focus {
        Some(f) => {
            let mine: Vec<&InstanceSummary> = summaries
                .iter()
                .filter(|s| s.id.starts_with(&format!("{f}.")))
                .collect();
            let _ = writeln!(out, "focused stack {f}: {} instance(s)", mine.len());
            for s in mine {
                let _ = writeln!(out, "  {}", super::instance_line(s));
            }
        }
        None => out.push_str("no stack focused (call focus_stack to work on one)\n"),
    }
    out.push_str(&problems::render(&found, focus.as_deref()));
    Ok(out)
}

pub(super) async fn list_stacks(ctx: &Ctx) -> ToolResult {
    let state = ctx.state.cluster.state().await;
    if state.stacks.is_empty() {
        return Ok("no stacks deployed".into());
    }
    let mut out = String::new();
    for stack in state.stacks.values() {
        let count = state
            .assignments
            .keys()
            .filter(|i| i.stack == stack.name)
            .count();
        let _ = writeln!(
            out,
            "{}  revision={}  instances={count}",
            stack.name, stack.revision
        );
    }
    Ok(out)
}

pub(super) async fn list_nodes(ctx: &Ctx) -> ToolResult {
    let summaries = nodes::list_nodes(&ctx.state).await;
    let state = ctx.state.cluster.state().await;
    let mut out = String::new();
    for n in summaries {
        let _ = write!(
            out,
            "{}  role={}  availability={}  {}  instances={}",
            n.name,
            n.role,
            n.availability,
            if n.reachable {
                "reachable"
            } else {
                "UNREACHABLE"
            },
            n.container_count
        );
        if let Some(node) = state.nodes.get(&n.name) {
            let _ = write!(
                out,
                "  arch={}  drives=[{}]  networks=[{}]  egress_networks=[{}]",
                node.arch,
                node.drives.keys().cloned().collect::<Vec<_>>().join(","),
                node.networks.iter().cloned().collect::<Vec<_>>().join(","),
                node.egress_networks
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(","),
            );
            if !node.labels.is_empty() {
                let labels: Vec<String> = node
                    .labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                let _ = write!(out, "  labels=[{}]", labels.join(","));
            }
            if node.last_resort {
                out.push_str("  last_resort");
            }
        }
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("no nodes");
    }
    Ok(out)
}

pub(super) async fn get_cluster_config(ctx: &Ctx) -> ToolResult {
    let cfg = ctx.state.cluster_config.read().await;
    serde_yaml::to_string(&*cfg).map_err(|e| api(crate::error::ApiError::internal(e)))
}
