//! `clouduler folder-import`: get a local file or folder into a drive volume.
//!
//! This is a filesystem operation on whichever node runs it, so it only
//! makes sense on a node that actually implements the target drive (for a
//! served drive, that includes a node that mounts it over FUSE: a write there
//! forwards to the real server transparently). The one role it refuses is a
//! served drive's standby: its directory is a sync *target*, and the next
//! push from the active server would delete whatever was written there
//! directly.
//!
//! Once the data is in place this pushes it straight to every other node
//! holding a copy, using the same delta engine and wire protocol the
//! background sync scheduler uses, rather than waiting for the volume's own
//! sync interval.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_drive::{
    drive_port, resolve_relative, DeltaEngine, DriveRegistry, DriveRole, ProgressSink, SyncEngine,
};
use cl_raft::client::Client;
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::state::{ClusterState, DesiredState, Request};
use tracing::debug;

use crate::cli::FolderImportArgs;
use cl_raft::client::client_for;

/// Upper bound on how long to give instances to stop cleanly before
/// touching the volume they mount, on top of each service's own
/// `stop_grace_period`. `wait_for_stopped` polls local docker for instances
/// on this node and the cluster's self-reported `container_status` for
/// everything else, returning as soon as all of them are confirmed stopped
/// rather than always sleeping the full margin.
const STOP_POLL_MARGIN: Duration = Duration::from_secs(15);

/// How often to poll local docker while waiting for instances to stop.
const STOP_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Live one-line progress for a long-running step, redrawn in place with a
/// carriage return. Only active on a real terminal: piped/logged output gets
/// no progress line at all, since overwriting a line makes no sense there.
struct Progress {
    live: bool,
}

impl Progress {
    fn new() -> Self {
        Self {
            live: std::io::stdout().is_terminal(),
        }
    }

    fn update(&self, line: &str) {
        if self.live {
            print!("\r\x1b[K{line}");
            let _ = std::io::stdout().flush();
        }
    }

    /// End the current progress line so subsequent `println!`s start clean.
    fn finish(&self) {
        if self.live {
            println!();
        }
    }
}

/// Tracks bytes/files copied so far, for [`Progress::update`].
#[derive(Default)]
struct CopyProgress {
    files: u64,
    bytes: u64,
}

impl CopyProgress {
    fn file(&mut self, bytes: u64, progress: &Progress) {
        self.files += 1;
        self.bytes += bytes;
        progress.update(&format!(
            "copying: {} file(s), {} transferred",
            self.files,
            human_bytes(self.bytes)
        ));
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// What one poll saw of a single instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    Running,
    /// No answer we can trust: docker errored, or no status was ever reported.
    Unknown,
    /// This node's docker says the container is down or gone.
    StoppedLocal,
    /// The owning node's last `container_status` says it is down. That report
    /// carries no timestamp, so it can predate the stop request.
    StoppedRemote,
}

/// Outcome of one poll, see [`judge`].
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Every instance is positively confirmed stopped.
    Done,
    /// These are still running, of unknown status, or not yet confirmed.
    Waiting(Vec<InstanceId>),
}

/// Decide whether every instance is confirmed stopped, given what this poll
/// observed at `now` (time since the stop was requested).
///
/// A remote `running:false` is only trusted once it has held, unbroken, for a
/// full scheduler sweep: the owning node only notices the desired change and
/// reports `container_status` once per `SWEEP_INTERVAL`, so a shorter
/// observation could be a report from before the stop was requested.
/// `remote_stopped_since` carries that first-seen time between polls.
fn judge(
    observations: &[(InstanceId, Observed)],
    remote_stopped_since: &mut BTreeMap<InstanceId, Duration>,
    now: Duration,
) -> Verdict {
    let mut pending = Vec::new();
    for (instance, observed) in observations {
        if *observed != Observed::StoppedRemote {
            remote_stopped_since.remove(instance);
        }
        let confirmed = match observed {
            Observed::StoppedLocal => true,
            Observed::StoppedRemote => {
                let since = *remote_stopped_since.entry(instance.clone()).or_insert(now);
                now.saturating_sub(since) >= cl_sched::run::SWEEP_INTERVAL
            }
            Observed::Running | Observed::Unknown => false,
        };
        if !confirmed {
            pending.push(instance.clone());
        }
    }
    if pending.is_empty() {
        Verdict::Done
    } else {
        Verdict::Waiting(pending)
    }
}

