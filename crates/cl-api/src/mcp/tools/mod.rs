//! The MCP tool catalog: names, argument schemas, who may call what, and the
//! dispatcher. Each tool is a plain async function returning text; the one
//! place that redacts, bounds and audits results is `mcp::finish`, not here.
//!
//! Descriptions and parameter docs are the AI's only manual for these tools,
//! so they say what a call does, what it refuses, and what to do next.

use std::sync::{Arc, OnceLock};

use cl_raft::InstanceId;
use rmcp::handler::server::common::schema_for_type;
use rmcp::model::{JsonObject, Tool};
use rmcp::schemars::{self, JsonSchema};
use serde::de::DeserializeOwned;

use super::redact::Redactor;
use super::session::Sessions;
use crate::auth::oauth::{McpCaller, McpGrant};
use crate::error::ApiError;
use crate::routes::instances::{self, InstanceSummary};
use crate::server::AppState;

mod exec;
mod read;
mod secrets;
mod stack;

/// Everything a tool call needs.
pub struct Ctx {
    pub state: AppState,
    pub caller: McpCaller,
    /// Key of the session focus lives under.
    pub session: String,
    /// Who the focus belongs to; see `Sessions`.
    pub owner: String,
    pub sessions: Arc<Sessions>,
    /// Applied to every result by `mcp::finish`; tools use it to refuse
    /// input that already contains a secret.
    pub redactor: Arc<Redactor>,
}

pub type ToolResult = Result<String, String>;

pub(super) fn api(e: ApiError) -> String {
    e.to_string()
}

impl Ctx {
    pub fn focus(&self) -> Option<String> {
        self.sessions.focus(&self.session, &self.owner)
    }

    pub fn set_focus(&self, stack: Option<String>) {
        self.sessions.set_focus(&self.session, &self.owner, stack);
    }

    pub fn require_focus(&self) -> Result<String, String> {
        self.focus().ok_or_else(|| {
            "No stack is focused. Call focus_stack with the stack's name first; writes, exec and \
             read_file only work on the focused stack."
                .to_string()
        })
    }

    /// The focused stack, and an error unless `stack` is that stack.
    pub fn require_in_focus(&self, stack: &str, what: &str) -> Result<String, String> {
        let focus = self.require_focus()?;
        if focus == stack {
            Ok(focus)
        } else {
            Err(format!(
                "{what} belongs to stack {stack}, but the focused stack is {focus}. Call \
                 focus_stack with {stack} first if you mean to work on it."
            ))
        }
    }
}

// ---- argument parsing ----

pub(super) fn parse<P: DeserializeOwned>(args: JsonObject) -> Result<P, String> {
    serde_json::from_value(serde_json::Value::Object(args))
        .map_err(|e| format!("invalid arguments: {e}"))
}

#[derive(serde::Deserialize, JsonSchema)]
pub struct NoArgs {}

pub(super) fn parse_instance(raw: &str) -> Result<InstanceId, String> {
    raw.parse::<InstanceId>()
        .map_err(|_| format!("{raw:?} is not an instance id; ids look like stack.service.0"))
}

// ---- instance helpers shared by several tools ----

/// Resolves `target` to instances. A full id (`stack.service.N`) names one
/// instance in any stack; a bare service name means every replica of that
/// service in the focused stack.
pub(super) async fn resolve_target(ctx: &Ctx, target: &str) -> Result<Vec<InstanceId>, String> {
    let state = ctx.state.cluster.state().await;
    if let Ok(id) = target.parse::<InstanceId>() {
        if !state.assignments.contains_key(&id) {
            return Err(format!("no instance {id}"));
        }
        return Ok(vec![id]);
    }
    let stack = ctx.focus().ok_or_else(|| {
        format!(
            "{target:?} is not an instance id (stack.service.N), and no stack is focused to \
             resolve a service name against. Pass a full instance id, or call focus_stack first."
        )
    })?;
    let ids: Vec<InstanceId> = state
        .assignments
        .keys()
        .filter(|i| i.stack == stack && i.service == target)
        .cloned()
        .collect();
    if ids.is_empty() {
        let services: std::collections::BTreeSet<&str> = state
            .assignments
            .keys()
            .filter(|i| i.stack == stack)
            .map(|i| i.service.as_str())
            .collect();
        return Err(format!(
            "stack {stack} has no service {target:?}. Services with instances: {}",
            if services.is_empty() {
                "none (is it deployed?)".to_string()
            } else {
                services.into_iter().collect::<Vec<_>>().join(", ")
            }
        ));
    }
    Ok(ids)
}

