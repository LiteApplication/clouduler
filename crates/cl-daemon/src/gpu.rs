//! Finding this node's GPUs, so the scheduler only places a service that
//! needs one where one exists.
//!
//! Detected once at start and then every few minutes (GPUs do not hot-plug
//! often), off the async threads: the directory scans run in `spawn_blocking`
//! and `nvidia-smi` runs with a timeout. The result goes into the node's
//! roster entry through `report_self`.
//!
//! What is read:
//! - NVIDIA: `/dev/nvidia[0-9]*`, `nvidia-smi --query-gpu=index,uuid,name`
//!   when it is installed, and whether docker has an `nvidia` runtime.
//! - DRI render nodes: `/dev/dri/renderD*`, the driver from
//!   `/sys/class/drm/<node>/device/driver`.
//! - The device paths a service's `devices:` entry may name.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cl_docker::Docker;
use cl_raft::state::{NodeGpus, NvidiaGpu, RenderNode};
use tokio::sync::{watch, RwLock};
use tracing::{debug, info};

/// How often to look again once the first detection has worked.
const RESCAN: Duration = Duration::from_secs(5 * 60);
/// How often to look again while docker could not be asked about runtimes.
const RESCAN_NO_DOCKER: Duration = Duration::from_secs(30);
const SMI_TIMEOUT: Duration = Duration::from_secs(10);
const DOCKER_TIMEOUT: Duration = Duration::from_secs(10);

/// `/dev` subdirectories whose entries are listed too.
const DEV_SUBDIRS: &[&str] = &[
    "dri",
    "snd",
    "input",
    "dvb",
    "nvidia-caps",
    "serial/by-id",
    "serial/by-path",
];

/// Where the NVIDIA container toolkit's binaries are looked for, and which
/// binaries. Docker's `--gpus` works with either on the daemon host's PATH,
/// with no `nvidia` runtime registered.
const TOOLKIT_DIRS: &[&str] = &["/usr/bin", "/usr/local/bin", "/usr/sbin"];
const TOOLKIT_BINS: &[&str] = &["nvidia-container-runtime-hook", "nvidia-container-cli"];

/// Detect on a schedule, publishing each result. `None` until the first one,
/// so a daemon that has only just started does not report "no GPUs".
pub async fn run(
    docker: Arc<RwLock<Option<Docker>>>,
    tx: watch::Sender<Option<NodeGpus>>,
) {
    loop {
        let runtimes = match docker.read().await.clone() {
            Some(docker) => tokio::time::timeout(DOCKER_TIMEOUT, docker.runtimes())
                .await
                .ok()
                .and_then(Result::ok),
            None => None,
        };
        let gpus = detect(runtimes.as_deref().unwrap_or_default()).await;
        let asked = runtimes.is_some();
        if tx.borrow().as_ref() != Some(&gpus) {
            info!(
                nvidia = gpus.nvidia_count(),
                nvidia_runtime = gpus.nvidia_runtime,
                render_nodes = gpus.render_nodes.len(),
                "detected gpus"
            );
        }
        tx.send_replace(Some(gpus));
        tokio::time::sleep(if asked { RESCAN } else { RESCAN_NO_DOCKER }).await;
    }
}

async fn detect(docker_runtimes: &[String]) -> NodeGpus {
    let mut gpus = tokio::task::spawn_blocking(|| scan(Path::new("/dev"), Path::new("/sys")))
        .await
        .unwrap_or_default();
    if !gpus.nvidia_devices.is_empty() {
        gpus.nvidia = nvidia_smi().await;
    }
    gpus.nvidia_runtime = docker_runtimes.iter().any(|r| r == "nvidia");
    gpus.nvidia_hook = tokio::task::spawn_blocking(|| {
        find_toolkit(&TOOLKIT_DIRS.iter().map(Path::new).collect::<Vec<_>>())
    })
    .await
    .ok()
    .flatten();
    gpus
}

/// Everything detectable from the filesystem. `dev` and `sys` are parameters
/// so a test can point them at a temp directory.
fn scan(dev: &Path, sys: &Path) -> NodeGpus {
    let mut gpus = NodeGpus::default();
    let names = list_dir(dev);
    gpus.nvidia_devices = nvidia_device_paths(names.iter().map(String::as_str));

    let dri = list_dir(&dev.join("dri"));
    for name in render_node_names(dri.iter().map(String::as_str)) {
        let link = std::fs::read_link(sys.join("class/drm").join(&name).join("device/driver"));
        gpus.render_nodes.push(RenderNode {
            path: format!("/dev/dri/{name}"),
            driver: link
                .ok()
                .map(|l| driver_name(&l.to_string_lossy()))
                .unwrap_or_default(),
        });
    }

    let mut paths = BTreeSet::new();
    for name in names.iter().filter(|n| keep_dev_entry(n)) {
        paths.insert(format!("/dev/{name}"));
    }
    for sub in DEV_SUBDIRS {
        for name in list_dir(&dev.join(sub)) {
            paths.insert(format!("/dev/{sub}/{name}"));
        }
    }
    gpus.dev_paths = paths;
    gpus
}

