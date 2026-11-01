//! Small text helpers shared by the renderers.

use crate::message::{AlertMessage, Phase};
use crate::settings::Severity;

/// Cut `s` to at most `max` characters, ending in an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push('\u{2026}');
    out
}

/// Civil date from days since the Unix epoch (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-11-01T09:12:00.000Z`, what Discord wants for an embed timestamp.
pub fn rfc3339(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

/// `2026-11-01 09:12 UTC`, for people.
pub fn human_time(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        t / 3600,
        t % 3600 / 60
    )
}

/// `09:12`, for a digest line.
pub fn clock(secs: i64) -> String {
    let t = secs.rem_euclid(86_400);
    format!("{:02}:{:02}", t / 3600, t % 3600 / 60)
}

/// `45s`, `5m`, `2h 10m`, `3d 4h`.
pub fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => match s % 3600 / 60 {
            0 => format!("{}h", s / 3600),
            m => format!("{}h {m}m", s / 3600),
        },
        _ => match s % 86_400 / 3600 {
            0 => format!("{}d", s / 86_400),
            h => format!("{}d {h}h", s / 86_400),
        },
    }
}

pub fn severity_word(s: Severity) -> &'static str {
    match s {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Critical => "critical",
    }
}

/// `Resolved: x`, `Escalated to critical: x`, or just the title.
pub fn title_with_phase(m: &AlertMessage) -> String {
    match m.phase {
        Phase::Resolved => format!("Resolved: {}", m.title),
        Phase::Escalated { .. } => {
            format!("Escalated to {}: {}", severity_word(m.severity), m.title)
        }
        Phase::Opened | Phase::Event => m.title.clone(),
    }
}

/// How long the alert was open, for a resolved message.
pub fn open_for(m: &AlertMessage) -> Option<String> {
    (m.phase == Phase::Resolved && m.at > m.opened_at).then(|| human_duration(m.at - m.opened_at))
}

pub fn phase_word(p: Phase) -> &'static str {
    match p {
        Phase::Opened => "opened",
        Phase::Escalated { .. } => "escalated",
        Phase::Resolved => "resolved",
        Phase::Event => "event",
    }
}

/// Only absolute http(s) URLs are ever put in a button or a link.
pub fn is_http(url: &str) -> bool {
    let u = url.trim();
    (u.starts_with("https://") || u.starts_with("http://")) && !u.contains(char::is_whitespace)
}

/// The first usable link of an alert.
pub fn primary_link(m: &AlertMessage) -> Option<&crate::message::Link> {
    m.links.iter().find(|l| is_http(&l.url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 6), "hello\u{2026}");
        assert_eq!(truncate("héllo wörld", 7), "héllo\u{2026}");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("abc", 1), "\u{2026}");
    }

    #[test]
    fn formats_time() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_782_983_520), "2026-07-02T09:12:00.000Z");
        assert_eq!(human_time(1_782_983_520), "2026-07-02 09:12 UTC");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00.000Z");
        assert_eq!(clock(1_782_983_520), "09:12");
    }

    #[test]
    fn formats_duration() {
        assert_eq!(human_duration(5), "5s");
        assert_eq!(human_duration(300), "5m");
        assert_eq!(human_duration(7800), "2h 10m");
        assert_eq!(human_duration(7200), "2h");
        assert_eq!(human_duration(3 * 86_400 + 4 * 3600), "3d 4h");
    }

    #[test]
    fn only_http_links() {
        assert!(is_http("https://x.test/a"));
        assert!(!is_http("javascript:alert(1)"));
        assert!(!is_http("/alerts"));
        assert!(!is_http("https://x.test/a b"));
    }
}
