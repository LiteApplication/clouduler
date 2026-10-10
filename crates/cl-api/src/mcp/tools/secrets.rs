//! Config and secret tools. None of these can return a secret's value:
//! `inspect_secret` and `generate_secret` report facts about it, nothing more.

use std::fmt::Write;

use cl_config::StackFile;
use cl_raft::state::MAX_OBJECT_BYTES;
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;
use zeroize::Zeroizing;

use super::{api, Ctx, ToolResult};
use crate::routes::objects::{is_reserved, is_reserved_secret};
use crate::secrets::{self, GenerateOptions, Symbols};

#[derive(Deserialize, JsonSchema)]
pub struct NameArgs {
    /// The config or secret's name.
    pub name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct PutConfigArgs {
    /// Config name. It is mounted into a service by this name from the stack's `configs:` list.
    pub name: String,
    /// The file content, as text.
    pub content: String,
}

/// Which symbols a generated secret may contain.
#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SymbolsArg {
    /// `true` allows the default safe symbol set, `false` allows none.
    Flag(bool),
    /// Exactly these characters, e.g. `"-_"`.
    Custom(String),
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, JsonSchema)]
pub struct GenerateSecretArgs {
    /// Secret name, as referenced from a stack's `secrets:` list. Letters, digits, `.`, `_`, `-`.
    pub name: String,
    /// Length in characters (1 to 4096).
    pub length: usize,
    /// Include a-z (default true).
    #[serde(default = "yes")]
    pub lowercase: bool,
    /// Include A-Z (default true).
    #[serde(default = "yes")]
    pub uppercase: bool,
    /// Include 0-9 (default true).
    #[serde(default = "yes")]
    pub digits: bool,
    /// `false` (default) for none, `true` for the default safe symbols, or a string of exactly the symbols allowed, e.g. `"-_"`. Prefer none or a small set for values embedded in URLs or connection strings.
    #[serde(default)]
    pub symbols: Option<SymbolsArg>,
    /// Leave out easily confused characters `0O1lI|` (default false).
    #[serde(default)]
    pub exclude_ambiguous: bool,
    /// Retry until every selected class appears at least once (default true).
    #[serde(default = "yes")]
    pub require_each_class: bool,
    /// Replace an existing secret of this name (default false). Rotating a secret a running service already uses (a database password!) breaks that service.
    #[serde(default)]
    pub overwrite: bool,
}

fn valid_object_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a valid name: use 1 to 128 letters, digits, '.', '_' or '-'"
        ))
    }
}

pub(super) async fn list_configs(ctx: &Ctx) -> ToolResult {
    let state = ctx.state.cluster.state().await;
    let mut out = String::new();
    for c in state.configs.values().filter(|c| !is_reserved(&c.name)) {
        let _ = writeln!(
            out,
            "{}  size={}  revision={}  owner={}",
            c.name,
            c.len(),
            c.revision,
            c.owner.as_deref().unwrap_or("-")
        );
    }
    if out.is_empty() {
        out.push_str("no configs");
    }
    Ok(out)
}

pub(super) async fn get_config(ctx: &Ctx, args: NameArgs) -> ToolResult {
    let state = ctx.state.cluster.state().await;
    let entry = state
        .configs
        .get(&args.name)
        .filter(|_| !is_reserved(&args.name))
        .ok_or_else(|| format!("no config {}", args.name))?;
    let bytes = entry
        .decode()
        .map_err(|e| format!("config is corrupt: {e}"))?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(e) => Ok(format!(
            "config {} is binary ({} bytes); not shown",
            args.name,
            e.as_bytes().len()
        )),
    }
}

pub(super) async fn list_secrets(ctx: &Ctx) -> ToolResult {
    let state = ctx.state.cluster.state().await;
    let mut out = String::new();
    for s in state
        .secrets
        .values()
        .filter(|s| !is_reserved_secret(&s.name))
    {
        let _ = writeln!(
            out,
            "{}  revision={}  owner={}",
            s.name,
            s.revision,
            s.owner.as_deref().unwrap_or("-")
        );
    }
    if out.is_empty() {
        out.push_str("no secrets");
    }
    Ok(out)
}

/// Where each stack mounts `secret`, so the AI can tell what depends on it.
fn used_by(state: &cl_raft::ClusterState, secret: &str) -> Vec<String> {
    let mut out = Vec::new();
    for stack in state.stacks.values() {
        let Ok(file) = StackFile::parse(&stack.spec) else {
            continue;
        };
        for (service, def) in &file.services {
            for r in def.secrets.iter().filter(|r| r.source == secret) {
                let how = match &r.env {
                    Some(var) => format!("env {var}"),
                    None => format!("file {}", r.secret_target().display()),
                };
                out.push(format!("{}/{service} as {how}", stack.name));
            }
        }
    }
    out
}

