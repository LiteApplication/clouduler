//! Tools that look inside containers: logs, exec and file reads.

use std::fmt::Write;

use cl_config::{StackFile, SECRET_DIR};
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use super::{api, resolve_in_focus, resolve_target, Ctx, ToolResult};
use crate::mcp::problems::unix_now;
use crate::routes::instances::{self, ExecRunRequest};

const DEFAULT_LOG_TAIL: u32 = 200;
const MAX_LOG_TAIL: u32 = 2000;
const EXEC_OUTPUT_CAP: usize = 64 * 1024;
const DEFAULT_EXEC_TIMEOUT: u64 = 30;
const MAX_EXEC_TIMEOUT: u64 = 300;
const DEFAULT_FILE_BYTES: usize = 64 * 1024;
const MAX_FILE_BYTES: usize = 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
pub struct GetLogsArgs {
    /// An instance id (`stack.service.index`, any stack) or a service name in the focused stack (all its replicas).
    pub target: String,
    /// How many recent lines per instance (default 200, max 2000).
    #[serde(default)]
    pub tail: Option<u32>,
    /// Only lines from the last N seconds.
    #[serde(default)]
    pub since_seconds: Option<u64>,
    /// Keep only lines containing this text (case-insensitive). Applied to the fetched tail, so raise `tail` when searching far back.
    #[serde(default)]
    pub grep: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// An instance id in the focused stack, or one of its service names (runs in the first running replica).
    pub target: String,
    /// The command as an argv array, e.g. `["ls","-la","/app"]`. There is no shell: use `["sh","-c","a | b"]` for pipes.
    pub cmd: Vec<String>,
    /// User to run as (default: the container's user).
    #[serde(default)]
    pub user: Option<String>,
    /// Text written to the command's stdin, which is then closed.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Seconds before the command is abandoned (default 30, max 300).
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// An instance id in the focused stack, or one of its service names.
    pub target: String,
    /// Absolute path inside the container. A directory returns its entries.
    pub path: String,
    /// Maximum bytes of content to return (default 65536, max 1048576).
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

pub(super) async fn get_logs(ctx: &Ctx, args: GetLogsArgs) -> ToolResult {
    let ids = resolve_target(ctx, &args.target).await?;
    let tail = args.tail.unwrap_or(DEFAULT_LOG_TAIL).clamp(1, MAX_LOG_TAIL);
    let since = args
        .since_seconds
        .map(|s| unix_now() - i64::try_from(s).unwrap_or(i64::MAX / 2));
    let needle = args.grep.as_deref().map(str::to_lowercase);
    let many = ids.len() > 1;

    let mut out = String::new();
    for id in &ids {
        let lines = match instances::fetch_logs(&ctx.state, id, tail, since).await {
            Ok(lines) => lines,
            Err(e) => {
                let _ = writeln!(out, "[{id}] could not fetch logs: {}", api(e));
                continue;
            }
        };
        let total = lines.len();
        let mut shown = 0;
        for l in lines {
            if let Some(n) = &needle {
                if !l.line.to_lowercase().contains(n) {
                    continue;
                }
            }
            shown += 1;
            let ts = l.timestamp.as_deref().unwrap_or("");
            let tag = if l.stream == "stderr" { " stderr" } else { "" };
            if many {
                let _ = writeln!(out, "[{id}]{tag} {ts} {}", l.line);
            } else {
                let _ = writeln!(out, "{ts}{tag} {}", l.line);
            }
        }
        if needle.is_some() {
            let _ = writeln!(out, "-- {id}: {shown} of {total} fetched lines matched");
        } else if total == 0 {
            let _ = writeln!(out, "-- {id}: no log lines");
        }
    }
    Ok(out)
}

pub(super) async fn exec(ctx: &Ctx, args: ExecArgs) -> ToolResult {
    if args.cmd.is_empty() || args.cmd[0].is_empty() {
        return Err("cmd must be a non-empty argv array, e.g. [\"ls\",\"-la\"]".into());
    }
    let id = resolve_in_focus(ctx, &args.target).await?;
    let req = ExecRunRequest {
        cmd: args.cmd,
        user: args.user,
        stdin: args.stdin,
        timeout_secs: args
            .timeout_s
            .unwrap_or(DEFAULT_EXEC_TIMEOUT)
            .clamp(1, MAX_EXEC_TIMEOUT),
        max_output: EXEC_OUTPUT_CAP,
    };
    let r = instances::exec_run(&ctx.state, &id, req)
        .await
        .map_err(api)?;
    let mut out = format!("instance {id}\n");
    match (r.timed_out, r.exit_code) {
        (true, _) => out.push_str("TIMED OUT before the command finished\n"),
        (false, Some(code)) => {
            let _ = writeln!(out, "exit code: {code}");
        }
        (false, None) => out.push_str("exit code: unknown\n"),
    }
    let _ = writeln!(
        out,
        "--- stdout{} ---\n{}",
        if r.stdout_truncated {
            " (truncated)"
        } else {
            ""
        },
        r.stdout
    );
    let _ = writeln!(
        out,
        "--- stderr{} ---\n{}",
        if r.stderr_truncated {
            " (truncated)"
        } else {
            ""
        },
        r.stderr
    );
    Ok(out)
}

/// Resolves `.` and `..` and repeated slashes without touching a filesystem,
/// so a path cannot dodge the secret-path check by spelling.
pub(super) fn normalize_path(path: &str) -> Option<String> {
    if !path.starts_with('/') || path.contains('\0') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

fn is_under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Why `path` may not be read from this service, if it may not.
fn refused_path(path: &str, secret_targets: &[String]) -> Option<String> {
    if is_under(path, SECRET_DIR) || is_under(path, "/var/run/secrets") {
        return Some(format!(
            "{path} is under {SECRET_DIR}, where secrets are mounted"
        ));
    }
    secret_targets
        .iter()
        .find(|t| is_under(path, t))
        .map(|t| format!("{path} is where a secret is mounted ({t})"))
}

fn refusal(path: &str, why: &str) -> String {
    format!(
        "refusing to read {path}: {why}. Secret values are never shown; use inspect_secret to \
         check a secret's shape."
    )
}

pub(super) async fn read_file(ctx: &Ctx, args: ReadFileArgs) -> ToolResult {
    // The mount point of every secret is known without looking at the
    // service, so refuse it before anything else: the answer must not depend
    // on which instances exist.
    let path = normalize_path(&args.path)
        .ok_or_else(|| format!("path must be absolute, got {:?}", args.path))?;
    if let Some(why) = refused_path(&path, &[]) {
        return Err(refusal(&path, &why));
    }
    let id = resolve_in_focus(ctx, &args.target).await?;

    let state = ctx.state.cluster.state().await;
    let secret_targets: Vec<String> = state
        .stacks
        .get(&id.stack)
        .and_then(|s| StackFile::parse(&s.spec).ok())
        .and_then(|f| f.services.get(&id.service).cloned())
        .map(|svc| {
            svc.secrets
                .iter()
                .filter(|r| !r.is_env())
                .filter_map(|r| normalize_path(&r.secret_target().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(why) = refused_path(&path, &secret_targets) {
        return Err(refusal(&path, &why));
    }

    let max = args
        .max_bytes
        .unwrap_or(DEFAULT_FILE_BYTES)
        .min(MAX_FILE_BYTES);
    let f = instances::read_file(&ctx.state, &id, &path, Some(max))
        .await
        .map_err(api)?;
    let mut out = format!(
        "{} in {id}: {} ({} bytes{})\n",
        f.path,
        f.kind,
        f.size,
        if f.truncated { ", truncated" } else { "" }
    );
    if f.kind == "dir" {
        for e in &f.entries {
            let _ = writeln!(out, "{e}");
        }
    } else if f.is_binary {
        out.push_str("binary file; content not shown\n");
    } else if let Some(content) = &f.content {
        out.push_str(content);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_normalized_lexically() {
        assert_eq!(normalize_path("/a/./b//c").as_deref(), Some("/a/b/c"));
        assert_eq!(
            normalize_path("/a/b/../../run/secrets/x").as_deref(),
            Some("/run/secrets/x")
        );
        assert_eq!(normalize_path("/../etc").as_deref(), Some("/etc"));
        assert_eq!(normalize_path("relative"), None);
    }

    #[test]
    fn secret_locations_are_refused_however_they_are_spelled() {
        let targets = vec!["/etc/app/db_password".to_string()];
        for p in [
            "/run/secrets",
            "/run/secrets/db",
            "/var/run/secrets/x",
            "/etc/app/db_password",
        ] {
            assert!(refused_path(p, &targets).is_some(), "{p}");
        }
        let normalized = normalize_path("/run/./secrets/../secrets/db").unwrap();
        assert!(refused_path(&normalized, &targets).is_some());
        for p in ["/etc/app", "/etc/app/config.toml", "/run/secretsx", "/run"] {
            assert!(refused_path(p, &targets).is_none(), "{p}");
        }
    }
}
