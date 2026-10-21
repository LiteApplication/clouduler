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
        // Under a service manager the daemon can run with neither $HOME nor
        // $XDG_CACHE_HOME, and restic then refuses to open its cache ("unable
        // to locate cache directory") - which makes `forget --prune` fail and
        // also keeps it from judging a lock stale. Give it a directory.
        let has_cache_home = |var: &str| std::env::var_os(var).is_some_and(|v| !v.is_empty());
        if !env.contains_key("RESTIC_CACHE_DIR")
            && !has_cache_home("RESTIC_CACHE_DIR")
            && !has_cache_home("XDG_CACHE_HOME")
            && !has_cache_home("HOME")
        {
            env.insert(
                "RESTIC_CACHE_DIR".to_string(),
                std::env::temp_dir()
                    .join("clouduler-restic-cache")
                    .display()
                    .to_string(),
            );
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

    /// Makes sure the repository exists, creating it only when it really is
    /// absent. `cat config` is the cheapest read restic offers, but it also
    /// fails for a wrong password, a permissions or I/O problem, so its
    /// failure alone must not trigger `init`: on an existing repo that only
    /// reports "config file already exists" and hides the real cause.
    pub fn ensure_initialized(&self) -> Result<()> {
        let output = self
            .command(&["cat", "config"])
            .output()
            .map_err(ResticError::Io)?;
        if output.status.success() {
            return Ok(());
        }
        let probe = String::from_utf8_lossy(&output.stderr).into_owned();
        if probe_means_unreadable(&probe) {
            return Err(ResticError::Failed(probe));
        }
        match self.init() {
            Ok(()) => Ok(()),
            // Raced another run's `init`, or the probe failed for a reason
            // we don't recognise: either way the probe's text is the useful
            // one, and a genuinely usable repo will let the backup proceed.
            Err(ResticError::Failed(init)) if init.contains("already exists") => {
                Err(ResticError::Failed(probe))
            }
            Err(e) => Err(e),
        }
    }

    fn init(&self) -> Result<()> {
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
        // A crashed earlier run leaves its lock behind and `prune` needs an
        // exclusive one, so it would fail forever. Plain `unlock` only removes
        // locks restic itself deems stale (dead process / not refreshed), never
        // a live run's. Best effort: if it fails, forget reports the real error.
        let _ = run(self.command(&["unlock"]));
        let mut args: Vec<String> = vec!["forget".into(), "--prune".into()];
        args.extend(retention_args.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(self.command(&arg_refs))
    }

    /// Restores `snapshot` into `target_dir`, treating `source_dir` - the
    /// absolute path the snapshot was taken of - as the root of what is
    /// restored.
    ///
    /// restic stores the absolute paths it backed up and, by default,
    /// recreates them under `--target`: restoring the snapshot of
    /// `/srv/drives/fast` into `/srv/drives/fast` would land everything in
    /// `/srv/drives/fast/srv/drives/fast`, and an `--include /vol` filter
    /// would match nothing at all. The `snapshot:subfolder` form roots the
    /// restore at `source_dir` instead, so its contents (`<volume>/...`) land
    /// directly in `target_dir` and `include` is relative to it.
    ///
    /// `include` scopes the restore to one path under `source_dir` (a volume
    /// subdirectory, with a leading `/`); `None` restores everything, for a
    /// whole-drive restore. Files in `target_dir` that the snapshot does not
    /// contain are left alone.
    pub fn restore(
        &self,
        snapshot: &str,
        source_dir: &str,
        target_dir: &Path,
        include: Option<&str>,
    ) -> Result<()> {
        let args = restore_args(snapshot, source_dir, target_dir, include)?;
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

/// The `restic restore` argument vector for `ResticRepo::restore`, separated
/// out so its exact shape is unit-testable without a restic binary.
pub fn restore_args(
    snapshot: &str,
    source_dir: &str,
    target_dir: &Path,
    include: Option<&str>,
) -> Result<Vec<String>> {
    crate::restore::validate_snapshot_id(snapshot).map_err(ResticError::Failed)?;
    if !source_dir.starts_with('/') {
        return Err(ResticError::Failed(format!(
            "the snapshot's source path {source_dir:?} is not absolute"
        )));
    }
    let mut args: Vec<String> = vec![
        "restore".into(),
        "--target".into(),
        target_dir.display().to_string(),
    ];
    if let Some(include) = include {
        args.push("--include".into());
        args.push(include.into());
    }
    // `--` so nothing positional can ever be read as a flag.
    args.push("--".into());
    args.push(format!("{snapshot}:{source_dir}"));
    Ok(args)
}

/// Whether a failed `cat config` says the repository is there but cannot be
/// opened, as opposed to missing.
fn probe_means_unreadable(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    [
        "wrong password",
        "no key found",
        "permission denied",
        "input/output error",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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
    /// The absolute paths the snapshot was taken of - what `restore` needs
    /// as its subfolder root. Not part of the api's snapshot listing.
    #[serde(default, skip_serializing)]
    pub paths: Vec<String>,
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
    fn restore_roots_the_snapshot_at_its_source_path() {
        let args = restore_args(
            "abcd1234",
            "/var/lib/clouduler/drives/fast",
            Path::new("/var/lib/clouduler/drives/fast"),
            Some("/gitlab-data"),
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "restore",
                "--target",
                "/var/lib/clouduler/drives/fast",
                "--include",
                "/gitlab-data",
                "--",
                "abcd1234:/var/lib/clouduler/drives/fast",
            ]
        );
        let whole = restore_args("latest", "/a", Path::new("/t"), None).unwrap();
        assert_eq!(whole, vec!["restore", "--target", "/t", "--", "latest:/a"]);
    }

    #[test]
    fn restore_refuses_a_flag_shaped_snapshot_or_a_relative_source() {
        assert!(restore_args("--help", "/a", Path::new("/t"), None).is_err());
        assert!(restore_args("abcd:/x", "/a", Path::new("/t"), None).is_err());
        assert!(restore_args("abcd1234", "relative", Path::new("/t"), None).is_err());
    }

    #[test]
    fn an_unrecognized_line_is_ignored() {
        let line = r#"{"message_type":"verbose_status","action":"unchanged"}"#;
        assert!(parse_backup_line(line).is_none());
    }
}

#[cfg(test)]
mod probe_tests {
    use super::probe_means_unreadable;

    #[test]
    fn existing_but_unopenable_repos_are_not_reinitialised() {
        assert!(probe_means_unreadable(
            "Fatal: wrong password or no key found"
        ));
        assert!(probe_means_unreadable("open config: Permission denied"));
        assert!(!probe_means_unreadable(
            "Fatal: unable to open config file: no such file or directory\nIs there a repository at the following location?"
        ));
    }
}
