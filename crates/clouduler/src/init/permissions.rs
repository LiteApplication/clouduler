//! The `clouduler` system group, and keeping cluster files owned to it.
//!
//! Controlling clouduler is, in practice, being able to run the cli against a
//! running daemon: `stack.rs`'s `client_for` connects over the same plain-TCP
//! node-to-node rpc channel raft itself uses (`cl_raft::client::Client` /
//! `cl_raft::network::Peer`), and that channel has no authentication of its
//! own — `cl-raft/src/server.rs::dispatch` answers `Forward`/`Query`/raft
//! requests from any caller who can open the socket. So there is no separate
//! credential gating cli access; a non-root operator's cli invocation can do
//! exactly what root's can, provided it can read the two files that make that
//! connection possible:
//!
//! - the config file, to find the daemon's rpc address (`cfg.rpc_addr`), and
//! - `cfg.secrets.key_file`, which several cli commands
//!   (`stack.rs::deploy`, `secret.rs`) load directly and locally to
//!   seal/unseal secrets before/after they cross the wire.
//!
//! `cfg.secrets.node_key_file` and `data_dir` are not on that path — nothing
//! in the cli reads either — but are chowned to the group too, per the
//! group's purpose (everything clouduler needs) and so a config that later
//! points a cli command at them does not silently regress.

use std::path::Path;
use std::process::Command;

use cl_config::ClusterConfig;
use nix::unistd::{chown, Gid, Group};
use tracing::warn;

/// The system group whose members can operate the cluster from the cli.
/// Hardcoded rather than a config knob — nothing here needs to vary per
/// cluster, and one is one fewer thing to get wrong in `cluster.yaml`.
pub const GROUP: &str = "clouduler";

/// Idempotent, best-effort, and cheap enough to run on every daemon startup
/// rather than only from `clouduler init`: creates the `clouduler` group if
/// it is missing, then chowns the config file, both key files, and the data
/// directory to `root:clouduler` with group read (and, for the data
/// directory, group execute for traversal) added on top of their existing
/// mode.
///
/// Failures are logged and swallowed instead of failing startup: root can
/// still do everything a `clouduler`-group member could regardless of
/// whether this succeeds, so a `groupadd` that fails (no `shadow-utils`, a
/// read-only `/etc`, ...) should not be why the daemon refuses to come up.
/// The fixup is retried on every restart, so a transient failure heals
/// itself the next time the service bounces.
pub fn ensure(cfg: &ClusterConfig, data_dir: &Path, config_path: &Path) {
    if let Err(e) = ensure_group() {
        warn!(
            error = %e,
            "could not ensure the {GROUP} system group exists; \
             group members will not be able to use the cli against this node"
        );
        return;
    }

    let Some(gid) = lookup_gid() else {
        warn!("the {GROUP} group does not exist even right after creating it; skipping permission fixups");
        return;
    };

    // Group-readable, never group-writable: nothing a cli invocation does
    // needs to write these, and group write on the cluster key would let any
    // member overwrite it and brick every secret sealed with the old one.
    fixup(config_path, gid, 0o640);
    fixup(&cfg.secrets.key_file, gid, 0o640);
    fixup(&cfg.secrets.node_key_file, gid, 0o640);

    // Group execute only, no group read: nothing under `data_dir` is read
    // directly by any cli command (only `cluster.yaml` and `key_file` are,
    // both outside it by default), and it can hold a lot of files — the raft
    // store, materialized stack files, drive volume data — so this only
    // touches the directory entry itself rather than recursing into it.
    fixup(data_dir, gid, 0o750);
}

/// Create the group if it does not already exist.
fn ensure_group() -> anyhow::Result<()> {
    if lookup_gid().is_some() {
        return Ok(());
    }

    // `groupadd` ships with shadow-utils, which every mainstream
    // systemd-based distro clouduler targets already has (systemd itself
    // depends on it transitively via pam/login on all of them). Shelling out
    // is simpler and more correct than reimplementing `/etc/group` edits by
    // hand, and `-f` makes a second daemon (or a re-run of `clouduler init`)
    // racing this one a no-op instead of an error.
    let output = Command::new("groupadd")
        .args(["-f", GROUP])
        .output()
        .map_err(|e| anyhow::anyhow!("running groupadd: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "groupadd -f {GROUP} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn lookup_gid() -> Option<Gid> {
    Group::from_name(GROUP).ok().flatten().map(|group| group.gid)
}

/// chown `path` to `root:gid` and set its mode to exactly `mode`, best
/// effort. Set exactly rather than OR'd in on top of whatever is there
/// already, so a file someone loosened by hand (e.g. a key left world
/// readable) gets tightened back up rather than just gaining the group bit
/// on top of the mistake. A missing path (a key not yet generated, a data
/// directory on a brand new install before the daemon has created it) is not
/// an error: whatever creates it applies its own permissions, and the next
/// startup fixes it up.
fn fixup(path: &Path, gid: Gid, mode: u32) {
    if !path.exists() {
        return;
    }

    if let Err(e) = chown(path, None, Some(gid)) {
        warn!(path = %path.display(), error = %e, "could not chown to the {GROUP} group");
    }

    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        warn!(path = %path.display(), error = %e, "could not set permissions");
    }
}
