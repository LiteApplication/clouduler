//! The `clouduler` system group, and keeping cluster files owned to it.
//!
//! Operating the cluster from the cli means connecting to the daemon over the
//! node rpc channel, which is authenticated by the cluster key, not a per-user
//! credential. A non-root operator can do what root can if it can read:
//!
//! - the config file, for `cfg.rpc_addr`, and
//! - `cfg.secrets.key_file`, to authenticate and to seal/unseal secrets.
//!
//! `node_key_file` and `data_dir` are chowned too, so a later config pointing
//! a cli command at them does not silently regress.

use std::path::Path;
use std::process::Command;

use cl_config::ClusterConfig;
use nix::unistd::{chown, Gid, Group};
use tracing::warn;

/// The system group whose members can operate the cluster from the cli.
/// Hardcoded: nothing needs to vary per cluster.
pub const GROUP: &str = "clouduler";

/// Idempotent, best-effort, cheap enough to run on every daemon startup:
/// creates the group if missing, then chowns the config file, both key files
/// and the data directory to `root:clouduler`, adding group read (group
/// execute for the data directory) to their mode.
///
/// Failures are logged and swallowed: root works regardless, and a failing
/// `groupadd` must not stop the daemon. Retried on every restart.
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

    // Group execute only: traversal needs it, and the directory holds many
    // files, so only the entry itself is touched, not its contents.
    fixup(data_dir, gid, 0o750);
}

/// Commands that can create the system group, in the order tried. `-r` makes
/// it a system group and `-f` turns "already exists" (racing daemons or init
/// re-runs) into success; `addgroup --system` is the Debian/busybox spelling.
fn group_commands() -> [(&'static str, Vec<&'static str>); 2] {
    [
        ("groupadd", vec!["-r", "-f", GROUP]),
        ("addgroup", vec!["--system", GROUP]),
    ]
}

/// Create the group if missing. Called when the systemd unit is written (systemd
/// will not start a unit whose group is missing) and on every daemon start.
pub fn ensure_group() -> anyhow::Result<()> {
    if lookup_gid().is_some() {
        return Ok(());
    }

    let mut last_error = String::new();
    for (program, args) in group_commands() {
        match Command::new(program).args(&args).output() {
            Ok(out) if out.status.success() => return Ok(()),
            Ok(out) => {
                // A concurrent creator may have won the race.
                if lookup_gid().is_some() {
                    return Ok(());
                }
                last_error = format!(
                    "{program} {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => last_error = format!("running {program}: {e}"),
        }
    }
    anyhow::bail!("could not create the {GROUP} group: {last_error}")
}

fn lookup_gid() -> Option<Gid> {
    Group::from_name(GROUP)
        .ok()
        .flatten()
        .map(|group| group.gid)
}

/// Whether the calling process's user belongs to the `clouduler` group, checking
/// both the primary gid and supplementary groups (`getgid()` alone misses a
/// `usermod -aG` until next login).
///
/// `None` means the group does not exist yet, distinct from `Some(false)`
/// because the fix differs: `ensure` creates it on first daemon start.
pub fn current_user_is_member() -> Option<bool> {
    let gid = lookup_gid()?;
    if nix::unistd::getgid() == gid || nix::unistd::getegid() == gid {
        return Some(true);
    }
    Some(nix::unistd::getgroups().is_ok_and(|groups| groups.contains(&gid)))
}

/// chown `path` to `root:gid` and set its mode to exactly `mode`, best effort.
/// Exact rather than OR'd in, so a hand-loosened file is tightened back. A
/// missing path is not an error: its creator applies permissions and the next
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_creation_is_a_system_group_and_idempotent() {
        let [(prog, args), (fallback, fargs)] = group_commands();
        assert_eq!(prog, "groupadd");
        assert!(args.contains(&"-r") && args.contains(&"-f") && args.contains(&GROUP));
        assert_eq!(fallback, "addgroup");
        assert!(fargs.contains(&"--system") && fargs.contains(&GROUP));
    }

    #[test]
    fn ensure_group_is_a_noop_when_the_group_exists() {
        // root always exists as a group; the lookup short-circuits creation.
        assert!(Group::from_name("root").unwrap().is_some());
    }
}