/// Resolves `target` for a tool that acts inside containers (exec,
/// read_file), which may only touch the focused stack. The scope check comes
/// before the existence check so the error teaches the AI to focus rather
/// than leaking which instances exist elsewhere.
pub(super) async fn resolve_in_focus(ctx: &Ctx, target: &str) -> Result<InstanceId, String> {
    ctx.require_focus()?;
    if let Ok(id) = target.parse::<InstanceId>() {
        ctx.require_in_focus(&id.stack, &format!("instance {id}"))?;
    }
    let ids = resolve_target(ctx, target).await?;
    first_running(ctx, &ids).await
}

/// The first replica that is actually running, for tools that act on one
/// container.
pub(super) async fn first_running(ctx: &Ctx, ids: &[InstanceId]) -> Result<InstanceId, String> {
    if ids.len() == 1 {
        return Ok(ids[0].clone());
    }
    let state = ctx.state.cluster.state().await;
    let mut seen = Vec::new();
    for id in ids {
        let Some(assignment) = state.assignments.get(id) else {
            continue;
        };
        let summary = instances::summarize(&ctx.state, &state, assignment).await;
        if summary.state == "running" {
            return Ok(id.clone());
        }
        seen.push(format!("{id}={}", summary.state));
    }
    Err(format!("no running replica; states: {}", seen.join(", ")))
}

pub(super) fn instance_line(s: &InstanceSummary) -> String {
    let mut line = format!(
        "{}  node={}  desired={}  state={}  health={}",
        s.id, s.node, s.desired, s.state, s.health
    );
    if let Some(image) = &s.image {
        line.push_str(&format!("  image={image}"));
    }
    if let Some(started) = &s.started_at {
        line.push_str(&format!("  started={started}"));
    }
    if let Some(reason) = &s.reason {
        line.push_str(&format!("  progress={reason}"));
    }
    if let Some(m) = &s.migration {
        line.push_str(&format!("  migrating_to={} ({})", m.to, m.phase));
    }
    if let Some(e) = &s.placement_error {
        line.push_str(&format!("  placement_error={e}"));
    }
    if let Some(p) = &s.proxy {
        match &p.error {
            Some(e) => line.push_str(&format!("  proxy={}: FAILING: {e}", p.host)),
            None => line.push_str(&format!("  proxy={}: ok", p.host)),
        }
    }
    line
}

// ---- the catalog ----

pub struct ToolSpec {
    pub tool: Tool,
    /// Only callable (and listed) for an admin grant.
    pub admin: bool,
}

fn spec<P: JsonSchema + 'static>(
    name: &'static str,
    admin: bool,
    description: &'static str,
) -> ToolSpec {
    ToolSpec {
        tool: Tool::new(name, description, schema_for_type::<P>()),
        admin,
    }
}

