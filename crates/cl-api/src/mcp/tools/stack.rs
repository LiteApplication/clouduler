//! Focus, stack and instance tools.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::{Duration, Instant};

use cl_config::StackFile;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use super::{api, instance_line, parse_instance, Ctx, ToolResult};
use crate::mcp::problems::{self, Problem};
use crate::routes::instances::{self, InstanceSummary};
use crate::routes::stacks;

const MAX_WAIT_SECS: u64 = 120;
const DEFAULT_WAIT_SECS: u64 = 20;
const POLL_EVERY: Duration = Duration::from_secs(2);

#[derive(Deserialize, JsonSchema)]
pub struct FocusStackArgs {
    /// Stack name (a DNS-style label such as `blog`). May be a stack that does not exist yet.
    pub name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetStackArgs {
    /// Stack name. Defaults to the focused stack.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListInstancesArgs {
    /// Only this stack's instances. Defaults to the focused stack (or every stack when none is focused).
    #[serde(default)]
    pub stack: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetInstanceArgs {
    /// Instance id, `stack.service.index`, e.g. `blog.web.0`.
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct InstanceArgs {
    /// Instance id, `stack.service.index`. Must belong to the focused stack.
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ValidateArgs {
    /// The stack file, as YAML text.
    pub spec: String,
    /// Stack name to validate under. Defaults to the focused stack.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeployArgs {
    /// The stack file, as YAML text. Secrets are referenced by name, never inlined.
    pub spec: String,
    /// Config file contents the stack mounts, as `{config name: text}`. A config that already exists and is not listed keeps its value.
    #[serde(default)]
    pub configs: Option<BTreeMap<String, String>>,
    /// How long to wait for instances to come up and report their states (default 20, max 120). 0 returns immediately.
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveStackArgs {
    /// Must equal the focused stack's name, as a guard against removing the wrong one.
    pub confirm: String,
}

// ---- focus ----

pub(super) async fn focus_stack(ctx: &Ctx, args: FocusStackArgs) -> ToolResult {
    stacks::check_stack_name(&ctx.state, &args.name)
        .await
        .map_err(api)?;
    ctx.set_focus(Some(args.name.clone()));
    let mut out = format!("focused stack {}\n", args.name);
    out.push_str(&stack_report(ctx, &args.name).await?);
    Ok(out)
}

pub(super) fn get_focus(ctx: &Ctx) -> ToolResult {
    Ok(match ctx.focus() {
        Some(f) => format!("focused stack: {f}"),
        None => "no stack is focused".to_string(),
    })
}

pub(super) fn clear_focus(ctx: &Ctx) -> ToolResult {
    ctx.set_focus(None);
    Ok("focus cleared".to_string())
}

// ---- reads ----

fn belongs_to(summary: &InstanceSummary, stack: &str) -> bool {
    summary.id.starts_with(&format!("{stack}."))
}

/// Spec summary (when deployed), instance states and problems for one stack.
async fn stack_report(ctx: &Ctx, name: &str) -> ToolResult {
    let state = ctx.state.cluster.state().await;
    let mut out = String::new();
    let Some(stack) = state.stacks.get(name) else {
        out.push_str(
            "this stack does not exist yet. Read clouduler://docs/stacks and call get_cluster_config, \
             write a spec, then validate_stack and deploy_stack.\n",
        );
        return Ok(out);
    };
    let _ = writeln!(out, "revision {}", stack.revision);
    if let Ok(parsed) = StackFile::parse(&stack.spec) {
        for (service, def) in &parsed.services {
            let _ = writeln!(out, "service {service}: image {}", def.image);
        }
    }
    let all = instances::list_all(&ctx.state).await;
    let mine: Vec<InstanceSummary> = all.into_iter().filter(|s| belongs_to(s, name)).collect();
    let _ = writeln!(out, "instances: {}", mine.len());
    for s in &mine {
        let _ = writeln!(out, "  {}", instance_line(s));
    }
    let found: Vec<Problem> = problems::classify(&state, &mine, problems::unix_now())
        .into_iter()
        .filter(|p| p.stack.as_deref() == Some(name))
        .collect();
    out.push_str(&problems::render(&found, Some(name)));
    Ok(out)
}

pub(super) async fn get_stack(ctx: &Ctx, args: GetStackArgs) -> ToolResult {
    let name = match args.name.or_else(|| ctx.focus()) {
        Some(n) => n,
        None => return Err("give a stack name, or call focus_stack first".into()),
    };
    let detail = stacks::stack_detail(&ctx.state, &name).await.map_err(api)?;
    let mut out = String::new();
    let _ = writeln!(out, "stack {} revision {}", detail.name, detail.revision);
    let _ = writeln!(out, "owned configs: {}", detail.configs.join(", "));
    let _ = writeln!(out, "owned secrets: {}", detail.secrets.join(", "));
    let _ = writeln!(out, "uses configs: {}", detail.used_configs.join(", "));
    let _ = writeln!(out, "uses secrets: {}", detail.used_secrets.join(", "));
    let _ = writeln!(
        out,
        "requirements: {}",
        serde_json::to_string(&detail.requirements).unwrap_or_default()
    );
    let _ = writeln!(out, "spec:\n{}", detail.spec);
    out.push_str(&stack_report(ctx, &name).await?);
    Ok(out)
}

pub(super) async fn list_instances(ctx: &Ctx, args: ListInstancesArgs) -> ToolResult {
    let all = instances::list_all(&ctx.state).await;
    let scope = args.stack.or_else(|| ctx.focus());
    let mut out = String::new();
    let (mine, others): (Vec<_>, Vec<_>) = match &scope {
        Some(stack) => all.iter().partition(|s| belongs_to(s, stack)),
        None => (all.iter().collect(), Vec::new()),
    };
    if let Some(stack) = &scope {
        let _ = writeln!(out, "stack {stack}: {} instance(s)", mine.len());
    }
    for s in &mine {
        let _ = writeln!(out, "{}", instance_line(s));
    }
    if !others.is_empty() {
        let bad = others.iter().filter(|s| s.state != "running").count();
        let _ = writeln!(
            out,
            "{} instance(s) in other stacks ({bad} not running) hidden; pass `stack` to list them",
            others.len()
        );
    }
    if mine.is_empty() && others.is_empty() {
        out.push_str("no instances\n");
    }
    Ok(out)
}

pub(super) async fn get_instance(ctx: &Ctx, args: GetInstanceArgs) -> ToolResult {
    let id = parse_instance(&args.id)?;
    let state = ctx.state.cluster.state().await;
    let summary = match state.assignments.get(&id) {
        Some(a) => instances::summarize(&ctx.state, &state, a).await,
        None => instances::list_all(&ctx.state)
            .await
            .into_iter()
            .find(|s| s.id == args.id)
            .ok_or_else(|| format!("no instance {id}"))?,
    };
    let mut out = format!("{}\n", instance_line(&summary));
    if let Some(d) = &summary.image_digest {
        let _ = writeln!(out, "pinned digest: {d}");
    }
    if let Some(d) = &summary.running_digest {
        let _ = writeln!(out, "running digest: {d}");
    }
    let found = problems::classify(&state, std::slice::from_ref(&summary), problems::unix_now());
    for p in found.iter().filter(|p| p.subject == summary.id) {
        let _ = writeln!(out, "{}", p.line());
    }
    Ok(out)
}

pub(super) async fn validate_stack(ctx: &Ctx, args: ValidateArgs) -> ToolResult {
    let name = args.name.or_else(|| ctx.focus()).ok_or_else(|| {
        "give `name`, or call focus_stack first, so the spec is validated under a stack name"
            .to_string()
    })?;
    let response = stacks::validate_stack(&ctx.state, &name, &args.spec).await;
    serde_json::to_string_pretty(&response).map_err(|e| e.to_string())
}

// ---- writes ----

/// Names of the secrets whose values appear in `text`, from the redactor's
/// own markers.
fn leaked_secret_names(redacted: &str) -> Vec<String> {
    let mut names = Vec::new();
    for part in redacted.split("[REDACTED secret:").skip(1) {
        if let Some(name) = part.split(']').next() {
            if !names.contains(&name.to_string()) {
                names.push(name.to_string());
            }
        }
    }
    names
}

pub(super) async fn deploy_stack(ctx: &Ctx, args: DeployArgs) -> ToolResult {
    let name = ctx.require_focus()?;

    let configs = args.configs.unwrap_or_default();
    let mut inputs: Vec<(&str, &str)> = vec![("the spec", args.spec.as_str())];
    inputs.extend(configs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    for (what, text) in inputs {
        let redacted = ctx.redactor.redact(text);
        if redacted != text {
            return Err(format!(
                "refusing to deploy: {what} contains the plaintext of secret(s) {}. Never inline a \
                 secret: create it with generate_secret and reference it from the service's \
                 `secrets:` list.",
                leaked_secret_names(&redacted).join(", ")
            ));
        }
    }

    let configs: BTreeMap<String, Vec<u8>> = configs
        .into_iter()
        .map(|(k, v)| (k, v.into_bytes()))
        .collect();
    stacks::deploy_stack(&ctx.state, &name, args.spec, configs, BTreeMap::new())
        .await
        .map_err(api)?;

    let wait = Duration::from_secs(
        args.wait_seconds
            .unwrap_or(DEFAULT_WAIT_SECS)
            .min(MAX_WAIT_SECS),
    );
    let deadline = Instant::now() + wait;
    let mut settled = false;
    let mine = loop {
        let mine: Vec<InstanceSummary> = instances::list_all(&ctx.state)
            .await
            .into_iter()
            .filter(|s| belongs_to(s, &name))
            .collect();
        settled = settled || is_settled(&mine);
        if settled || Instant::now() + POLL_EVERY > deadline {
            break mine;
        }
        tokio::time::sleep(POLL_EVERY).await;
    };

    let state = ctx.state.cluster.state().await;
    let revision = state.stacks.get(&name).map(|s| s.revision).unwrap_or(0);
    let mut out = format!("deployed stack {name} (revision {revision})\n");
    if settled {
        out.push_str("all instances are up\n");
    } else if wait.is_zero() {
        out.push_str("not waiting for instances (wait_seconds=0)\n");
    } else {
        let _ = writeln!(
            out,
            "instances had not settled after {}s; use list_problems and get_logs to see why",
            wait.as_secs()
        );
    }
    for s in &mine {
        let _ = writeln!(out, "  {}", instance_line(s));
    }
    let found: Vec<Problem> = problems::classify(&state, &mine, problems::unix_now())
        .into_iter()
        .filter(|p| p.stack.as_deref() == Some(name.as_str()))
        .collect();
    if !found.is_empty() {
        out.push_str(&problems::render(&found, Some(&name)));
    }
    Ok(out)
}

/// Every instance that should run is running and not failing its healthcheck.
fn is_settled(mine: &[InstanceSummary]) -> bool {
    !mine.is_empty()
        && mine.iter().all(|s| {
            s.desired != "running"
                || (s.state == "running" && matches!(s.health.as_str(), "healthy" | "none"))
        })
}

pub(super) async fn remove_stack(ctx: &Ctx, args: RemoveStackArgs) -> ToolResult {
    let focus = ctx.require_focus()?;
    if args.confirm != focus {
        return Err(format!(
            "confirm must equal the focused stack's name ({focus}); got {:?}. Only the focused \
             stack can be removed.",
            args.confirm
        ));
    }
    stacks::remove_stack(&ctx.state, focus.clone())
        .await
        .map_err(api)?;
    Ok(format!("removed stack {focus}"))
}

pub(super) async fn restart_instance(ctx: &Ctx, args: InstanceArgs) -> ToolResult {
    let id = parse_instance(&args.id)?;
    ctx.require_in_focus(&id.stack, &format!("instance {id}"))?;
    instances::restart_instance(&ctx.state, id.clone())
        .await
        .map_err(api)?;
    Ok(format!(
        "restarting {id}; check list_problems in a few seconds"
    ))
}

pub(super) async fn update_instance(ctx: &Ctx, args: InstanceArgs) -> ToolResult {
    let id = parse_instance(&args.id)?;
    ctx.require_in_focus(&id.stack, &format!("instance {id}"))?;
    instances::update_instance_guarded(&ctx.state, id.clone())
        .await
        .map_err(api)?;
    Ok(format!(
        "update of {id} started; check list_problems in a few seconds"
    ))
}
