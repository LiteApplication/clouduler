//! The `clouduler backup` commands: destinations, per-drive schedules,
//! manual runs, history, snapshots, and restore. Talk to a running daemon
//! over the node RPC channel, the same way the `stack`/`secret` commands do.

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::{
    BackupDestination, BackupDestinationKind, BackupRun, BackupRunOutcome, BackupTrigger, DriveBackupTarget, Request,
};
use cl_restic::destination::remote_repo;
use cl_restic::restic::ResticRepo;
use cl_restic::secret_names::{credentials_secret_name, password_secret_name};
use cl_secret::{ClusterKey, Sealed};
use uuid::Uuid;

use cl_daemon::backup_restore;
use cl_daemon::backup_runner::instances_using_drive;
use crate::cli::{BackupDestinationAddArgs, BackupDestinationCommand, BackupDestinationKindArg, BackupTargetCommand};
use cl_init::prompt::Prompt;
use cl_raft::client::client_for;

pub async fn destination(cfg: &ClusterConfig, command: BackupDestinationCommand) -> Result<()> {
    match command {
        BackupDestinationCommand::Add(args) => add_destination(cfg, *args).await,
        BackupDestinationCommand::List { via } => list_destinations(cfg, via.as_deref()).await,
        BackupDestinationCommand::Remove { id, via } => remove_destination(cfg, &id, via.as_deref()).await,
    }
}

pub async fn target(cfg: &ClusterConfig, command: BackupTargetCommand) -> Result<()> {
    match command {
        BackupTargetCommand::Set {
            id,
            drive,
            destination,
            frequency,
            time_of_day,
            keep_last,
            keep_within,
            stop_containers,
            enabled,
            via,
        } => set_target(cfg, id, drive, destination, frequency, time_of_day, keep_last, keep_within, stop_containers, enabled, via.as_deref()).await,
        BackupTargetCommand::List { via } => list_targets(cfg, via.as_deref()).await,
        BackupTargetCommand::Remove { id, via } => remove_target(cfg, &id, via.as_deref()).await,
    }
}

async fn add_destination(cfg: &ClusterConfig, args: BackupDestinationAddArgs) -> Result<()> {
    let kind = match args.kind {
        BackupDestinationKindArg::NodeLocal => BackupDestinationKind::NodeLocal {
            node: args.node.ok_or_else(|| anyhow!("--node is required for --kind node-local"))?,
            path: args.path.ok_or_else(|| anyhow!("--path is required for --kind node-local"))?,
        },
        BackupDestinationKindArg::Ssh => BackupDestinationKind::Ssh {
            host: args.host.ok_or_else(|| anyhow!("--host is required for --kind ssh"))?,
            port: args.port,
            user: args.user.ok_or_else(|| anyhow!("--user is required for --kind ssh"))?,
            path: args.path.ok_or_else(|| anyhow!("--path is required for --kind ssh"))?,
        },
        BackupDestinationKindArg::S3 => BackupDestinationKind::S3 {
            bucket: args.bucket.ok_or_else(|| anyhow!("--bucket is required for --kind s3"))?,
            endpoint: args.endpoint.ok_or_else(|| anyhow!("--endpoint is required for --kind s3"))?,
            region: args.region,
            prefix: args.prefix,
        },
    };

    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "adding a backup destination needs the cluster key at {}. Run this from a node that holds it",
            cfg.secrets.key_file.display()
        )
    })?;
    let client = client_for(cfg, args.via.as_deref())?;

    let password_secret = password_secret_name(&args.id);
    let password = args.password.unwrap_or_else(generate_password);
    seal_and_submit(&client, &key, &password_secret, password.as_bytes()).await?;

    let credentials_secret = match kind {
        BackupDestinationKind::Ssh { .. } => match args.ssh_key_file {
            Some(path) => {
                let pem = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                let name = credentials_secret_name(&args.id);
                seal_and_submit(&client, &key, &name, &pem).await?;
                Some(name)
            }
            None => None,
        },
        BackupDestinationKind::S3 { .. } => match (args.s3_access_key_id, args.s3_secret_access_key) {
            (Some(access_key), Some(secret_key)) => {
                let name = credentials_secret_name(&args.id);
                seal_and_submit(&client, &key, &name, format!("{access_key}\n{secret_key}").as_bytes()).await?;
                Some(name)
            }
            _ => None,
        },
        BackupDestinationKind::NodeLocal { .. } => None,
    };

    let response = client
        .submit(Request::PutBackupDestination(BackupDestination {
            id: args.id.clone(),
            name: args.name,
            kind,
            password_secret,
            credentials_secret,
        }))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected destination {}: {error}", args.id);
    }
    println!("added destination {}", args.id);
    Ok(())
}

