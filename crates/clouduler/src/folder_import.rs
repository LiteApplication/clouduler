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

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::{ClusterConfig, StackFile};
use cl_drive::{drive_port, resolve_relative, DeltaEngine, DriveRegistry, DriveRole, SyncEngine};
use cl_raft::client::Client;
use cl_raft::ids::{InstanceId, VolumeId};
use cl_raft::state::{ClusterState, DesiredState, Request};
use tracing::debug;

use crate::cli::FolderImportArgs;
use crate::stack::client_for;

/// How long to give instances to stop cleanly before touching the volume
/// they mount, on top of each service's own `stop_grace_period`. There is no
/// cluster-wide signal for "actually stopped" to poll (container state is
/// local to whichever node runs it), so this is a fixed wait: the scheduler
/// sweeps every 10s and a service's own grace period is added on top.
const STOP_POLL_MARGIN: Duration = Duration::from_secs(15);

pub async fn run(cfg: &ClusterConfig, node_name: &str, args: FolderImportArgs) -> Result<()> {
    let (drive, volume, path) = parse_target(&args.target)?;
    debug!(drive, volume, path = path.as_deref().unwrap_or(""), "parsed target");
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
    debug!(via = args.via.as_deref().unwrap_or("<any manager>"), "submitting through");
    let state = client.state().await.map_err(|e| anyhow!("{e}"))?;

    if registry.is_served(&drive) && registry.role(&drive, &state.unreachable) == DriveRole::Standby {
        bail!(
            "node {node_name} is a standby for drive {drive}; import from the active server or a node that mounts it, not a backup candidate"
        );
    }

    let volume_root = registry
        .ensure_volume(&id)
        .with_context(|| format!("preparing the local path for {id}"))?;
    debug!(root = %volume_root.display(), "volume root");

    let mut stopped: Vec<InstanceId> = Vec::new();
    if args.replace {
        let using = instances_using(&state, &id);
        if !using.is_empty() {
            if !args.force {
                bail!(
                    "{id} is mounted by {}; pass --force to stop them, replace the volume, and start them back up",
                    using.iter().map(InstanceId::to_string).collect::<Vec<_>>().join(", ")
                );
            }
            let grace = max_stop_grace(&state, &using);
            debug!(
                instances = %using.iter().map(InstanceId::to_string).collect::<Vec<_>>().join(", "),
                wait = ?(grace + STOP_POLL_MARGIN),
                "stopping instances before replacing the volume"
            );
            set_desired(&client, &using, DesiredState::Stopped).await?;
            tokio::time::sleep(grace + STOP_POLL_MARGIN).await;
            stopped = using;
        }
        debug!("clearing the volume before replacing it");
        clear_directory(&volume_root)?;
        import_replacing(&args, &volume_root)?;
    } else {
        let subroot = match &path {
            Some(p) => resolve_relative(&volume_root, p).with_context(|| format!("resolving {p} under {id}"))?,
            None => volume_root.clone(),
        };
        std::fs::create_dir_all(&subroot).with_context(|| format!("creating {}", subroot.display()))?;
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
        apply_method(&args, &args.source, &dest)?;
    }

    let generation = touch_volume(&client, &id, node_name).await?;
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
    })
    .await;

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

/// Instances whose stack mounts `id`.
fn instances_using(state: &ClusterState, id: &VolumeId) -> Vec<InstanceId> {
    let mut found = Vec::new();
    for assignment in state.assignments.values() {
        let instance = &assignment.instance;
        let Some(stack) = state.stacks.get(&instance.stack) else { continue };
        let Ok(file) = StackFile::parse(&stack.spec) else { continue };
        let Some(service) = file.services.get(&instance.service) else { continue };
        let uses = service
            .volumes
            .iter()
            .any(|m| m.drive.as_deref() == Some(id.drive.as_str()) && m.volume.as_deref() == Some(id.volume.as_str()));
        if uses {
            found.push(instance.clone());
        }
    }
    found
}