/// Wait for `instances` to stop. Returns `Ok` only once every one is
/// positively confirmed stopped; a deadline, an unreachable docker or cluster,
/// or an inconclusive status is an error, so the caller never touches a
/// volume something might still be writing to.
///
/// Instances assigned to `node_name` are checked against the local docker
/// daemon directly; instances on other nodes are checked against the
/// cluster's self-reported `container_status` (see
/// `cl_sched::run::Scheduler::report_container_status`), so this returns as
/// soon as the remote node actually confirms its container is down instead
/// of always sleeping the full margin. An instance with no report yet counts
/// as running.
async fn wait_for_stopped(
    client: &Client,
    node_name: &str,
    instances: &[InstanceId],
    deadline: Duration,
) -> Result<()> {
    let docker = cl_docker::Docker::connect().map_err(|e| {
        anyhow!("could not connect to docker to confirm the instances stopped: {e}")
    })?;

    let start = tokio::time::Instant::now();
    let mut remote_stopped_since = BTreeMap::new();
    let mut pending: Vec<InstanceId> = instances.to_vec();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            bail!(
                "timed out after {deadline:?} waiting for {} to stop (still running or status unknown)",
                pending.iter().map(InstanceId::to_string).collect::<Vec<_>>().join(", ")
            );
        }
        // A failed state read leaves `pending` as it was: nothing new is confirmed.
        if let Ok(state) = client.state().await {
            let mut observations = Vec::with_capacity(instances.len());
            for instance in instances {
                let is_local = state
                    .assignments
                    .values()
                    .any(|a| a.node == node_name && &a.instance == instance);
                let observed = if is_local {
                    // An inspect error says nothing about the container, so it
                    // counts as running; only "no such container" is stopped.
                    match docker.inspect(&instance.container_name()).await {
                        Ok(Some(s)) if s.running => Observed::Running,
                        Ok(_) => Observed::StoppedLocal,
                        Err(_) => Observed::Unknown,
                    }
                } else {
                    match state.container_status.get(instance) {
                        Some(status) if status.running => Observed::Running,
                        Some(_) => Observed::StoppedRemote,
                        None => Observed::Unknown,
                    }
                };
                observations.push((instance.clone(), observed));
            }
            match judge(&observations, &mut remote_stopped_since, elapsed) {
                Verdict::Done => return Ok(()),
                Verdict::Waiting(still) => pending = still,
            }
        }
        tokio::time::sleep(STOP_POLL_INTERVAL.min(deadline - elapsed)).await;
    }
}

