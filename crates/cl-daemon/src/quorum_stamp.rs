//! A note on disk of when this node last had a quorum.
//!
//! A restarted daemon reads it to tell "bounced a moment ago, the leader has
//! not moved anything yet" apart from "gone long enough that it may have".
//! Best effort throughout: with no usable note the daemon starts closed, which
//! is always safe.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const FILE_NAME: &str = "last-quorum";

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Replace the note atomically, so a crash mid-write leaves the old one
/// rather than a torn file.
pub fn write(path: &Path, unix_secs: u64) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, unix_secs.to_string())?;
    std::fs::rename(&tmp, path)
}

/// The stored timestamp, or `None` when there is none or it is unreadable.
pub fn read(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// How long ago quorum was last seen, if that is recent enough that the leader
/// cannot yet have rescheduled this node's work (younger than `node_timeout`).
/// A timestamp from the future (the clock was set back) counts as stale.
pub fn recent_age(stamp: Option<u64>, now: u64, node_timeout: Duration) -> Option<Duration> {
    let age = Duration::from_secs(now.checked_sub(stamp?)?);
    (age < node_timeout).then_some(age)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_stamp_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        assert_eq!(read(&p), None);
        write(&p, 1_700_000_000).unwrap();
        assert_eq!(read(&p), Some(1_700_000_000));
        // Overwriting leaves no temp file behind.
        write(&p, 1_700_000_010).unwrap();
        assert_eq!(read(&p), Some(1_700_000_010));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn garbage_reads_as_no_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path());
        std::fs::write(&p, "not a number").unwrap();
        assert_eq!(read(&p), None);
    }

    #[test]
    fn only_a_stamp_younger_than_node_timeout_is_recent() {
        let timeout = Duration::from_secs(60);
        assert_eq!(
            recent_age(Some(1000), 1010, timeout),
            Some(Duration::from_secs(10))
        );
        assert_eq!(recent_age(Some(1000), 1060, timeout), None);
        assert_eq!(recent_age(None, 1010, timeout), None);
        // from the future: do not trust it.
        assert_eq!(recent_age(Some(2000), 1010, timeout), None);
    }
}
