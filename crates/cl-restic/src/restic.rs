//! A thin wrapper around the `restic` binary: build the environment/args,
//! run it, and parse just enough of its `--json` output for what the runner
//! needs (a snapshot id and byte count, or the parsed error).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::destination::ResolvedRepo;
use crate::error::{ResticError, Result};

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
    pub fn new(
        resolved: ResolvedRepo,
        extra_args: Vec<String>,
        password_file: &Path,
        credentials: &[(String, String)],
    ) -> Self {
        let mut env: HashMap<String, String> = resolved.extra_env.into_iter().collect();
        for (key, value) in credentials {
            env.insert(key.clone(), value.clone());
        }
        env.insert(
            "RESTIC_PASSWORD_FILE".to_string(),
            password_file.display().to_string(),
        );
        Self {
            repository: resolved.repository,
            extra_args,
            env,
        }
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
        self.command(&["cat", "config"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Only meaningful when `is_initialized` said no - restic's own `init`
    /// on an already-initialized repo fails.
    pub fn init(&self) -> Result<()> {
        run(self.command(&["init"]))
    }

    /// Runs `restic backup`, calling `on_progress` with restic's own
    /// `percent_done` (0.0-1.0) as each `--json` status line streams in, so a
    /// caller can surface live progress instead of only learning the outcome
    /// once the whole (possibly very long) run finishes.
    pub fn backup(
        &self,
        paths: &[&Path],
        tags: &[&str],
        mut on_progress: impl FnMut(f64),
    ) -> Result<BackupOutcome> {
        let mut args: Vec<String> = vec!["backup".into(), "--json".into()];
        // Larger packs drastically cut index/metadata overhead for big
        // datasets (700 GB+).  Default 4 MiB means millions of tiny packs;
        // 32 MiB brings that to a manageable tens-of-thousands.
        args.push("--pack-size".into());
        args.push("32".into());
        // More disk readers in parallel: the default of 2 leaves NVMe /
        // fast-USB storage underutilised during the scan+read phase.
        args.push("--read-concurrency".into());
        args.push("4".into());
        for tag in tags {
            args.push("--tag".into());
            args.push((*tag).to_string());
        }
        for path in paths {
            args.push(path.display().to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let mut child = self
            .command(&arg_refs)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(ResticError::Io)?;

        // Drained on its own thread rather than after `wait()`: restic can
        // write enough to stderr (warnings alongside a long-running backup)
        // to fill the pipe buffer, which would otherwise deadlock this
        // thread against a restic that's blocked writing to a pipe nobody is
        // reading.
        let stderr = child.stderr.take().expect("stderr piped");
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let mut stderr = stderr;
            let _ = stderr.read_to_string(&mut buf);
            buf
        });

        let stdout = child.stdout.take().expect("stdout piped");
        let mut summary: Option<Result<BackupOutcome>> = None;
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(ResticError::Io)?;
            match parse_backup_line(&line) {
                Some(BackupLine::Status {
                    percent_done: Some(percent),
                }) => on_progress(percent),
                Some(BackupLine::Status { percent_done: None }) => {}
                Some(BackupLine::Summary(result)) => summary = Some(result),
                None => {}
            }
        }

        let status = child.wait().map_err(ResticError::Io)?;
        let stderr_text = stderr_thread.join().unwrap_or_default();
        if !status.success() {
            return Err(ResticError::Failed(stderr_text));
        }
        match summary {
            Some(result) => result,
            None => Err(ResticError::Parse(
                "no summary line in restic backup output".into(),
            )),
        }
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
        let output = self
            .command(&["snapshots", "--json"])
            .output()
            .map_err(ResticError::Io)?;
        if !output.status.success() {
            return Err(ResticError::Failed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|e| ResticError::Parse(e.to_string()))
    }
}

fn run(mut cmd: Command) -> Result<()> {
    let output = cmd.output().map_err(ResticError::Io)?;
    if !output.status.success() {
        return Err(ResticError::Failed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
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
    percent_done: Option<f64>,
    #[serde(default)]
    snapshot_id: Option<String>,
    #[serde(default)]
    total_bytes_processed: Option<u64>,
}

/// One line of `restic backup --json` output, categorized for `backup`'s
/// streaming reader: a `"status"` line carries live progress, a `"summary"`
/// line (always the last one restic prints) carries the final outcome, and
/// everything else (`"verbose_status"`, `"error"`, ...) is ignored here.
enum BackupLine {
    Status { percent_done: Option<f64> },
    Summary(Result<BackupOutcome>),
}

fn parse_backup_line(line: &str) -> Option<BackupLine> {
    let parsed: BackupJsonLine = serde_json::from_str(line).ok()?;
    match parsed.message_type.as_str() {
        "status" => Some(BackupLine::Status {
            percent_done: parsed.percent_done,
        }),
        "summary" => Some(BackupLine::Summary(
            parsed
                .snapshot_id
                .ok_or_else(|| ResticError::Parse("summary line has no snapshot_id".into()))
                .map(|snapshot_id| BackupOutcome {
                    snapshot_id,
                    bytes: parsed.total_bytes_processed.unwrap_or(0),
                }),
        )),
        _ => None,
    }
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
    fn a_status_line_carries_percent_done() {
        let line = r#"{"message_type":"status","percent_done":0.42}"#;
        match parse_backup_line(line) {
            Some(BackupLine::Status {
                percent_done: Some(p),
            }) => assert!((p - 0.42).abs() < 1e-9),
            _ => panic!("expected a status line with percent_done"),
        }
    }

    #[test]
    fn a_summary_line_parses_the_snapshot_id_and_bytes() {
        let line =
            r#"{"message_type":"summary","snapshot_id":"abc123","total_bytes_processed":4096}"#;
        match parse_backup_line(line) {
            Some(BackupLine::Summary(Ok(outcome))) => {
                assert_eq!(
                    outcome,
                    BackupOutcome {
                        snapshot_id: "abc123".into(),
                        bytes: 4096
                    }
                )
            }
            _ => panic!("expected a parsed summary"),
        }
    }

    #[test]
    fn a_summary_line_missing_a_snapshot_id_is_an_error() {
        let line = r#"{"message_type":"summary","total_bytes_processed":0}"#;
        match parse_backup_line(line) {
            Some(BackupLine::Summary(Err(_))) => {}
            _ => panic!("expected a summary parse error"),
        }
    }

    #[test]
    fn an_unrecognized_line_is_ignored() {
        let line = r#"{"message_type":"verbose_status","action":"unchanged"}"#;
        assert!(parse_backup_line(line).is_none());
    }
}