pub async fn run(cfg: &ClusterConfig, node_name: &str, args: FolderImportArgs) -> Result<()> {
    let (drive, volume, path) = parse_target(&args.target)?;
    debug!(
        drive,
        volume,
        path = path.as_deref().unwrap_or(""),
        "parsed target"
    );
    if args.replace && path.is_some() {
        bail!("--replace takes no sub-path; target must be <drive>/<volume>");
    }
    if !args.source.exists() {
        bail!("{} does not exist", args.source.display());
    }

    let registry = DriveRegistry::new(cfg, node_name)
        .with_context(|| format!("node {node_name} is not in the cluster config"))?;
    if !registry.implements(&drive) {
        bail!(
            "node {node_name} does not implement drive {drive}; run folder-import from a node that does"
        );
    }

    let id = VolumeId::new(drive.clone(), volume.clone());
    let client = client_for(cfg, args.via.as_deref())?;
    // Loaded before anything is stopped or moved: the push at the end needs it,
    // and failing there would leave the instances down.
    let keys = cl_drive::transport::key_watch_from_config(cfg)?;
    debug!(
        via = args.via.as_deref().unwrap_or("<any manager>"),
        "submitting through"
    );
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    if registry.is_served(&drive) && registry.role(&drive, &state.unreachable) == DriveRole::Standby
    {
        bail!(
            "node {node_name} is a standby for drive {drive}; import from the active server or a node that mounts it, not a backup candidate"
        );
    }

    let volume_root = registry
        .ensure_volume(&id)
        .with_context(|| format!("preparing the local path for {id}"))?;
    debug!(root = %volume_root.display(), "volume root");

    let progress = Progress::new();
    let mut copied = CopyProgress::default();

    let mut stopped: Vec<InstanceId> = Vec::new();
    if args.replace {
        let using = instances_using(&state, &id)?;
        if !using.is_empty() {
            if !args.force {
                bail!(
                    "{id} is mounted by {}; pass --force to stop them, replace the volume, and start them back up",
                    using.iter().map(InstanceId::to_string).collect::<Vec<_>>().join(", ")
                );
            }
            let grace = max_stop_grace(&state, &using);
            // The owning node only notices the Stopped request on its next sweep.
            let deadline = grace + STOP_POLL_MARGIN + cl_sched::run::SWEEP_INTERVAL;
            debug!(
                instances = %using.iter().map(InstanceId::to_string).collect::<Vec<_>>().join(", "),
                deadline = ?deadline,
                "stopping instances before replacing the volume"
            );
            set_desired(&client, &using, DesiredState::Stopped).await?;
            wait_for_stopped(&client, node_name, &using, deadline)
                .await
                .map_err(|e| {
                    anyhow!(
                        "{e:#}\nthe instances were left Stopped and the volume {id} was not touched; \
                         retry the import, or bring them back with `clouduler stack start <stack>` ({})",
                        using.iter().map(|i| i.stack.as_str()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", ")
                    )
                })?;
            stopped = using;
        }
        debug!("clearing the volume before replacing it");
        clear_directory(&volume_root)?;
        import_replacing(&args, &volume_root, &mut copied, &progress)?;
    } else {
        let subroot = match &path {
            Some(p) => resolve_relative(&volume_root, p)
                .with_context(|| format!("resolving {p} under {id}"))?,
            None => volume_root.clone(),
        };
        std::fs::create_dir_all(&subroot)
            .with_context(|| format!("creating {}", subroot.display()))?;
        let name = args
            .source
            .file_name()
            .ok_or_else(|| anyhow!("{} has no file name", args.source.display()))?;
        let dest = subroot.join(name);
        if dest.exists() {
            bail!(
                "{} already exists; remove it first, or import to a different path",
                dest.display()
            );
        }
        debug!(dest = %dest.display(), "placing source at");
        apply_method(&args, &args.source, &dest, &mut copied, &progress)?;
    }
    progress.finish();

    let generation = touch_volume(&client, &id, node_name).await?;
    let progress = Progress::new();
    debug!(generation, "volume touched, pushing now");
    let pushed = push_now(PushArgs {
        client: &client,
        registry: &registry,
        state: &state,
        drive: &drive,
        node_name,
        id: &id,
        volume_root: &volume_root,
        generation,
        progress: &progress,
        keys: &keys,
    })
    .await;
    progress.finish();

    if !stopped.is_empty() {
        set_desired(&client, &stopped, DesiredState::Running).await?;
    }

    match &path {
        Some(p) if !args.replace => println!("imported {} into {id}/{p}", args.source.display()),
        _ => println!("imported {} into {id}", args.source.display()),
    }
    if pushed.total == 0 {
        println!("no other node holds this drive; nothing to push");
    } else if pushed.failed.is_empty() {
        println!("pushed to {} node(s)", pushed.total);
    } else {
        println!(
            "pushed to {}/{} node(s); failed: {} (the background sync will retry)",
            pushed.total - pushed.failed.len(),
            pushed.total,
            pushed.failed.join(", ")
        );
    }
    Ok(())
}

/// Split `<drive>/<volume>[/path inside volume]`.
fn parse_target(target: &str) -> Result<(String, String, Option<String>)> {
    let mut parts = target.splitn(3, '/');
    let drive = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("target must look like <drive>/<volume>[/path]"))?;
    let volume = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("target must look like <drive>/<volume>[/path]"))?;
    let path = parts.next().filter(|s| !s.is_empty()).map(str::to_string);
    Ok((drive.to_string(), volume.to_string(), path))
}

/// Instances whose stack mounts `id`. Errors if an assignment's stack is
/// missing or its spec won't parse: whether that instance mounts the volume
/// is then unknowable, and assuming it doesn't risks replacing data under it.
pub fn instances_using(state: &ClusterState, id: &VolumeId) -> Result<Vec<InstanceId>> {
    let mut found = Vec::new();
    let mut unknown = Vec::new();
    for assignment in state.assignments.values() {
        let instance = &assignment.instance;
        let Some(stack) = state.stacks.get(&instance.stack) else {
            unknown.push(format!("{instance} (stack {} is missing)", instance.stack));
            continue;
        };
        let file = match StackFile::parse(&stack.spec) {
            Ok(file) => file,
            Err(e) => {
                unknown.push(format!("{instance} (stack spec does not parse: {e})"));
                continue;
            }
        };
        let Some(service) = file.services.get(&instance.service) else {
            continue;
        };
        let uses = service.volumes.iter().any(|m| {
            m.drive.as_deref() == Some(id.drive.as_str())
                && m.volume.as_deref() == Some(id.volume.as_str())
        });
        if uses {
            found.push(instance.clone());
        }
    }
    if !unknown.is_empty() {
        bail!(
            "cannot tell whether {id} is mounted by: {}; fix or remove those stacks first",
            unknown.join(", ")
        );
    }
    Ok(found)
}

