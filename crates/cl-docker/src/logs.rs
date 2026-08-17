//! Container log streaming.

use bollard::query_parameters::LogsOptionsBuilder;
use futures::{Stream, StreamExt};

use crate::client::Docker;
use crate::error::{classify, Result};

/// Which stream a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// A single line of container output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: LogStream,
    pub timestamp: Option<String>,
    pub text: String,
}

/// How many lines to fetch from the end of the log before following.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tail {
    All,
    Lines(u32),
}

/// Options controlling which log lines are returned.
#[derive(Debug, Clone)]
pub struct LogOptions {
    pub follow: bool,
    pub tail: Tail,
    /// Only return lines logged after this unix timestamp.
    pub since: Option<i64>,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            follow: false,
            tail: Tail::All,
            since: None,
        }
    }
}

impl Docker {
    /// Stream logs for a container. Timestamps are always requested from docker
    /// and split off the front of each line, rather than parsed some other way.
    pub fn logs(&self, name: &str, opts: LogOptions) -> impl Stream<Item = Result<LogLine>> + '_ {
        let tail = match opts.tail {
            Tail::All => "all".to_string(),
            Tail::Lines(n) => n.to_string(),
        };
        let mut builder = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .follow(opts.follow)
            .timestamps(true)
            .tail(&tail);
        if let Some(since) = opts.since {
            builder = builder.since(since as i32);
        }
        let options = builder.build();

        self.inner
            .logs(name, Some(options))
            .map(|item| item.map_err(classify).map(log_line_from_output))
    }
}

fn log_line_from_output(output: bollard::container::LogOutput) -> LogLine {
    use bollard::container::LogOutput;
    let (stream, bytes) = match output {
        LogOutput::StdOut { message } => (LogStream::Stdout, message),
        LogOutput::StdErr { message } => (LogStream::Stderr, message),
        // stdin/console streams shouldn't occur for `logs`, but keep them rather
        // than panic if docker ever sends one.
        LogOutput::StdIn { message } => (LogStream::Stdout, message),
        LogOutput::Console { message } => (LogStream::Stdout, message),
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    split_timestamp(stream, text)
}

/// Docker prefixes each line with an RFC3339 timestamp and a space when
/// `timestamps(true)` is set. Split it off here instead of asking for logs
/// twice (with and without timestamps).
fn split_timestamp(stream: LogStream, line: String) -> LogLine {
    let line = line.strip_suffix('\n').unwrap_or(&line).to_string();
    match line.split_once(' ') {
        Some((ts, rest)) if looks_like_rfc3339(ts) => LogLine {
            stream,
            timestamp: Some(ts.to_string()),
            text: rest.to_string(),
        },
        _ => LogLine {
            stream,
            timestamp: None,
            text: line,
        },
    }
}

fn looks_like_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 20 && b[4] == b'-' && b[7] == b'-' && b[10] == b'T'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_leading_rfc3339_timestamp() {
        let line = split_timestamp(
            LogStream::Stdout,
            "2024-01-02T15:04:05.999999999Z listening on :8080\n".to_string(),
        );
        assert_eq!(
            line.timestamp.as_deref(),
            Some("2024-01-02T15:04:05.999999999Z")
        );
        assert_eq!(line.text, "listening on :8080");
        assert_eq!(line.stream, LogStream::Stdout);
    }

    #[test]
    fn leaves_line_without_timestamp_untouched() {
        let line = split_timestamp(LogStream::Stderr, "panic: boom\n".to_string());
        assert_eq!(line.timestamp, None);
        assert_eq!(line.text, "panic: boom");
    }

    #[test]
    fn does_not_mistake_a_normal_word_for_a_timestamp() {
        let line = split_timestamp(LogStream::Stdout, "hello world\n".to_string());
        assert_eq!(line.timestamp, None);
        assert_eq!(line.text, "hello world");
    }
}