fn max_stop_grace(state: &ClusterState, instances: &[InstanceId]) -> Duration {
    instances
        .iter()
        .filter_map(|i| {
            let stack = state.stacks.get(&i.stack)?;
            let file = StackFile::parse(&stack.spec).ok()?;
            file.services.get(&i.service)?.stop_grace_period
        })
        .max()
        .unwrap_or(cl_sched::reconcile::DEFAULT_STOP_GRACE)
}

async fn set_desired(client: &Client, instances: &[InstanceId], desired: DesiredState) -> Result<()> {
    for instance in instances {
        let response = client
            .submit(Request::SetDesiredState { instance: instance.clone(), desired })
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
        .submit(Request::UpsertVolume { id: id.clone(), primary: node_name.to_string() })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = upsert.error {
        bail!("cluster rejected the volume record: {error}");
    }
    debug!(%id, primary = node_name, "submitting TouchVolume");
    let touch = client
        .submit(Request::TouchVolume { id: id.clone(), primary: node_name.to_string() })
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
}

/// Push the volume straight to every other node holding a copy, instead of
/// waiting for that volume's own sync interval to come around.
async fn push_now(args: PushArgs<'_>) -> PushResult {
    let PushArgs { client, registry, state, drive, node_name, id, volume_root, generation } = args;

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
    let engine = DeltaEngine::default();
    let mut failed = Vec::new();
    for (name, addr) in &targets {
        let peer = SocketAddr::new(addr.ip(), drive_port(addr.port()));
        debug!(node = name.as_str(), %peer, "syncing volume");
        match engine.sync(volume_root, peer, id, generation).await {
            Ok(_) => {
                debug!(node = name.as_str(), generation, "recording replica generation");
                let response = client
                    .submit(Request::SetVolumeReplica { id: id.clone(), node: name.clone(), generation })
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
    PushResult { total: targets.len(), failed }
}

fn apply_method(args: &FolderImportArgs, src: &Path, dest: &Path) -> Result<()> {
    if args.r#move {
        move_into(src, dest)
    } else if args.link {
        symlink_into(src, dest)
    } else {
        copy_into(src, dest)
    }
}

/// `--replace`: the volume's directory itself already has its old content
/// cleared, so the source's entries become its entries directly rather than
/// nesting under the source's own name.
fn import_replacing(args: &FolderImportArgs, volume_root: &Path) -> Result<()> {
    if args.source.is_dir() && !args.source.is_symlink() {
        for entry in std::fs::read_dir(&args.source).with_context(|| format!("reading {}", args.source.display()))? {
            let entry = entry?;
            apply_method(args, &entry.path(), &volume_root.join(entry.file_name()))?;
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
        apply_method(args, &args.source, &volume_root.join(name))?;
    }
    Ok(())
}

fn move_into(src: &Path, dest: &Path) -> Result<()> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        // EXDEV: source and destination are on different filesystems, which
        // a plain rename cannot cross (this is exactly what happens when the
        // destination is a FUSE-mounted served drive).
        Err(e) if e.raw_os_error() == Some(18) => {
            copy_into(src, dest)?;
            remove_source(src)
        }
        Err(e) => Err(e).with_context(|| format!("moving {} to {}", src.display(), dest.display())),
    }
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
    let target = src.canonicalize().with_context(|| format!("resolving {}", src.display()))?;
    std::os::unix::fs::symlink(&target, dest)
        .with_context(|| format!("linking {} at {}", target.display(), dest.display()))
}

fn copy_into(src: &Path, dest: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if meta.is_symlink() {
        let target = std::fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dest).with_context(|| format!("recreating symlink at {}", dest.display()))?;
    } else if meta.is_dir() {
        std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
            let entry = entry?;
            copy_into(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dest).with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
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
        assert_eq!(parse_target("bulk/imports").unwrap(), ("bulk".into(), "imports".into(), None));
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
        copy_into(&src, &dest).unwrap();

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
        copy_into(&src, &dest).unwrap();

        assert_eq!(fs::read_link(&dest).unwrap(), Path::new("real.txt"));
    }

    #[test]
    fn move_into_relocates_a_directory_on_the_same_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();

        let dest = tmp.path().join("dest");
        move_into(&src, &dest).unwrap();

        assert!(!src.exists());
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"hello");
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
}