fn max_stop_grace(state: &ClusterState, instances: &[InstanceId]) -> Duration {
    let default = cl_sched::run::configured_stop_grace(state);
    instances
        .iter()
        .filter_map(|i| {
            let stack = state.stacks.get(&i.stack)?;
            let file = StackFile::parse(&stack.spec).ok()?;
            file.services.get(&i.service)?.stop_grace_period
        })
        .max()
        .unwrap_or(default)
}

async fn set_desired(
    client: &Client,
    instances: &[InstanceId],
    desired: DesiredState,
) -> Result<()> {
    for instance in instances {
        let response = client
            .submit(Request::SetDesiredState {
                instance: instance.clone(),
                desired,
            })
            .await
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(error) = response.error {
            bail!("could not set {instance} to {desired:?}: {error}");
        }
    }
    Ok(())
}

/// Record that this node now holds the freshest copy. Creates the volume's
/// raft record first if this is the first time it has been written, then
/// bumps its generation so every other replica is marked stale. Returns the
/// new generation, for the immediate push below.
async fn touch_volume(client: &Client, id: &VolumeId, node_name: &str) -> Result<u64> {
    debug!(%id, primary = node_name, "submitting UpsertVolume");
    let upsert = client
        .submit(Request::UpsertVolume {
            id: id.clone(),
            primary: node_name.to_string(),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = upsert.error {
        bail!("cluster rejected the volume record: {error}");
    }
    debug!(%id, primary = node_name, "submitting TouchVolume");
    let touch = client
        .submit(Request::TouchVolume {
            id: id.clone(),
            primary: node_name.to_string(),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = touch.error {
        bail!("cluster rejected the sync trigger: {error}");
    }
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;
    Ok(state.volumes.get(id).map(|v| v.generation).unwrap_or(1))
}

struct PushResult {
    total: usize,
    failed: Vec<String>,
}

struct PushArgs<'a> {
    client: &'a Client,
    registry: &'a DriveRegistry,
    state: &'a ClusterState,
    drive: &'a str,
    node_name: &'a str,
    id: &'a VolumeId,
    volume_root: &'a Path,
    generation: u64,
    progress: &'a Progress,
    keys: &'a cl_raft::KeyWatch,
}

/// Push the volume straight to every other node holding a copy, instead of
/// waiting for that volume's own sync interval to come around.
async fn push_now(args: PushArgs<'_>) -> PushResult {
    let PushArgs {
        client,
        registry,
        state,
        drive,
        node_name,
        id,
        volume_root,
        generation,
        progress,
        keys,
    } = args;

    let targets: Vec<(String, SocketAddr)> = if registry.is_served(drive) {
        registry
            .served_candidates(drive)
            .iter()
            .filter(|n| n.as_str() != node_name)
            .filter_map(|n| state.nodes.get(n).map(|ns| (n.clone(), ns.rpc_addr)))
            .collect()
    } else {
        state
            .nodes
            .iter()
            .filter(|(name, ns)| name.as_str() != node_name && ns.implements(drive))
            .map(|(name, ns)| (name.clone(), ns.rpc_addr))
            .collect()
    };

    debug!(
        targets = %targets.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "),
        "pushing to"
    );
    let engine = DeltaEngine::new(keys.clone());
    let mut failed = Vec::new();
    for (i, (name, addr)) in targets.iter().enumerate() {
        let peer = SocketAddr::new(addr.ip(), drive_port(addr.port()));
        debug!(node = name.as_str(), %peer, "syncing volume");
        progress.update(&format!("pushing to {name} ({}/{})", i + 1, targets.len()));
        match engine
            .sync(
                volume_root,
                peer,
                id,
                generation,
                None,
                ProgressSink::none(),
            )
            .await
        {
            Ok(_) => {
                debug!(
                    node = name.as_str(),
                    generation, "recording replica generation"
                );
                let response = client
                    .submit(Request::SetVolumeReplica {
                        id: id.clone(),
                        node: name.clone(),
                        generation,
                    })
                    .await;
                if !matches!(response, Ok(r) if r.error.is_none()) {
                    failed.push(name.clone());
                }
            }
            Err(e) => {
                eprintln!("sync to {name} failed: {e}");
                failed.push(name.clone());
            }
        }
    }
    PushResult {
        total: targets.len(),
        failed,
    }
}

fn apply_method(
    args: &FolderImportArgs,
    src: &Path,
    dest: &Path,
    copied: &mut CopyProgress,
    progress: &Progress,
) -> Result<()> {
    if args.r#move {
        move_into(src, dest, copied, progress)
    } else if args.link {
        symlink_into(src, dest)
    } else {
        copy_into(src, dest, copied, progress)
    }
}

/// `--replace`: the volume's directory itself already has its old content
/// cleared, so the source's entries become its entries directly rather than
/// nesting under the source's own name.
fn import_replacing(
    args: &FolderImportArgs,
    volume_root: &Path,
    copied: &mut CopyProgress,
    progress: &Progress,
) -> Result<()> {
    if args.source.is_dir() && !args.source.is_symlink() {
        for entry in std::fs::read_dir(&args.source)
            .with_context(|| format!("reading {}", args.source.display()))?
        {
            let entry = entry?;
            apply_method(
                args,
                &entry.path(),
                &volume_root.join(entry.file_name()),
                copied,
                progress,
            )?;
        }
        if args.r#move {
            // Best effort: the source is empty now, but leaving it behind if
            // this fails (e.g. it wasn't actually empty on some odd fs) is
            // harmless.
            std::fs::remove_dir(&args.source).ok();
        }
    } else {
        let name = args
            .source
            .file_name()
            .ok_or_else(|| anyhow!("{} has no file name", args.source.display()))?;
        apply_method(
            args,
            &args.source,
            &volume_root.join(name),
            copied,
            progress,
        )?;
    }
    Ok(())
}

fn move_into(
    src: &Path,
    dest: &Path,
    copied: &mut CopyProgress,
    progress: &Progress,
) -> Result<()> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        // EXDEV: rename(2) refuses to cross a mount-point boundary even when
        // both sides are actually the same underlying device — this is
        // exactly what a bind mount looks like (e.g. the destination is a
        // FUSE-mounted served drive, or the source is bind-mounted into a
        // container/chroot from the same disk the volume lives on). link(2)
        // has no such restriction, only requiring the same device, so when
        // `same_device` confirms that's the case here, relink the tree
        // instead of paying for a full byte copy.
        Err(e) if e.raw_os_error() == Some(18) => {
            if same_device(src, dest)? {
                hardlink_into(src, dest, copied, progress)?;
            } else {
                copy_into(src, dest, copied, progress)?;
            }
            remove_source(src)
        }
        Err(e) => Err(e).with_context(|| format!("moving {} to {}", src.display(), dest.display())),
    }
}

