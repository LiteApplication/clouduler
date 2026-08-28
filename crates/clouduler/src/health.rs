//! `clouduler stack unhealthy` — an at-a-glance list of stacks whose actual
//! state doesn't match what's desired: crashed, restart-looping, failing its
//! healthcheck, mid-migration, on an unreachable node, assigned with no
//! container at all, running something other than the pinned image, wanted
//! but unable to land anywhere, or stopped (shown either way, so it's clear
//! the stop was deliberate rather than a crash that happens to look the
//! same). Ends with `cluster_warnings`, for the faults that belong to no
//! single instance.
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
use cl_raft::{Assignment, DesiredState};
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
    desired: DesiredState,
    state: String,
    health: String,
    started_at: Option<String>,
    reason: Option<String>,
    /// The reverse proxy's own active check of this instance's service, when
    /// it publishes a route. A backend that answers docker's health check
    /// but that the proxy can't actually reach (wrong port, app not bound
    /// yet, crashed mid-request) shows up here rather than in `health`.
    proxy_error: Option<String>,
}

impl Row {
    /// Worth showing: either the desired state doesn't match what's actually
    /// running (a stopped stack is always shown too, alongside real
    /// mismatches like a crash or a stuck placement), or something else is
    /// wrong (bad health, a failed migration, an unreachable node, a route
    /// the proxy can't reach its backend for).
    fn unhealthy(&self) -> bool {
        self.reason.is_some()
            || self.proxy_error.is_some()
            || self.health == "unhealthy"
            || self.desired == DesiredState::Stopped
            || matches!(self.state.as_str(), "restarting" | "unknown" | "pending_placement")
            || (self.desired == DesiredState::Running && self.state == "exited")
    }
}

/// Explains a row's state relative to its desired state, so `--placement`
/// can tell a deliberate `stack stop` apart from a crash - both report
/// state=exited, but only one is a problem.
fn describe_state(desired: DesiredState, state: &str) -> Option<String> {
    match (desired, state) {
        (DesiredState::Stopped, "exited" | "stopped") => Some("stopped (stack stop)".to_string()),
        (DesiredState::Stopped, other) => {
            Some(format!("desired state is stopped, but the container is still {other}"))
        }
        (DesiredState::Running, "exited") => Some("exited but desired state is running".to_string()),
        // Assigned to a node, wanted running, and no container exists there
        // at all. Distinct from `exited`, which at least got as far as being
        // created: this is a node that has not acted on its assignment —
        // docker unreachable, an image that will not pull, a reconcile loop
        // that is not running. It reported nothing before, because "stopped"
        // is not a state `unhealthy` used to consider wrong.
        (DesiredState::Running, "stopped") => {
            Some("no container exists on its node, but desired state is running".to_string())
        }
        (DesiredState::Running, "restarting") => {
            Some("restarting - usually a container crashing and being restarted in a loop".to_string())
        }
        _ => None,
    }
}

/// A container that is not running the image the cluster pinned for it.
///
/// Not an update being available — that is the Updates view's question, and
/// answering it needs a registry round trip this command deliberately does
/// not make. This is the purely local disagreement: a pin was recorded and
/// the container was never rebuilt onto it, so the cluster believes it is
/// running bits it is not.
fn describe_pin_drift(summary: &InstanceSummary) -> Option<String> {
    let pinned = summary.image_digest.as_deref()?;
    let running = summary.running_digest.as_deref()?;
    if pinned == running {
        return None;
    }
    Some(format!(
        "running {running}, but the cluster pinned {pinned} - the container was never rebuilt onto the pin"
    ))
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

    let warnings = cluster_warnings(&state);
    if !warnings.is_empty() {
        any = true;
        println!("cluster\t{} warning(s)", warnings.len());
        for warning in &warnings {
            println!("  {warning}");
        }
    }

    if !any {
        println!("no unhealthy instances");
    }
    Ok(())
}