/// The first toolkit binary found in `dirs`, as a full path.
fn find_toolkit(dirs: &[&Path]) -> Option<String> {
    for bin in TOOLKIT_BINS {
        for dir in dirs {
            let path = dir.join(bin);
            if path.is_file() {
                return Some(path.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn list_dir(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// Run `nvidia-smi` for the GPU list. Empty when it is not installed, fails
/// or hangs: the device nodes still say how many GPUs there are.
async fn nvidia_smi() -> Vec<NvidiaGpu> {
    let run = tokio::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,uuid,name", "--format=csv,noheader"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(SMI_TIMEOUT, run).await {
        Ok(Ok(out)) if out.status.success() => {
            parse_nvidia_smi(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(Ok(out)) => {
            debug!(status = %out.status, "nvidia-smi failed");
            Vec::new()
        }
        Ok(Err(e)) => {
            debug!(error = %e, "nvidia-smi could not run");
            Vec::new()
        }
        Err(_) => {
            debug!("nvidia-smi timed out");
            Vec::new()
        }
    }
}

/// `index, uuid, name` lines, as `--format=csv,noheader` prints them. Lines
/// that do not parse (a warning printed to stdout, say) are skipped.
fn parse_nvidia_smi(out: &str) -> Vec<NvidiaGpu> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ',').map(str::trim);
            let index = parts.next()?.parse().ok()?;
            let uuid = parts.next().filter(|u| !u.is_empty())?.to_string();
            let name = parts.next().unwrap_or_default().to_string();
            Some(NvidiaGpu { index, uuid, name })
        })
        .collect()
}

/// `/dev/nvidia0`, `/dev/nvidia1`, ... from the names under `/dev`, in index
/// order. Not `nvidiactl`, `nvidia-uvm` and the like.
fn nvidia_device_paths<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut found: Vec<(u32, String)> = names
        .filter_map(|n| {
            let index = n.strip_prefix("nvidia")?;
            let index = index.parse::<u32>().ok()?;
            Some((index, format!("/dev/{n}")))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
}

/// `renderD128`, ... from the names under `/dev/dri`.
fn render_node_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    names
        .filter(|n| {
            n.strip_prefix("renderD")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(str::to_string)
        .collect()
}

/// The driver a `device/driver` symlink points at: its last component
/// (`../../../bus/pci/drivers/i915` is `i915`).
fn driver_name(link_target: &str) -> String {
    link_target
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Whether an entry directly under `/dev` is worth reporting. The point is
/// to name devices a service could want (`dri`, `video0`, `ttyUSB0`,
/// `kfd`, `nvidia0`), not to list the hundreds of terminals and loop devices.
fn keep_dev_entry(name: &str) -> bool {
    // Console ttys, loop and ram devices, and raw disks: hundreds of entries
    // nobody passes to a container, kept out of raft state.
    const SKIP_PREFIXES: &[&str] = &[
        "loop", "ram", "zram", "vcs", "tty", "console", "ptmx", "pts", "sd", "nvme", "vd",
        "mmcblk", "dm-", "md",
    ];
    // USB and on-board serial adapters (Zigbee and Z-Wave dongles).
    const KEEP_TTY: &[&str] = &["ttyUSB", "ttyACM", "ttyAMA"];
    if KEEP_TTY.iter().any(|k| name.starts_with(k)) {
        return true;
    }
    !SKIP_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_smi_output_is_parsed_per_line() {
        let out = "0, GPU-3f1a2b4c-0000-1111-2222-333344445555, NVIDIA GeForce RTX 3090\n\
                   1, GPU-aaaa, Tesla T4\n";
        let gpus = parse_nvidia_smi(out);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].uuid, "GPU-3f1a2b4c-0000-1111-2222-333344445555");
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 3090");
        assert_eq!(gpus[1].name, "Tesla T4");
    }

    #[test]
    fn nvidia_smi_noise_and_failure_text_yield_no_gpus() {
        assert!(parse_nvidia_smi("").is_empty());
        assert!(parse_nvidia_smi(
            "NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver."
        )
        .is_empty());
        assert!(parse_nvidia_smi("No devices were found").is_empty());
    }

    #[test]
    fn only_numbered_nvidia_nodes_count_as_gpus() {
        let names = [
            "nvidia1", "nvidia0", "nvidiactl", "nvidia-uvm", "nvidia-modeset", "nvme0", "null",
            "nvidia10",
        ];
        assert_eq!(
            nvidia_device_paths(names.into_iter()),
            vec!["/dev/nvidia0", "/dev/nvidia1", "/dev/nvidia10"]
        );
    }

    #[test]
    fn render_nodes_are_told_from_cards_and_the_by_path_dir() {
        let names = ["by-path", "card0", "card1", "renderD128", "renderD129", "renderD"];
        assert_eq!(
            render_node_names(names.into_iter()),
            vec!["renderD128", "renderD129"]
        );
    }

    #[test]
    fn the_driver_is_the_last_component_of_the_sysfs_link() {
        assert_eq!(driver_name("../../../bus/pci/drivers/i915"), "i915");
        assert_eq!(driver_name("../../../../bus/pci/drivers/amdgpu/"), "amdgpu");
        assert_eq!(driver_name("xe"), "xe");
    }

    #[test]
    fn terminals_and_loop_devices_are_not_reported_but_serial_adapters_are() {
        for kept in ["dri", "video0", "kfd", "nvidia0", "ttyUSB0", "ttyACM1", "ttyAMA0", "serial"] {
            assert!(keep_dev_entry(kept), "{kept}");
        }
        for skipped in [
            "tty", "tty12", "ttyS0", "loop3", "ram0", "vcs1", "ptmx", "pts", "console", "sda", "sda1",
            "nvme0n1", "vda", "mmcblk0", "dm-0", "md0",
        ] {
            assert!(!keep_dev_entry(skipped), "{skipped}");
        }
    }

    #[test]
    fn a_fake_dev_tree_is_scanned() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("dev");
        let sys = root.path().join("sys");
        std::fs::create_dir_all(dev.join("dri")).unwrap();
        std::fs::write(dev.join("dri/renderD128"), "").unwrap();
        std::fs::write(dev.join("dri/card0"), "").unwrap();
        std::fs::write(dev.join("nvidia0"), "").unwrap();
        std::fs::write(dev.join("nvidiactl"), "").unwrap();
        std::fs::write(dev.join("tty3"), "").unwrap();
        std::fs::write(dev.join("ttyUSB0"), "").unwrap();
        std::fs::write(dev.join("sda"), "").unwrap();
        std::fs::create_dir_all(dev.join("serial/by-id")).unwrap();
        std::fs::create_dir_all(dev.join("serial/by-path")).unwrap();
        std::fs::write(dev.join("serial/by-id/usb-Zigbee_Dongle-if00"), "").unwrap();
        std::fs::write(dev.join("serial/by-path/pci-0000:00:14.0-usb-0:1:1.0"), "").unwrap();
        let drm = sys.join("class/drm/renderD128/device");
        std::fs::create_dir_all(&drm).unwrap();
        std::os::unix::fs::symlink("../../../bus/pci/drivers/i915", drm.join("driver")).unwrap();

        let gpus = scan(&dev, &sys);
        assert_eq!(gpus.nvidia_devices, vec!["/dev/nvidia0"]);
        assert_eq!(
            gpus.render_nodes,
            vec![RenderNode {
                path: "/dev/dri/renderD128".to_string(),
                driver: "i915".to_string()
            }]
        );
        assert!(gpus.has_device("/dev/dri"));
        assert!(gpus.has_device("/dev/dri/renderD128"));
        assert!(gpus.has_device("/dev/dri/card0"));
        assert!(gpus.has_device("/dev/nvidiactl"));
        assert!(!gpus.has_device("/dev/tty3"));
        assert!(!gpus.has_device("/dev/sda"));
        assert!(gpus.has_device("/dev/ttyUSB0"));
        assert!(gpus.has_device("/dev/serial/by-id/usb-Zigbee_Dongle-if00"));
        assert!(gpus.has_device("/dev/serial/by-path/pci-0000:00:14.0-usb-0:1:1.0"));
        assert!(!gpus.has_device("/dev/video0"));
    }

    #[test]
    fn the_toolkit_is_found_by_either_binary() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("a"), root.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(find_toolkit(&[a.as_path(), b.as_path()]), None);
        std::fs::write(b.join("nvidia-container-cli"), "").unwrap();
        assert_eq!(
            find_toolkit(&[a.as_path(), b.as_path()]),
            Some(b.join("nvidia-container-cli").to_string_lossy().into_owned())
        );
        std::fs::write(a.join("nvidia-container-runtime-hook"), "").unwrap();
        assert_eq!(
            find_toolkit(&[a.as_path(), b.as_path()]),
            Some(a.join("nvidia-container-runtime-hook").to_string_lossy().into_owned())
        );
    }

    #[test]
    fn a_host_without_gpus_scans_to_nothing_but_the_dev_listing() {
        let root = tempfile::tempdir().unwrap();
        let gpus = scan(&root.path().join("dev"), &root.path().join("sys"));
        assert_eq!(gpus, NodeGpus::default());
    }
}