/// Whether `src` and `dest` live on the same underlying device, even though
/// `rename` refused to cross between them (they're on different mount
/// points, e.g. one or both sides is a bind mount). `dest` doesn't exist
/// yet, so its parent directory is compared instead — a directory always
/// exists on the same device as everything created inside it.
fn same_device(src: &Path, dest: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let src_dev = std::fs::symlink_metadata(src)
        .with_context(|| format!("reading {}", src.display()))?
        .dev();
    let dest_parent = dest
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", dest.display()))?;
    let dest_dev = std::fs::metadata(dest_parent)
        .with_context(|| format!("reading {}", dest_parent.display()))?
        .dev();
    Ok(src_dev == dest_dev)
}

/// Recreate `src`'s tree at `dest` by hardlinking every regular file rather
/// than copying its bytes. The caller checks `same_device` first as a cheap
/// filter, but that check is only a hint: on btrfs, separate subvolumes of
/// the same filesystem report identical `st_dev` yet still refuse to
/// hardlink across the subvolume boundary. So every hardlink here still
/// falls back to a byte copy on `EXDEV`, the same way `move_into` falls back
/// on `rename`, rather than trusting `same_device` to have been right.
fn hardlink_into(
    src: &Path,
    dest: &Path,
    copied: &mut CopyProgress,
    progress: &Progress,
) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if meta.is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dest)
            .with_context(|| format!("recreating symlink at {}", dest.display()))?;
    } else if meta.is_dir() {
        std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
            let entry = entry?;
            hardlink_into(
                &entry.path(),
                &dest.join(entry.file_name()),
                copied,
                progress,
            )?;
        }
    } else {
        match std::fs::hard_link(src, dest) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(18) => {
                std::fs::copy(src, dest)
                    .with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("linking {} to {}", dest.display(), src.display()));
            }
        }
        copied.file(meta.len(), progress);
    }
    Ok(())
}