pub(super) async fn inspect_secret(ctx: &Ctx, args: NameArgs) -> ToolResult {
    let state = ctx.state.cluster.state().await;
    // A reserved secret is the cluster's own credential; to the AI it does not exist.
    let entry = state
        .secrets
        .get(&args.name)
        .filter(|_| !is_reserved_secret(&args.name))
        .ok_or_else(|| format!("no secret {}", args.name))?;
    let key = ctx
        .state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| "this node holds no cluster key".to_string())?;
    if entry.key_fingerprint != key.fingerprint() {
        return Err(
            "the secret was sealed under a different cluster key and cannot be inspected here"
                .into(),
        );
    }
    let plaintext = Zeroizing::new(
        Sealed::from_blob(entry.sealed.clone())
            .open(&key, &args.name)
            .map_err(|e| format!("could not open the secret: {e}"))?,
    );
    let facts = secrets::facts(&plaintext);
    let report = serde_json::json!({
        "name": args.name,
        "exists": true,
        "revision": entry.revision,
        "owner": entry.owner,
        "used_by": used_by(&state, &args.name),
        "facts": facts,
        "note": "facts only; the value is never shown",
    });
    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
}

pub(super) async fn put_config(ctx: &Ctx, args: PutConfigArgs) -> ToolResult {
    let focus = ctx.require_focus()?;
    valid_object_name(&args.name)?;
    if is_reserved(&args.name) {
        return Err(format!("{} is a reserved name", args.name));
    }
    if ctx.redactor.redact(&args.content) != args.content {
        return Err(
            "refusing to store this config: it contains a secret's plaintext. Reference the \
             secret from the stack instead of copying its value."
                .into(),
        );
    }
    if args.content.len() > MAX_OBJECT_BYTES {
        return Err(format!("config is over the {MAX_OBJECT_BYTES} byte limit"));
    }
    let state = ctx.state.cluster.state().await;
    let owner = state.configs.get(&args.name).and_then(|c| c.owner.clone());
    if let Some(o) = owner.as_deref().filter(|o| *o != focus) {
        return Err(format!(
            "config {} is owned by stack {o}, not the focused stack {focus}; focus {o} to change it",
            args.name
        ));
    }
    let response = ctx
        .state
        .cluster
        .submit(ClusterRequest::PutConfig {
            name: args.name.clone(),
            content: cl_raft::base64_encode(args.content.as_bytes()),
            owner,
        })
        .await
        .map_err(|e| api(crate::error::ApiError::not_leader(e)))?;
    if let Some(msg) = response.error {
        return Err(msg);
    }
    Ok(format!(
        "stored config {} ({} bytes). Mount it from a service's `configs:` and deploy_stack.",
        args.name,
        args.content.len()
    ))
}

pub(super) async fn generate_secret(ctx: &Ctx, args: GenerateSecretArgs) -> ToolResult {
    let focus = ctx.require_focus()?;
    valid_object_name(&args.name)?;
    if is_reserved_secret(&args.name) {
        return Err(format!("{} is a reserved name", args.name));
    }
    let state = ctx.state.cluster.state().await;
    let existing = state.secrets.get(&args.name);
    if existing.is_some() && !args.overwrite {
        return Err(format!(
            "secret {} already exists. Use it as is (inspect_secret shows its shape), or pass \
             overwrite=true only if nothing running depends on the old value: a rotated \
             database password locks the service out.",
            args.name
        ));
    }
    if let Some(o) = existing
        .and_then(|e| e.owner.as_deref())
        .filter(|o| *o != focus)
    {
        return Err(format!(
            "secret {} is owned by stack {o}, not the focused stack {focus}",
            args.name
        ));
    }
    let owner = existing.and_then(|e| e.owner.clone());

    let symbols = match args.symbols {
        Some(SymbolsArg::Flag(true)) => Symbols::Default,
        Some(SymbolsArg::Flag(false)) | None => Symbols::None,
        Some(SymbolsArg::Custom(set)) => Symbols::Custom(set),
    };
    let plaintext = Zeroizing::new(
        secrets::generate_with(&GenerateOptions {
            length: args.length,
            lowercase: args.lowercase,
            uppercase: args.uppercase,
            digits: args.digits,
            symbols,
            exclude_ambiguous: args.exclude_ambiguous,
            require_each_class: args.require_each_class,
        })
        .map_err(|e| e.to_string())?,
    );

    let key = ctx
        .state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| "this node holds no cluster key".to_string())?;
    let sealed = Sealed::seal(&key, &args.name, &plaintext).map_err(|e| e.to_string())?;
    let response = ctx
        .state
        .cluster
        .submit(ClusterRequest::PutSecret {
            name: args.name.clone(),
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner,
        })
        .await
        .map_err(|e| api(crate::error::ApiError::not_leader(e)))?;
    if let Some(msg) = response.error {
        return Err(msg);
    }
    let report = serde_json::json!({
        "name": args.name,
        "generated": true,
        "facts": secrets::facts(&plaintext),
        "note": "the value is stored sealed and is never shown; reference it from a service's `secrets:` list",
    });
    serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
}