pub fn catalog() -> &'static [ToolSpec] {
    static CATALOG: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        vec![
            spec::<stack::FocusStackArgs>(
                "focus_stack",
                false,
                "Set the stack you are working on for this session. The stack need not exist yet, so \
                 use this before creating a new one. Focus scopes reports (list_problems, \
                 cluster_overview, list_instances show full detail only for the focused stack) and is \
                 REQUIRED for every write, exec and read_file. Returns the stack's spec summary and \
                 health.",
            ),
            spec::<NoArgs>("get_focus", false, "Show which stack is focused in this session."),
            spec::<NoArgs>(
                "clear_focus",
                false,
                "Stop focusing a stack. Reports then cover the whole cluster; writes and exec are refused until you focus again.",
            ),
            spec::<NoArgs>(
                "cluster_overview",
                false,
                "Cluster health at a glance: leader and quorum, nodes (availability, instance counts), \
                 stack and instance counts, and faults (certificates, DNS, backups, updates). With a \
                 stack focused, shows that stack in detail plus a count of problems in other stacks.",
            ),
            spec::<NoArgs>(
                "list_problems",
                false,
                "Everything wrong right now: unhealthy, crashed, restart-looping or unreachable \
                 instances, instances that cannot be placed (with the reason), failed or stuck \
                 updates, proxy health-check failures, failed backups, certificate and DNS errors. \
                 Filtered to the focused stack, with a one-line count for problems elsewhere. Start \
                 here when something is not working.",
            ),
            spec::<NoArgs>("list_stacks", false, "List deployed stacks with their revision and instance counts."),
            spec::<stack::GetStackArgs>(
                "get_stack",
                false,
                "A stack's spec (YAML), revision, requirements, the configs and secrets it uses, and \
                 the state of each instance. Defaults to the focused stack.",
            ),
            spec::<stack::ListInstancesArgs>(
                "list_instances",
                false,
                "List instances with node, desired/actual state, health, image, and why one is pending \
                 or failing. With a stack focused and no `stack` argument, lists the focused stack and \
                 counts the rest.",
            ),
            spec::<stack::GetInstanceArgs>("get_instance", false, "Full detail for one instance (state, health, node, image digests, placement error, proxy health)."),
            spec::<exec::GetLogsArgs>(
                "get_logs",
                false,
                "Recent container logs (stdout and stderr, oldest first) of an instance, or of every \
                 replica of a service in the focused stack. Secrets are redacted. Use `grep` to \
                 narrow output, `since_seconds` to skip old lines.",
            ),
            spec::<NoArgs>("list_nodes", false, "Nodes with role, availability, reachability, drives, networks, open ports and instance counts. Needed to write valid placement constraints."),
            spec::<NoArgs>(
                "get_cluster_config",
                false,
                "The cluster configuration: networks (kind, subnet, domain), drives, domains and the web/proxy settings. Read this before writing a stack so networks, drives and hostnames are valid.",
            ),
            spec::<NoArgs>("list_configs", false, "List cluster configs (name, size, revision, owning stack)."),
            spec::<secrets::NameArgs>("get_config", false, "Read a cluster config's content as text. Secret values embedded in it are redacted."),
            spec::<NoArgs>("list_secrets", false, "List secret names with revision and owner. Never returns values."),
            spec::<secrets::NameArgs>(
                "inspect_secret",
                false,
                "Facts about a secret WITHOUT its value: exists, revision, owner, which stacks/services \
                 use it and as which env var or file, byte and character length, valid UTF-8, \
                 leading/trailing whitespace or newline, character classes present. Use it to check a \
                 secret is shaped the way a service expects (e.g. no trailing newline).",
            ),
            spec::<stack::ValidateArgs>(
                "validate_stack",
                false,
                "Check a stack spec against the cluster (networks, drives, constraints) without deploying. Returns errors with their location, or the requirements the stack would place on the cluster. Always run this before deploy_stack.",
            ),
            spec::<stack::DeployArgs>(
                "deploy_stack",
                true,
                "Deploy (create or update) the FOCUSED stack from a YAML spec, with optional config \
                 files. There is deliberately no way to pass secrets: reference a secret that already \
                 exists, or create it first with generate_secret. Refuses specs/configs that contain \
                 a secret's plaintext. After deploying, waits up to wait_seconds and reports instance \
                 states; follow up with list_problems and get_logs.",
            ),
            spec::<stack::RemoveStackArgs>("remove_stack", true, "Remove the focused stack and stop all its instances. `confirm` must equal the focused stack's name."),
            spec::<stack::InstanceArgs>("restart_instance", true, "Restart an instance of the focused stack (the scheduler re-places it, possibly on another node)."),
            spec::<stack::InstanceArgs>("update_instance", true, "Pull the instance's image again and recreate its container, for an instance of the focused stack."),
            spec::<secrets::PutConfigArgs>(
                "put_config",
                true,
                "Create or replace a cluster config (text) so a stack can mount it. Requires a \
                 focused stack. Refuses reserved names and configs owned by another stack.",
            ),
            spec::<secrets::GenerateSecretArgs>(
                "generate_secret",
                true,
                "Generate a random secret on the server, seal and store it, and return only its \
                 facts (never the value). Use this instead of asking the user for a password. \
                 Refuses to replace an existing secret unless overwrite=true, because rotating a \
                 live database password breaks the service using it.",
            ),
            spec::<exec::ExecArgs>(
                "exec",
                true,
                "Run a command inside a container of the FOCUSED stack, without a tty. `target` is an \
                 instance id or a service name (uses its first running replica). `cmd` is an argv \
                 array, not a shell string: use [\"sh\",\"-c\",\"...\"] for pipes. stdout and stderr \
                 are returned separately (each capped at 64 KiB) with the exit code. Output is \
                 redacted of secret values.",
            ),
            spec::<exec::ReadFileArgs>(
                "read_file",
                true,
                "Read a file or list a directory in a container of the FOCUSED stack. Works in \
                 images with no shell. Binary files report size and type only. Refuses secret mount \
                 paths and /run/secrets.",
            ),
        ]
    })
}

