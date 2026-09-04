//! A thin wrapper around the `restic` binary: build the environment/args,
//! run it, and parse just enough of its `--json` output for what the runner
//! needs (a snapshot id and byte count, or the parsed error).

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::destination::ResolvedRepo;
use crate::error::{Result, ResticError};

/// One restic repository, ready to invoke commands against.
pub struct ResticRepo {
    pub repository: String,
    /// Extra args every invocation gets, e.g. an `Ssh` destination's
    /// non-default-port `sftp.command` (see `destination::ssh_command_args`).
    pub extra_args: Vec<String>,
    /// Env every invocation gets: `RESTIC_PASSWORD_FILE` (always) plus
    /// whatever `ResolvedRepo::extra_env` and backend credentials the caller
    /// resolved. Never holds the password itself - only the path to the temp
    /// file the caller wrote it to, so this struct's own `Debug` can't leak
    /// it.
    env: HashMap<String, String>,
}

impl ResticRepo {
    /// `password_file` must already exist and hold the repo password -
    /// callers write it from a sealed secret and clean it up afterward (see
    /// `cl-secret` and the crate doc comment on why this is a file rather
    /// than an env var: restic itself needs one or the other, and a file is
    /// the one that doesn't show up in a process listing).
    pub fn new(resolved: ResolvedRepo, extra_args: Vec<String>, password_file: &Path, credentials: &[(String, String)]) -> Self {
        let mut env: HashMap<String, String> = resolved.extra_env.into_iter().collect();
        for (key, value) in credentials {
            env.insert(key.clone(), value.clone());
        }
        env.insert("RESTIC_PASSWORD_FILE".to_string(), password_file.display().to_string());
        Self { repository: resolved.repository, extra_args, env }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("restic");
        cmd.arg("-r").arg(&self.repository);
        for arg in &self.extra_args {
            cmd.arg(arg);
        }
        cmd.args(args);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd
    }

    /// Whether the repository has already been initialized, checked with the
    /// cheapest read restic offers rather than parsing `init`'s failure text.
    pub fn is_initialized(&self) -> bool {
        self.command(&["cat", "config"]).output().is_ok_and(|o| o.status.success())
    }

    /// Only meaningful when `is_initialized` said no - restic's own `init`
    /// on an already-initialized repo fails.
    pub fn init(&self) -> Result<()> {
        run(self.command(&["init"]))
    }

    pub fn backup(&self, paths: &[&Path], tags: &[&str]) -> Result<BackupOutcome> {
        let mut args: Vec<String> = vec!["backup".into(), "--json".into()];
        for tag in tags {
            args.push("--tag".into());
            args.push((*tag).to_string());
        }
        for path in paths {
            args.push(path.display().to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.command(&arg_refs).output().map_err(ResticError::Io)?;
        if !output.status.success() {
            return Err(ResticError::Failed(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        parse_backup_summary(&output.stdout)
    }

    /// No-op when `retention_args` is empty - see `retention::forget_args`'s
    /// doc comment on why an unset policy means "keep forever", not "prune
    /// with no policy" (which restic itself refuses to do anyway).
    pub fn forget(&self, retention_args: &[String]) -> Result<()> {
        if retention_args.is_empty() {
            return Ok(());
        }
        let mut args: Vec<String> = vec!["forget".into(), "--prune".into()];
        args.extend(retention_args.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(self.command(&arg_refs))
    }

    /// `include` scopes the restore to one path within the snapshot (a
    /// volume subdirectory); `None` restores everything, for a whole-drive
    /// restore.
    pub fn restore(&self, snapshot: &str, target_dir: &Path, include: Option<&str>) -> Result<()> {
        let mut args: Vec<String> = vec![
            "restore".into(),
            snapshot.into(),
            "--target".into(),
            target_dir.display().to_string(),
        ];
        if let Some(include) = include {
            args.push("--include".into());
            args.push(include.into());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(self.command(&arg_refs))
    }

    pub fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        let output = self.command(&["snapshots", "--json"]).output().map_err(ResticError::Io)?;
        if !output.status.success() {
            return Err(ResticError::Failed(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
        serde_json::from_slice(&output.stdout).map_err(|e| ResticError::Parse(e.to_string()))
    }
}

fn run(mut cmd: Command) -> Result<()> {
    let output = cmd.output().map_err(ResticError::Io)?;
    if !output.status.success() {
        return Err(ResticError::Failed(String::from_utf8_lossy(&output.stderr).into_owned()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupOutcome {
    pub snapshot_id: String,
    pub bytes: u64,
}

#[derive(Deserialize)]
struct BackupJsonLine {
    #[serde(default)]
    message_type: String,
    #[serde(default)]
    snapshot_id: Option<String>,
    #[serde(default)]
    total_bytes_processed: Option<u64>,
}

/// `restic backup --json` prints one json object per line; the last one is
/// the summary (`message_type: "summary"`), carrying the snapshot id and
/// byte count this crate needs. Progress lines (`message_type: "status"`)
/// are ignored here - live progress is streamed separately by the runner
/// (see the crate doc comment / `cl-api`'s progress plumbing), not parsed
/// from the final captured output.
fn parse_backup_summary(stdout: &[u8]) -> Result<BackupOutcome> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines().rev() {
        let Ok(parsed) = serde_json::from_str::<BackupJsonLine>(line) else {
            continue;
        };
        if parsed.message_type == "summary" {
            let snapshot_id = parsed
                .snapshot_id
                .ok_or_else(|| ResticError::Parse("summary line has no snapshot_id".into()))?;
            return Ok(BackupOutcome { snapshot_id, bytes: parsed.total_bytes_processed.unwrap_or(0) });
        }
    }
    Err(ResticError::Parse("no summary line in restic backup output".into()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub id: String,
    pub time: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_line_is_found_among_progress_lines() {
        let stdout = concat!(
            r#"{"message_type":"status","percent_done":0.1}"#,
            "\n",
            r#"{"message_type":"status","percent_done":0.9}"#,
            "\n",
            r#"{"message_type":"summary","snapshot_id":"abc123","total_bytes_processed":4096}"#,
            "\n",
        );
        let outcome = parse_backup_summary(stdout.as_bytes()).expect("parses");
        assert_eq!(outcome, BackupOutcome { snapshot_id: "abc123".into(), bytes: 4096 });
    }

    #[test]
    fn no_summary_line_is_an_error() {
        let stdout = concat!(r#"{"message_type":"status","percent_done":0.1}"#, "\n");
        assert!(parse_backup_summary(stdout.as_bytes()).is_err());
    }

    #[test]
    fn a_summary_line_missing_a_snapshot_id_is_an_error() {
        let stdout = concat!(r#"{"message_type":"summary","total_bytes_processed":0}"#, "\n");
        assert!(parse_backup_summary(stdout.as_bytes()).is_err());
    }
}
