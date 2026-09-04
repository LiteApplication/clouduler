//! Translating a `DriveBackupTarget`'s retention fields into `restic forget`
//! arguments.

use cl_raft::state::DriveBackupTarget;

/// `restic forget` arguments for one target's retention policy. Empty when
/// the target sets neither `keep_last` nor `keep_within_secs` - restic's own
/// `forget` refuses to prune anything without at least one `--keep-*` flag,
/// so an unset target intentionally keeps every snapshot forever, which the
/// caller should treat as "skip the forget call" (see `restic::ResticRepo::forget`).
pub fn forget_args(target: &DriveBackupTarget) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(keep_last) = target.keep_last {
        args.push("--keep-last".to_string());
        args.push(keep_last.to_string());
    }
    if let Some(secs) = target.keep_within_secs {
        args.push("--keep-within".to_string());
        args.push(format_within(secs));
    }
    args
}

/// restic's `--keep-within` takes a duration like `30d12h`, built here from
/// days/hours since that's the granularity retention is actually expressed
/// in - nobody sets a keep-within of a few seconds.
fn format_within(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    match (days, hours) {
        (0, 0) => "1h".to_string(), // restic requires a non-zero duration
        (days, 0) => format!("{days}d"),
        (0, hours) => format!("{hours}h"),
        (days, hours) => format!("{days}d{hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(keep_last: Option<u32>, keep_within_secs: Option<u64>) -> DriveBackupTarget {
        DriveBackupTarget {
            id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            enabled: true,
            frequency_secs: 86_400,
            time_of_day: None,
            keep_last,
            keep_within_secs,
            stop_containers: None,
            last_run: None,
        }
    }

    #[test]
    fn no_retention_set_produces_no_args() {
        assert!(forget_args(&target(None, None)).is_empty());
    }

    #[test]
    fn keep_last_alone() {
        assert_eq!(forget_args(&target(Some(7), None)), vec!["--keep-last", "7"]);
    }

    #[test]
    fn keep_within_formats_days_and_hours() {
        assert_eq!(forget_args(&target(None, Some(30 * 86_400))), vec!["--keep-within", "30d"]);
        assert_eq!(forget_args(&target(None, Some(3600))), vec!["--keep-within", "1h"]);
        assert_eq!(forget_args(&target(None, Some(30 * 86_400 + 5 * 3600))), vec!["--keep-within", "30d5h"]);
        assert_eq!(forget_args(&target(None, Some(0))), vec!["--keep-within", "1h"]);
    }

    #[test]
    fn both_policies_combine() {
        assert_eq!(
            forget_args(&target(Some(7), Some(30 * 86_400))),
            vec!["--keep-last", "7", "--keep-within", "30d"]
        );
    }
}
