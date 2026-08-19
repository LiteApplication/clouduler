//! Building the release binary inside a docker container.
//!
//! One node per architecture builds natively in a stock `rust:*-bookworm`
//! image, sidestepping cross toolchains entirely: the whole point of "one
//! builder per arch" is that the container already runs on that arch's
//! silicon, so `cargo build --release` produces a binary for it with no
//! target flag needed.

use std::path::Path;

use cl_docker::{BindMount, ContainerSpec, Docker, RestartPolicy};

/// Official image tag the build container runs. Bookworm because that is
/// what clouduler's own release binaries are built and tested against
/// elsewhere in this repo.
pub const BUILD_IMAGE: &str = "rust:1-bookworm";

/// Path inside the container the checkout is bind-mounted at.
const WORKDIR: &str = "/build/src";
/// Where cargo drops the built binary, relative to the workdir.
const BUILT_BINARY: &str = "target/release/clouduler";

/// The container spec for a build of `checkout_path`, named so a rerun can
/// find and clean up a leftover container from a previous attempt.
///
/// `ContainerSpec` has no first-class workdir field, so the `cd` is folded
/// into the command via a shell, keeping the mount point and the build
/// directory in the same place without adding a build-only field to the
/// shared spec type.
pub fn build_container_spec(name: &str, checkout_path: &Path) -> ContainerSpec {
    ContainerSpec {
        name: name.to_string(),
        image: BUILD_IMAGE.to_string(),
        stack: "_clouduler_self_update".to_string(),
        service: "build".to_string(),
        instance: name.to_string(),
        command: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("cd {WORKDIR} && cargo build --release --bin clouduler"),
        ]),
        mounts: vec![BindMount {
            host_path: checkout_path.display().to_string(),
            container_path: WORKDIR.to_string(),
            read_only: false,
        }],
        restart_policy: RestartPolicy::No,
        ..Default::default()
    }
}

/// Run a build to completion inside `container_name`, which must already have
/// been created with [`build_container_spec`] and started, then pull the
/// resulting binary out and hash it.
///
/// Returns the binary bytes and their blake3 hash (hex), so the caller can
/// write the artifact to disk and record the hash in the run's `Built` phase
/// without re-reading the file.
pub async fn wait_and_collect(
    docker: &Docker,
    container_name: &str,
) -> Result<(Vec<u8>, String), String> {
    let exit_code = docker
        .wait(container_name)
        .await
        .map_err(|e| format!("waiting for build container: {e}"))?;
    if exit_code != 0 {
        return Err(format!("cargo build exited with status {exit_code}"));
    }

    let binary = docker
        .download_file(container_name, &format!("{WORKDIR}/{BUILT_BINARY}"))
        .await
        .map_err(|e| format!("fetching built binary: {e}"))?;

    let hash = blake3::hash(&binary).to_hex().to_string();
    Ok((binary, hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_checkout_is_bind_mounted_read_write() {
        let spec = build_container_spec("build-x86_64", &PathBuf::from("/srv/checkouts/abc"));
        let mount = &spec.mounts[0];
        assert_eq!(mount.host_path, "/srv/checkouts/abc");
        assert_eq!(mount.container_path, WORKDIR);
        assert!(!mount.read_only);
    }

    #[test]
    fn the_command_builds_the_release_binary_from_the_workdir() {
        let spec = build_container_spec("build-x86_64", &PathBuf::from("/srv/checkouts/abc"));
        let command = spec.command.expect("command set").join(" ");
        assert!(command.contains(WORKDIR));
        assert!(command.contains("cargo build --release --bin clouduler"));
    }

    #[test]
    fn the_container_does_not_restart_itself() {
        let spec = build_container_spec("build-x86_64", &PathBuf::from("/srv/checkouts/abc"));
        assert_eq!(spec.restart_policy, RestartPolicy::No);
    }
}