/// Faults that belong to no single instance, and so had nowhere to be
/// reported before.
///
/// All three are conditions an operator finds out about the hard way: a node
/// that has dropped out shows up only as every one of its instances reading
/// `unknown`, and a volume whose replica has silently stopped catching up
/// shows up as nothing at all until the day it is promoted to primary and the
/// data turns out to be old.
fn cluster_warnings(state: &ClusterState) -> Vec<String> {
    let mut warnings = Vec::new();

    for node in &state.unreachable {
        warnings.push(format!("node {node} is unreachable"));
    }

    for (id, volume) in &state.volumes {
        if !state.nodes.contains_key(&volume.primary) {
            warnings.push(format!(
                "volume {id}: its primary {} is not a member of the cluster",
                volume.primary
            ));
        } else if state.unreachable.contains(&volume.primary) {
            warnings.push(format!(
                "volume {id}: its primary {} is unreachable, so nothing can write to it",
                volume.primary
            ));
        }

        for (node, generation) in &volume.replicas {
            if *generation < volume.generation {
                warnings.push(format!(
                    "volume {id}: the replica on {node} is at generation {generation}, {} behind the primary",
                    volume.generation - generation,
                ));
            }
        }
    }

    warnings
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
    if let Some(error) = &row.proxy_error {
        line.push_str(&format!("\tproxy={error}"));
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
    #[serde(default)]
    proxy: Option<ProxyHealthSummary>,
    /// What the cluster has pinned for this instance.
    #[serde(default)]
    image_digest: Option<String>,
    /// What the container is actually running. Differs from `image_digest`
    /// when a pin was recorded but the container was never rebuilt onto it.
    #[serde(default)]
    running_digest: Option<String>,
}

/// Mirrors `cl-api`'s `ProxyHealthSummary`: the reverse proxy's own most
/// recent active check of this instance's service.
#[derive(Deserialize)]
struct ProxyHealthSummary {
    #[serde(default)]
    error: Option<String>,
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
        use cl_raft::state::MigrationPhase;
        match &migration.phase {
            MigrationPhase::Failed { reason } => {
                return (
                    stack,
                    Row {
                        instance,
                        node: Some(node),
                        desired: assignment.desired,
                        state: "migration_failed".to_string(),
                        health: "none".to_string(),
                        started_at: None,
                        reason: Some(reason.clone()),
                        proxy_error: None,
                    },
                );
            }
            // A move that has not finished. This command's own help has
            // always advertised "stuck migrating", but only `Failed` was ever
            // reported: an instance wedged in `Stopping` — the shape of the
            // magev-planner.frontend.0 incident, where the driver skipped the
            // job on every poll forever — showed up as a plain `exited`
            // container, indistinguishable from a crash.
            //
            // A move in progress is normally brief, so seeing one here is not
            // by itself a fault; seeing the same one across successive runs
            // is. There is no timestamp on a `Migration` to tell those apart
            // from a single run, and adding one would mean putting a clock
            // reading into the raft log, so this reports the phase and leaves
            // that judgement to whoever is watching.
            phase if !phase.is_terminal() => {
                return (
                    stack,
                    Row {
                        instance,
                        node: Some(node),
                        desired: assignment.desired,
                        state: "migrating".to_string(),
                        health: "none".to_string(),
                        started_at: None,
                        reason: Some(format!(
                            "moving from {} to {} ({}) - if this phase does not change, the move is stuck",
                            migration.from,
                            migration.to,
                            format!("{phase:?}").to_lowercase(),
                        )),
                        proxy_error: None,
                    },
                );
            }
            _ => {}
        }
    }

    if state.unreachable.contains(&node) {
        return (
            stack,
            Row {
                instance,
                node: Some(node),
                desired: assignment.desired,
                state: "unknown".to_string(),
                health: "none".to_string(),
                started_at: None,
                reason: Some("node is unreachable".to_string()),
                proxy_error: None,
            },
        );
    }

    match fetch_summary(cfg, http, key, &node, &assignment.instance).await {
        Ok(summary) => {
            // State first: a container that is not running at all is the more
            // useful thing to say about it than which image it isn't running.
            let reason =
                describe_state(assignment.desired, &summary.state).or_else(|| describe_pin_drift(&summary));
            let proxy_error = summary.proxy.and_then(|p| p.error);
            (
                stack,
                Row {
                    instance,
                    node: Some(node),
                    desired: assignment.desired,
                    state: summary.state,
                    health: summary.health,
                    started_at: summary.started_at,
                    reason,
                    proxy_error,
                },
            )
        }
        Err(e) => (
            stack,
            Row {
                instance,
                node: Some(node),
                desired: assignment.desired,
                state: "unknown".to_string(),
                health: "none".to_string(),
                started_at: None,
                reason: Some(e.to_string()),
                proxy_error: None,
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

/// Instances every deployed stack wants but that have no running assignment,
/// either because no node currently satisfies what they need, or because a
/// placement decision exists and nothing is stopping it, yet the real
/// scheduler loop still hasn't turned it into an assignment - the reconcile
/// loop stalling after a partial batch (see the mediaserver.jellyfin.0 /
/// docs/clouduler-migration.md incident this was written for) is otherwise
/// invisible: it recomputes as perfectly placeable every time this command
/// runs, so only comparing against `unplaceable` would report "healthy"
/// forever. Mirrors `cl-api`'s `pending_placements`, against this node's own
/// copy of the cluster config rather than the replicated one, the same trade
/// `stack deploy` already makes when it validates locally before submitting.
fn pending_rows(cfg: &ClusterConfig, state: &ClusterState) -> Vec<(String, Row)> {
    let colocate_proxy_hosts = cfg.proxy.dns_publish == cl_config::DnsPublishMode::OwningNode;
    let desired = cl_sched::desired_instances(
        state,
        &cfg.egress_networks(),
        &cfg.port_gated_networks(),
        colocate_proxy_hosts,
    );
    let served_drives = cfg.served_drives();

    let outcome = cl_sched::place(state, &desired, &served_drives, &cfg.nat_networks());

    let unplaceable = outcome.unplaceable.into_iter().map(|u| {
        (
            u.instance.stack.clone(),
            Row {
                instance: u.instance.to_string(),
                node: None,
                desired: DesiredState::Running,
                state: "pending_placement".to_string(),
                health: "none".to_string(),
                started_at: None,
                reason: Some(u.reason),
                proxy_error: None,
            },
        )
    });

    let stuck = outcome.decisions.into_iter().filter_map(|d| match d {
        cl_sched::Decision::Place { instance, node } if !state.assignments.contains_key(&instance) => Some((
            instance.stack.clone(),
            Row {
                instance: instance.to_string(),
                node: Some(node.clone()),
                desired: DesiredState::Running,
                state: "pending_placement".to_string(),
                health: "none".to_string(),
                started_at: None,
                reason: Some(format!(
                    "wants to run on {node} but has no assignment yet - the scheduler decided this but hasn't applied it"
                )),
                proxy_error: None,
            },
        )),
        _ => None,
    });

    unplaceable.chain(stuck).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::ids::VolumeId;


    fn summary(pinned: Option<&str>, running: Option<&str>) -> InstanceSummary {
        InstanceSummary {
            state: "running".into(),
            health: "healthy".into(),
            started_at: None,
            proxy: None,
            image_digest: pinned.map(str::to_string),
            running_digest: running.map(str::to_string),
        }
    }

    fn row_with(reason: Option<String>, state: &str, desired: DesiredState) -> Row {
        Row {
            instance: "app.web.0".into(),
            node: Some("n1".into()),
            desired,
            state: state.into(),
            health: "none".into(),
            started_at: None,
            reason,
            proxy_error: None,
        }
    }

    /// The gap this command had: an instance assigned to a node, wanted
    /// running, with no container there at all reported nothing.
    #[test]
    fn a_wanted_instance_with_no_container_is_unhealthy() {
        let reason = describe_state(DesiredState::Running, "stopped").expect("a reason");
        assert!(reason.contains("no container exists"));
        assert!(row_with(Some(reason), "stopped", DesiredState::Running).unhealthy());
    }

    #[test]
    fn a_deliberate_stop_is_still_reported_as_deliberate() {
        for state in ["exited", "stopped"] {
            let reason = describe_state(DesiredState::Stopped, state).expect("a reason");
            assert_eq!(reason, "stopped (stack stop)", "{state}");
        }
    }

    #[test]
    fn a_healthy_running_instance_has_nothing_to_say() {
        assert_eq!(describe_state(DesiredState::Running, "running"), None);
        assert!(!row_with(None, "running", DesiredState::Running).unhealthy());
    }

    #[test]
    fn a_container_not_on_its_pin_is_reported() {
        let drift = describe_pin_drift(&summary(Some("sha256:pinned"), Some("sha256:actual")));
        let reason = drift.expect("drift between the pin and what is running");
        assert!(reason.contains("sha256:pinned"));
        assert!(reason.contains("sha256:actual"));
    }

    /// Half an answer is not evidence of drift. An image built locally has no
    /// registry digest at all, and reporting every one of those as wrong
    /// would bury the real faults.
    #[test]
    fn an_unknown_digest_on_either_side_is_not_drift() {
        assert_eq!(describe_pin_drift(&summary(Some("sha256:a"), Some("sha256:a"))), None);
        assert_eq!(describe_pin_drift(&summary(Some("sha256:a"), None)), None);
        assert_eq!(describe_pin_drift(&summary(None, Some("sha256:a"))), None);
    }

    /// Built by hand rather than through `apply`: the volume requests all
    /// validate that the primary is a roster member implementing the drive,
    /// and one of these cases is deliberately a primary that is not.
    fn state_with_volume(primary: &str, generation: u64, replica: Option<(&str, u64)>) -> ClusterState {
        let mut state = ClusterState::default();
        let id = VolumeId::new("fast", "data");
        let mut replicas = std::collections::BTreeMap::from([(primary.to_string(), generation)]);
        if let Some((node, at)) = replica {
            replicas.insert(node.to_string(), at);
        }
        state.volumes.insert(
            id.clone(),
            cl_raft::state::VolumeState {
                id,
                generation,
                primary: primary.to_string(),
                replicas,
            },
        );
        state
    }

    #[test]
    fn a_replica_that_has_fallen_behind_is_reported() {
        let state = state_with_volume("n1", 4, Some(("n2", 1)));
        let generation = state.volumes[&VolumeId::new("fast", "data")].generation;
        let warnings = cluster_warnings(&state);
        assert!(
            warnings.iter().any(|w| w.contains("replica on n2") && w.contains("behind")),
            "expected a lag warning for generation {generation}, got {warnings:?}",
        );
    }

    #[test]
    fn a_replica_that_has_caught_up_is_not_reported() {
        let state = state_with_volume("n1", 1, Some(("n2", 1)));
        let lag: Vec<_> = cluster_warnings(&state)
            .into_iter()
            .filter(|w| w.contains("behind"))
            .collect();
        assert!(lag.is_empty(), "{lag:?}");
    }

    /// A volume whose primary is not in the roster at all is the shape a
    /// removed node leaves behind, and nothing else in this command would
    /// mention it: no assignment names that node any more.
    #[test]
    fn a_volume_whose_primary_left_the_cluster_is_reported() {
        let state = state_with_volume("gone", 1, None);
        let warnings = cluster_warnings(&state);
        assert!(
            warnings.iter().any(|w| w.contains("not a member of the cluster")),
            "{warnings:?}",
        );
    }

    #[test]
    fn a_quiet_cluster_warns_about_nothing() {
        assert!(cluster_warnings(&ClusterState::default()).is_empty());
    }
}