async fn seal_and_submit(client: &cl_raft::client::Client, key: &ClusterKey, name: &str, plaintext: &[u8]) -> Result<()> {
    let sealed = Sealed::seal(key, name, plaintext).with_context(|| format!("sealing {name}"))?;
    let response = client
        .submit(Request::PutSecret {
            name: name.to_string(),
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected secret {name}: {error}");
    }
    Ok(())
}

fn generate_password() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

async fn list_destinations(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?.state().await.map_err(|e| anyhow!("{e}"))?;
    if state.restic_destinations.is_empty() {
        println!("no backup destinations");
        return Ok(());
    }
    for (id, d) in &state.restic_destinations {
        let kind = match &d.kind {
            BackupDestinationKind::NodeLocal { node, path } => format!("node-local {node}:{path}"),
            BackupDestinationKind::Ssh { host, port, user, path } => format!("ssh {user}@{host}:{port}{path}"),
            BackupDestinationKind::S3 { bucket, endpoint, .. } => format!("s3 {endpoint}/{bucket}"),
        };
        println!("{id}\t{}\t{kind}", d.name);
    }
    Ok(())
}

async fn remove_destination(cfg: &ClusterConfig, id: &str, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let response = client
        .submit(Request::RemoveBackupDestination { id: id.to_string() })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("could not remove destination {id}: {error}");
    }
    println!("removed destination {id}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn set_target(
    cfg: &ClusterConfig,
    id: String,
    drive: String,
    destination_id: String,
    frequency: String,
    time_of_day: Option<String>,
    keep_last: Option<u32>,
    keep_within: Option<String>,
    stop_containers: Option<bool>,
    enabled: bool,
    via: Option<&str>,
) -> Result<()> {
    let frequency_secs = humantime::parse_duration(&frequency)
        .with_context(|| format!("parsing --frequency {frequency}"))?
        .as_secs();
    let keep_within_secs = keep_within
        .map(|s| humantime::parse_duration(&s).with_context(|| format!("parsing --keep-within {s}")))
        .transpose()?
        .map(|d| d.as_secs());
    if let Some(tod) = &time_of_day {
        let valid = tod
            .split_once(':')
            .and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
            .is_some_and(|(h, m)| h <= 23 && m <= 59);
        if !valid {
            bail!("--time-of-day must be HH:MM, got {tod}");
        }
    }

    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    // Preserve an existing target's last_run across a re-`set`, so editing a
    // schedule doesn't reset "when did this last actually run".
    let last_run = state.restic_targets.get(&id).and_then(|t| t.last_run);

    let response = client
        .submit(Request::PutBackupTarget(DriveBackupTarget {
            id: id.clone(),
            drive,
            destination_id,
            enabled,
            frequency_secs,
            time_of_day,
            keep_last,
            keep_within_secs,
            stop_containers,
            last_run,
        }))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected target {id}: {error}");
    }
    println!("set target {id}");
    Ok(())
}

async fn list_targets(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?.state().await.map_err(|e| anyhow!("{e}"))?;
    if state.restic_targets.is_empty() {
        println!("no backup targets");
        return Ok(());
    }
    for (id, t) in &state.restic_targets {
        let enabled = if t.enabled { "enabled" } else { "disabled" };
        let last_run = t.last_run.map(|s| s.to_string()).unwrap_or_else(|| "never".to_string());
        println!("{id}\tdrive={}\tdestination={}\t{enabled}\tevery {}s\tlast_run={last_run}", t.drive, t.destination_id, t.frequency_secs);
    }
    Ok(())
}

async fn remove_target(cfg: &ClusterConfig, id: &str, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let response = client
        .submit(Request::RemoveBackupTarget { id: id.to_string() })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("could not remove target {id}: {error}");
    }
    println!("removed target {id}");
    Ok(())
}

/// Trigger a run immediately, bypassing the schedule - the daemon's
/// per-node runner picks up `Running` runs on its own poll, the same as a
/// scheduled one; this just skips `schedule::is_due`.
pub async fn run(cfg: &ClusterConfig, id: &str, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let target = state.restic_targets.get(id).ok_or_else(|| anyhow!("unknown backup target {id}"))?;
    if state
        .restic_runs
        .values()
        .any(|r| r.target_id == id && matches!(r.outcome, BackupRunOutcome::Running { .. }))
    {
        bail!("a backup is already running for target {id}");
    }

    let run_id = Uuid::new_v4().to_string();
    let response = client
        .submit(Request::RecordBackupRun(BackupRun {
            id: run_id.clone(),
            target_id: id.to_string(),
            drive: target.drive.clone(),
            destination_id: target.destination_id.clone(),
            started_at: unix_now(),
            finished_at: None,
            outcome: BackupRunOutcome::Running { percent: None },
            triggered_by: BackupTrigger::Manual { by: whoami() },
        }))
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("could not start a run for {id}: {error}");
    }
    println!("started run {run_id} for target {id}");
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "cli".to_string())
}