/// Whether `name` is a known tool, and if so whether it needs admin.
pub fn is_admin_tool(name: &str) -> Option<bool> {
    catalog()
        .iter()
        .find(|s| s.tool.name == name)
        .map(|s| s.admin)
}

/// Whether the redaction layer may be skipped for a tool: only those that
/// return nothing but the focus name.
pub fn is_focus_only(name: &str) -> bool {
    matches!(name, "get_focus" | "clear_focus")
}

/// Runs a tool. `Err` is a tool-level failure the AI should read.
pub async fn call(ctx: &Ctx, name: &str, args: JsonObject) -> ToolResult {
    let admin = is_admin_tool(name).ok_or_else(|| format!("unknown tool {name:?}"))?;
    if admin && ctx.caller.grant != McpGrant::Admin {
        return Err(format!(
            "{name} needs an admin grant; this connection is read-only. Reconnect and approve \
             the admin grant to use it."
        ));
    }
    match name {
        "focus_stack" => stack::focus_stack(ctx, parse(args)?).await,
        "get_focus" => stack::get_focus(ctx),
        "clear_focus" => stack::clear_focus(ctx),
        "cluster_overview" => read::cluster_overview(ctx).await,
        "list_problems" => read::list_problems(ctx).await,
        "list_stacks" => read::list_stacks(ctx).await,
        "get_stack" => stack::get_stack(ctx, parse(args)?).await,
        "list_instances" => stack::list_instances(ctx, parse(args)?).await,
        "get_instance" => stack::get_instance(ctx, parse(args)?).await,
        "get_logs" => exec::get_logs(ctx, parse(args)?).await,
        "list_nodes" => read::list_nodes(ctx).await,
        "get_cluster_config" => read::get_cluster_config(ctx).await,
        "list_configs" => secrets::list_configs(ctx).await,
        "get_config" => secrets::get_config(ctx, parse(args)?).await,
        "list_secrets" => secrets::list_secrets(ctx).await,
        "inspect_secret" => secrets::inspect_secret(ctx, parse(args)?).await,
        "validate_stack" => stack::validate_stack(ctx, parse(args)?).await,
        "deploy_stack" => stack::deploy_stack(ctx, parse(args)?).await,
        "remove_stack" => stack::remove_stack(ctx, parse(args)?).await,
        "restart_instance" => stack::restart_instance(ctx, parse(args)?).await,
        "update_instance" => stack::update_instance(ctx, parse(args)?).await,
        "put_config" => secrets::put_config(ctx, parse(args)?).await,
        "generate_secret" => secrets::generate_secret(ctx, parse(args)?).await,
        "exec" => exec::exec(ctx, parse(args)?).await,
        "read_file" => exec::read_file(ctx, parse(args)?).await,
        other => Err(format!("unknown tool {other:?}")),
    }
}

/// A short, non-sensitive description of what a call targets, for the audit
/// log. Redacted by the caller before logging.
pub fn audit_target(name: &str, args: &JsonObject) -> String {
    let text = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let mut target = ["target", "id", "name", "stack", "confirm"]
        .iter()
        .find_map(|k| text(k))
        .unwrap_or_default();
    if name == "exec" {
        if let Some(cmd) = args.get("cmd").and_then(|v| v.as_array()) {
            let words: Vec<&str> = cmd.iter().filter_map(|w| w.as_str()).collect();
            target = format!("{target}: {}", words.join(" "));
        }
    }
    if name == "read_file" {
        if let Some(p) = text("path") {
            target = format!("{target}: {p}");
        }
    }
    target
}
