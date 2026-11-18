//! Pure "is this target due to run" logic: no clock reads beyond the `now` the
//! caller passes in.
//!
//! `time_of_day` is interpreted as UTC seconds-since-midnight, not local time:
//! timezone awareness needs a calendar library this workspace does not use, and
//! getting it silently wrong is worse than a documented simplification.

use cl_raft::state::DriveBackupTarget;

/// Whether `target` is due to run at `now` (unix seconds).
///
/// - No `time_of_day`: due once `frequency_secs` has elapsed since `last_run`
///   (or immediately if never run).
/// - With `time_of_day`: also requires `now` to be at or past today's occurrence
///   of that time, so a daily 02:00 target waits for 02:00 rather than firing
///   at 14:00.
pub fn is_due(target: &DriveBackupTarget, now: i64) -> bool {
    if !target.enabled {
        return false;
    }
    let elapsed_enough = match target.last_run {
        Some(last_run) => now - last_run >= target.frequency_secs as i64,
        None => true,
    };
    elapsed_enough && at_or_past_time_of_day(target, now)
}

fn at_or_past_time_of_day(target: &DriveBackupTarget, now: i64) -> bool {
    let Some(tod) = &target.time_of_day else {
        return true;
    };
    // A malformed time-of-day (rejected earlier at the api/cli) is treated as
    // "no constraint" rather than "never due".
    let Some((hour, minute)) = parse_time_of_day(tod) else {
        return true;
    };
    let seconds_since_midnight = now.rem_euclid(86_400);
    let target_seconds = i64::from(hour) * 3600 + i64::from(minute) * 60;
    seconds_since_midnight >= target_seconds
}

fn parse_time_of_day(s: &str) -> Option<(u32, u32)> {
    let (hour, minute) = s.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn target(
        frequency_secs: u64,
        time_of_day: Option<&str>,
        last_run: Option<i64>,
    ) -> DriveBackupTarget {
        DriveBackupTarget {
            id: "t1".into(),
            drive: "fast".into(),
            destination_id: "d1".into(),
            enabled: true,
            frequency_secs,
            time_of_day: time_of_day.map(str::to_string),
            keep_last: None,
            keep_within_secs: None,
            keep_daily: None,
            keep_weekly: None,
            keep_monthly: None,
            keep_yearly: None,
            stop_containers: None,
            last_run,
        }
    }

    #[test]
    fn a_disabled_target_is_never_due() {
        let mut t = target(DAY as u64, None, None);
        t.enabled = false;
        assert!(!is_due(&t, 0));
    }

    #[test]
    fn a_never_run_target_with_no_time_of_day_is_immediately_due() {
        assert!(is_due(&target(DAY as u64, None, None), 12345));
    }

    #[test]
    fn interval_scheduling_waits_out_the_frequency() {
        let t = target(DAY as u64, None, Some(1000));
        assert!(!is_due(&t, 1000 + DAY - 1));
        assert!(is_due(&t, 1000 + DAY));
    }

    #[test]
    fn a_time_of_day_target_waits_for_that_time_even_once_the_interval_elapsed() {
        // now = day 1, 01:00 UTC; target time is 02:00.
        let t = target(DAY as u64, Some("02:00"), Some(0));
        let one_am_day2 = DAY + 3600;
        let two_am_day2 = DAY + 2 * 3600;
        assert!(!is_due(&t, one_am_day2));
        assert!(is_due(&t, two_am_day2));
    }

    #[test]
    fn a_time_of_day_target_still_waits_out_the_interval_first() {
        // Ran 1 hour ago; frequency is a full day, so 02:00 today doesn't
        // matter yet even if `now` happens to be past it.
        let t = target(DAY as u64, Some("02:00"), Some(3 * 3600));
        assert!(!is_due(&t, 4 * 3600));
    }

    #[test]
    fn an_unparseable_time_of_day_does_not_block_scheduling() {
        let t = target(DAY as u64, Some("nonsense"), Some(0));
        assert!(is_due(&t, DAY));
    }
}