fn remove_source(src: &Path) -> Result<()> {
    let result = if src.is_dir() && !src.is_symlink() {
        std::fs::remove_dir_all(src)
    } else {
        std::fs::remove_file(src)
    };
    result.with_context(|| format!("removing {} after copying it", src.display()))
}

/// A single symlink at `dest` pointing at the source's absolute path. Not
/// dereferenced or duplicated: the volume just references it in place. Note
/// this is what the sync engine replicates too (same path string, sent as
/// is), so it only resolves on another node if that path exists there.
fn symlink_into(src: &Path, dest: &Path) -> Result<()> {
    let target = src
        .canonicalize()
        .with_context(|| format!("resolving {}", src.display()))?;
    std::os::unix::fs::symlink(&target, dest)
        .with_context(|| format!("linking {} at {}", target.display(), dest.display()))
}

fn copy_into(
    src: &Path,
    dest: &Path,
    copied: &mut CopyProgress,
    progress: &Progress,
) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if meta.is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dest)
            .with_context(|| format!("recreating symlink at {}", dest.display()))?;
    } else if meta.is_dir() {
        std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
            let entry = entry?;
            copy_into(
                &entry.path(),
                &dest.join(entry.file_name()),
                copied,
                progress,
            )?;
        }
    } else {
        std::fs::copy(src, dest)
            .with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
        copied.file(meta.len(), progress);
    }
    Ok(())
}