pub async fn history(cfg: &ClusterConfig, target_id: Option<&str>, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?.state().await.map_err(|e| anyhow!("{e}"))?;
    let mut runs: Vec<&BackupRun> = state
        .restic_runs
        .values()
        .filter(|r| target_id.is_none_or(|t| r.target_id == t))
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));

    if runs.is_empty() {
        println!("no backup runs");
        return Ok(());
    }
    for run in runs {
        let outcome = match &run.outcome {
            BackupRunOutcome::Running { percent: Some(p) } => format!("running ({:.0}%)", p * 100.0),
            BackupRunOutcome::Running { percent: None } => "running".to_string(),
            BackupRunOutcome::Succeeded { snapshot_id, bytes } => format!("succeeded snapshot={snapshot_id} bytes={bytes}"),
            BackupRunOutcome::Failed { error } => format!("failed: {error}"),
        };
        println!("{}\ttarget={}\tdrive={}\tstarted={}\t{outcome}", run.id, run.target_id, run.drive, run.started_at);
    }
    Ok(())
}

pub async fn snapshots(cfg: &ClusterConfig, destination_id: &str, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?.state().await.map_err(|e| anyhow!("{e}"))?;
    let destination = state
        .restic_destinations
        .get(destination_id)
        .ok_or_else(|| anyhow!("unknown backup destination {destination_id}"))?;

    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "listing snapshots needs the cluster key at {}. Run this from a node that holds it",
            cfg.secrets.key_file.display()
        )
    })?;
    let password_secret = &destination.password_secret;
    let password = cl_daemon::backup_secrets::open_secret(&state, &key, password_secret)
        .ok_or_else(|| anyhow!("cannot open the repo password for destination {destination_id}"))?;

    let cred_dir = std::env::temp_dir().join("clouduler-restic-snapshots").join(destination_id);
    std::fs::create_dir_all(&cred_dir)?;
    let password_path = cred_dir.join("password");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&password_path, &password)?;
        std::fs::set_permissions(&password_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let _guard = cl_daemon::backup_secrets::TempFileGuard(password_path.clone());

    let resolved = remote_repo(destination);
    let repo = ResticRepo::new(resolved, cl_restic::destination::ssh_command_args(destination), &password_path, &[]);
    let list = tokio::task::spawn_blocking(move || repo.snapshots())
        .await
        .map_err(|e| anyhow!("the restic task panicked: {e}"))?
        .map_err(|e| anyhow!("listing snapshots: {e}"))?;

    if list.is_empty() {
        println!("no snapshots at {destination_id}");
        return Ok(());
    }
    for snap in list {
        println!("{}\t{}\t{}", snap.id, snap.time, snap.tags.join(","));
    }
    Ok(())
}

pub async fn restore_drive(cfg: &ClusterConfig, node: &str, drive: &str, destination: &str, snapshot: Option<&str>, yes: bool, via: Option<&str>) -> Result<()> {
    let client = client_for(cfg, via)?;
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    let count = instances_using_drive(&state, drive).len();
    if !confirm_restore(yes, &format!("restore drive {drive} from {destination}, stopping {count} instance(s)?"))? {
        bail!("aborted");
    }
    backup_restore::restore_drive(cfg, node, drive, destination, snapshot, via).await
}

pub async fn restore_volume(cfg: &ClusterConfig, node: &str, volume: &cl_raft::ids::VolumeId, destination: &str, snapshot: Option<&str>, yes: bool, via: Option<&str>) -> Result<()> {
    if !confirm_restore(yes, &format!("restore volume {volume} from {destination}?"))? {
        bail!("aborted");
    }
    backup_restore::restore_volume(cfg, node, volume, destination, snapshot, via).await
}

#[allow(clippy::too_many_arguments)]
pub async fn restore_service(
    cfg: &ClusterConfig,
    node: &str,
    stack: &str,
    service: &str,
    destination: Option<&str>,
    snapshot: Option<&str>,
    yes: bool,
    via: Option<&str>,
) -> Result<()> {
    if !confirm_restore(yes, &format!("restore every volume used by {stack}/{service}?"))? {
        bail!("aborted");
    }
    backup_restore::restore_service(cfg, node, stack, service, destination, snapshot, via).await
}

fn confirm_restore(yes: bool, question: &str) -> Result<bool> {
    let mut prompt = Prompt::new(std::io::stdin().lock(), std::io::stdout(), !yes);
    prompt.say("this stops the affected containers and overwrites their data with the restored snapshot")?;
    prompt.confirm(question, yes)
}
