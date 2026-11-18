//! Translating a `DriveBackupTarget`'s retention fields into `restic forget`
//! arguments.

use cl_raft::state::DriveBackupTarget;

/// `restic forget` arguments for one target's retention policy. Empty when no
/// `keep_*` field is set: restic refuses to forget without a `--keep-*` flag, so
/// an unset target keeps everything and the caller skips the call (see
/// `restic::ResticRepo::forget`).
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
    for (flag, value) in [
        ("--keep-daily", target.keep_daily),
        ("--keep-weekly", target.keep_weekly),
        ("--keep-monthly", target.keep_monthly),
        ("--keep-yearly", target.keep_yearly),
    ] {
        // 0 would mean "keep none" to restic, never "off", so it is skipped.
        if let Some(n) = value.filter(|n| *n > 0) {
            args.push(flag.to_string());
            args.push(n.to_string());
        }
    }
    args
}

/// restic's `--keep-within` takes a duration like `30d12h`, built from days/hours.
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
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
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
        assert_eq!(
            forget_args(&target(Some(7), None)),
            vec!["--keep-last", "7"]
        );
    }

    #[test]
    fn keep_within_formats_days_and_hours() {
        assert_eq!(
            forget_args(&target(None, Some(30 * 86_400))),
            vec!["--keep-within", "30d"]
        );
        assert_eq!(
            forget_args(&target(None, Some(3600))),
            vec!["--keep-within", "1h"]
        );
        assert_eq!(
            forget_args(&target(None, Some(30 * 86_400 + 5 * 3600))),
            vec!["--keep-within", "30d5h"]
        );
        assert_eq!(
            forget_args(&target(None, Some(0))),
            vec!["--keep-within", "1h"]
        );
    }

    #[test]
    fn calendar_buckets_are_passed_through() {
        let mut t = target(None, None);
        t.keep_daily = Some(2);
        t.keep_weekly = Some(1);
        t.keep_monthly = Some(1);
        t.keep_yearly = Some(1);
        assert_eq!(
            forget_args(&t),
            vec![
                "--keep-daily",
                "2",
                "--keep-weekly",
                "1",
                "--keep-monthly",
                "1",
                "--keep-yearly",
                "1"
            ]
        );
    }

    #[test]
    fn both_policies_combine() {
        assert_eq!(
            forget_args(&target(Some(7), Some(30 * 86_400))),
            vec!["--keep-last", "7", "--keep-within", "30d"]
        );
    }
}