fn clear_directory(root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        let result = if meta.is_dir() && !meta.file_type().is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        result.with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_target_splits_drive_volume_and_path() {
        assert_eq!(
            parse_target("bulk/imports/sub/dir").unwrap(),
            ("bulk".into(), "imports".into(), Some("sub/dir".into()))
        );
        assert_eq!(
            parse_target("bulk/imports").unwrap(),
            ("bulk".into(), "imports".into(), None)
        );
    }

    #[test]
    fn parse_target_rejects_a_bare_drive() {
        assert!(parse_target("bulk").is_err());
        assert!(parse_target("").is_err());
    }

    #[test]
    fn copy_into_duplicates_a_directory_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("sub/b.txt"), b"world").unwrap();

        let dest = tmp.path().join("dest");
        copy_into(&src, &dest, &mut CopyProgress::default(), &Progress::new()).unwrap();

        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dest.join("sub/b.txt")).unwrap(), b"world");
        // The source is untouched.
        assert!(src.join("a.txt").exists());
    }

    #[test]
    fn copy_into_preserves_a_symlink_rather_than_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("real.txt"), b"data").unwrap();
        let src = tmp.path().join("link");
        std::os::unix::fs::symlink("real.txt", &src).unwrap();

        let dest = tmp.path().join("dest-link");
        copy_into(&src, &dest, &mut CopyProgress::default(), &Progress::new()).unwrap();

        assert_eq!(fs::read_link(&dest).unwrap(), Path::new("real.txt"));
    }

    #[test]
    fn move_into_relocates_a_directory_on_the_same_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();

        let dest = tmp.path().join("dest");
        move_into(&src, &dest, &mut CopyProgress::default(), &Progress::new()).unwrap();

        assert!(!src.exists());
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"hello");
    }

    #[test]
    fn same_device_is_true_for_two_paths_under_the_same_tempdir() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dest).unwrap();
        assert!(same_device(&src, &dest.join("child")).unwrap());
    }

    #[test]
    fn hardlink_into_links_files_instead_of_copying_their_bytes() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("sub/b.txt"), b"world").unwrap();

        let dest = tmp.path().join("dest");
        hardlink_into(&src, &dest, &mut CopyProgress::default(), &Progress::new()).unwrap();

        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"hello");
        assert_eq!(
            fs::metadata(src.join("a.txt")).unwrap().ino(),
            fs::metadata(dest.join("a.txt")).unwrap().ino(),
            "hardlink_into should share the source's inode, not copy its data"
        );
        // The source is untouched; move_into is the one that removes it.
        assert!(src.join("a.txt").exists());
    }

    #[test]
    fn symlink_into_points_at_the_absolute_source_path() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();

        let dest = tmp.path().join("dest-link");
        symlink_into(&src, &dest).unwrap();

        assert_eq!(fs::read_link(&dest).unwrap(), src.canonicalize().unwrap());
        // The source is untouched, unlike a move.
        assert!(src.exists());
    }

    #[test]
    fn clear_directory_empties_a_mix_of_files_dirs_and_symlinks_without_removing_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();

        clear_directory(&root).unwrap();

        assert!(root.exists());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    fn inst(n: u32) -> InstanceId {
        InstanceId::new("app", "web", n)
    }

    #[test]
    fn judge_waits_on_running_and_unknown_instances() {
        let mut since = BTreeMap::new();
        let obs = vec![
            (inst(0), Observed::StoppedLocal),
            (inst(1), Observed::Running),
            (inst(2), Observed::Unknown),
        ];
        assert_eq!(
            judge(&obs, &mut since, Duration::ZERO),
            Verdict::Waiting(vec![inst(1), inst(2)])
        );
    }

    #[test]
    fn judge_trusts_a_remote_stop_only_after_a_full_sweep() {
        let mut since = BTreeMap::new();
        let obs = vec![(inst(0), Observed::StoppedRemote)];
        let sweep = cl_sched::run::SWEEP_INTERVAL;
        let t0 = Duration::from_secs(2);
        assert!(matches!(judge(&obs, &mut since, t0), Verdict::Waiting(_)));
        assert!(matches!(
            judge(&obs, &mut since, t0 + sweep - Duration::from_millis(1)),
            Verdict::Waiting(_)
        ));
        assert_eq!(judge(&obs, &mut since, t0 + sweep), Verdict::Done);
    }

    #[test]
    fn judge_restarts_the_remote_window_when_it_flips_back_to_running() {
        let mut since = BTreeMap::new();
        let sweep = cl_sched::run::SWEEP_INTERVAL;
        let stopped = vec![(inst(0), Observed::StoppedRemote)];
        let running = vec![(inst(0), Observed::Running)];
        judge(&stopped, &mut since, Duration::ZERO);
        judge(&running, &mut since, sweep / 2);
        assert!(matches!(
            judge(&stopped, &mut since, sweep),
            Verdict::Waiting(_)
        ));
        assert_eq!(judge(&stopped, &mut since, sweep * 2), Verdict::Done);
    }

    fn stack_state(name: &str, spec: &str) -> cl_raft::state::StackState {
        cl_raft::state::StackState {
            name: name.into(),
            spec: spec.into(),
            revision: 1,
            configs: Default::default(),
            secrets: Default::default(),
            used_configs: Default::default(),
            used_secrets: Default::default(),
        }
    }

    fn assign(state: &mut ClusterState, instance: InstanceId) {
        state.assignments.insert(
            instance.clone(),
            cl_raft::state::Assignment {
                instance,
                node: "n1".into(),
                desired: DesiredState::Running,
                image_digest: None,
                migration: None,
            },
        );
    }

    const SPEC: &str =
        "services:\n  web:\n    image: nginx\n    volumes:\n      - fast/data:/data\n";

    #[test]
    fn instances_using_finds_mounting_instances() {
        let mut state = ClusterState::default();
        state.stacks.insert("app".into(), stack_state("app", SPEC));
        assign(&mut state, inst(0));
        let id = VolumeId::new("fast", "data");
        assert_eq!(instances_using(&state, &id).unwrap(), vec![inst(0)]);
        let other = VolumeId::new("fast", "other");
        assert!(instances_using(&state, &other).unwrap().is_empty());
    }

    #[test]
    fn instances_using_bails_on_a_missing_stack() {
        let mut state = ClusterState::default();
        assign(&mut state, inst(0));
        let err = instances_using(&state, &VolumeId::new("fast", "data")).unwrap_err();
        assert!(err.to_string().contains("app.web.0"), "{err}");
    }

    #[test]
    fn instances_using_bails_on_an_unparsable_spec() {
        let mut state = ClusterState::default();
        state
            .stacks
            .insert("app".into(), stack_state("app", "services: [unterminated"));
        assign(&mut state, inst(0));
        assert!(instances_using(&state, &VolumeId::new("fast", "data")).is_err());
    }
}
