//! This node's CPU and memory over time, plus free space on its drives.
//!
//! `sysinfo` computes CPU usage as the change between two refreshes, so one
//! `System` lives for the whole process and a background task refreshes it
//! every `SAMPLE_INTERVAL`. The last `HISTORY_LEN` samples are kept for the
//! sparklines.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sysinfo::{Disks, System};

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
pub const HISTORY_LEN: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Unix seconds.
    pub at: i64,
    pub cpu_percent: f32,
    pub memory_used_bytes: u64,
}

/// Free space on the filesystem backing one of this node's drives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DriveSpace {
    pub drive: String,
    pub path: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

struct Sampler {
    system: System,
    ring: VecDeque<Sample>,
}

impl Sampler {
    fn sample(&mut self) {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.push(Sample {
            at: unix_now(),
            cpu_percent: self.system.global_cpu_usage(),
            memory_used_bytes: self.system.used_memory(),
        });
    }

    fn push(&mut self, sample: Sample) {
        if self.ring.len() == HISTORY_LEN {
            self.ring.pop_front();
        }
        self.ring.push_back(sample);
    }
}

static SAMPLER: OnceLock<Mutex<Sampler>> = OnceLock::new();

fn sampler() -> &'static Mutex<Sampler> {
    SAMPLER.get_or_init(|| {
        let mut system = System::new();
        // Baseline for the first delta.
        system.refresh_cpu_usage();
        Mutex::new(Sampler {
            system,
            ring: VecDeque::with_capacity(HISTORY_LEN),
        })
    })
}

/// Start the 5 second sampling task. Idempotent enough for tests: a second
/// call just runs a second sampler into the same ring, so call it once.
pub fn spawn() {
    sampler();
    tokio::spawn(async {
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        loop {
            interval.tick().await;
            if let Ok(mut s) = sampler().lock() {
                s.sample();
            }
        }
    });
}

/// Newest CPU reading, 0 before the first sample lands.
pub fn latest_cpu_percent() -> f32 {
    sampler()
        .lock()
        .ok()
        .and_then(|s| s.ring.back().map(|x| x.cpu_percent))
        .unwrap_or(0.0)
}

/// Up to `HISTORY_LEN` samples, oldest first.
pub fn history() -> Vec<Sample> {
    sampler()
        .lock()
        .map(|s| s.ring.iter().copied().collect())
        .unwrap_or_default()
}

/// Space on each drive's backing directory: the mount point with the longest
/// prefix of the path is the filesystem it lives on.
pub fn drive_space(drives: &BTreeMap<String, PathBuf>) -> Vec<DriveSpace> {
    let disks = Disks::new_with_refreshed_list();
    let mounts: Vec<(PathBuf, u64, u64)> = disks
        .iter()
        .map(|d| {
            (
                d.mount_point().to_path_buf(),
                d.total_space(),
                d.available_space(),
            )
        })
        .collect();
    drives
        .iter()
        .map(|(drive, path)| {
            let (total_bytes, free_bytes) = match_mount(path, &mounts).unwrap_or((0, 0));
            DriveSpace {
                drive: drive.clone(),
                path: path.display().to_string(),
                total_bytes,
                free_bytes,
            }
        })
        .collect()
}

/// `(total, free)` of the mount whose path is the longest prefix of `path`.
fn match_mount(path: &Path, mounts: &[(PathBuf, u64, u64)]) -> Option<(u64, u64)> {
    mounts
        .iter()
        .filter(|(mount, _, _)| path.starts_with(mount))
        .max_by_key(|(mount, _, _)| mount.components().count())
        .map(|(_, total, free)| (*total, *free))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_matching_mount_wins() {
        let mounts = vec![
            (PathBuf::from("/"), 100, 10),
            (PathBuf::from("/srv"), 200, 20),
            (PathBuf::from("/srv/fast"), 300, 30),
            (PathBuf::from("/srv/fastest"), 400, 40),
        ];
        assert_eq!(
            match_mount(Path::new("/srv/fast/data"), &mounts),
            Some((300, 30))
        );
        assert_eq!(
            match_mount(Path::new("/srv/other"), &mounts),
            Some((200, 20))
        );
        assert_eq!(match_mount(Path::new("/home/x"), &mounts), Some((100, 10)));
        assert_eq!(match_mount(Path::new("/srv/x"), &[]), None);
    }

    #[test]
    fn the_ring_keeps_only_the_newest_samples() {
        let mut s = Sampler {
            system: System::new(),
            ring: VecDeque::new(),
        };
        for i in 0..(HISTORY_LEN as i64 + 5) {
            s.push(Sample {
                at: i,
                cpu_percent: 0.0,
                memory_used_bytes: 0,
            });
        }
        assert_eq!(s.ring.len(), HISTORY_LEN);
        assert_eq!(s.ring.front().unwrap().at, 5);
    }

    #[test]
    fn sampling_records_a_reading() {
        let mut s = Sampler {
            system: System::new(),
            ring: VecDeque::new(),
        };
        s.sample();
        assert_eq!(s.ring.len(), 1);
        assert!(s.ring[0].memory_used_bytes > 0);
    }
}
