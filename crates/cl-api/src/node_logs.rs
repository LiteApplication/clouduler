//! This daemon's own log lines, kept in memory so the ui can show them.
//!
//! `LogRingLayer` is a `tracing_subscriber` layer the binary adds next to its
//! normal output. It keeps the last `CAPACITY` events and broadcasts each new
//! one to anyone following. Only this process's lines: another node's are
//! fetched from that node (see `routes::nodes`).

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

pub const CAPACITY: usize = 2000;
/// How far a slow follower may fall behind before it skips lines.
const BROADCAST_BACKLOG: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    /// Counts up from 0 for this process, so a client can tell it missed lines.
    pub seq: u64,
    /// Unix milliseconds.
    pub at: i64,
    /// `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    pub level: String,
    pub target: String,
    /// The message, followed by `key=value` for any other fields.
    pub message: String,
}

impl LogLine {
    /// One line of plain text, the way a terminal would show it.
    pub fn render(&self) -> String {
        format!("{} {:>5} {}", self.at, self.level, self.message)
    }
}

pub struct LogRing {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<LogLine>,
}

struct Inner {
    lines: VecDeque<LogLine>,
    next_seq: u64,
}

impl Default for LogRing {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                lines: VecDeque::with_capacity(CAPACITY),
                next_seq: 0,
            }),
            tx: broadcast::channel(BROADCAST_BACKLOG).0,
        }
    }
}

impl LogRing {
    pub fn push(&self, level: &str, target: &str, message: String) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let line = LogLine {
            seq: inner.next_seq,
            at: unix_millis(),
            level: level.to_string(),
            target: target.to_string(),
            message,
        };
        inner.next_seq += 1;
        if inner.lines.len() == CAPACITY {
            inner.lines.pop_front();
        }
        inner.lines.push_back(line.clone());
        // Sent under the lock so a subscriber (which also takes it) sees each
        // line exactly once, either in its backlog or on the receiver.
        let _ = self.tx.send(line);
    }

    /// The last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let skip = inner.lines.len().saturating_sub(n);
        inner.lines.iter().skip(skip).cloned().collect()
    }

    /// The last `n` lines plus a receiver for every line after them, with
    /// nothing missed or repeated between the two.
    pub fn follow(&self, n: usize) -> (Vec<LogLine>, broadcast::Receiver<LogLine>) {
        let Ok(inner) = self.inner.lock() else {
            return (Vec::new(), self.tx.subscribe());
        };
        let skip = inner.lines.len().saturating_sub(n);
        let backlog = inner.lines.iter().skip(skip).cloned().collect();
        (backlog, self.tx.subscribe())
    }
}

static RING: OnceLock<LogRing> = OnceLock::new();

/// The process-wide ring `LogRingLayer` feeds.
pub fn ring() -> &'static LogRing {
    RING.get_or_init(LogRing::default)
}

/// Feeds every event the subscriber's filter lets through into `ring()`.
#[derive(Default, Clone, Copy)]
pub struct LogRingLayer;

impl<S: Subscriber> Layer<S> for LogRingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        ring().push(meta.level().as_str(), meta.target(), visitor.finish());
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.fields.trim_start().to_string()
        } else {
            format!("{}{}", self.message, self.fields)
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn the_ring_keeps_the_newest_lines_in_order() {
        let ring = LogRing::default();
        for i in 0..(CAPACITY + 10) {
            ring.push("INFO", "t", format!("line {i}"));
        }
        let all = ring.tail(usize::MAX);
        assert_eq!(all.len(), CAPACITY);
        assert_eq!(all[0].message, "line 10");
        assert_eq!(all.last().unwrap().seq, (CAPACITY + 9) as u64);
        let last3 = ring.tail(3);
        assert_eq!(last3[0].message, format!("line {}", CAPACITY + 7));
    }

    #[tokio::test]
    async fn following_replays_the_tail_then_streams_without_gaps() {
        let ring = LogRing::default();
        ring.push("INFO", "t", "one".into());
        ring.push("INFO", "t", "two".into());
        let (backlog, mut rx) = ring.follow(10);
        assert_eq!(
            backlog
                .iter()
                .map(|l| l.message.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        ring.push("WARN", "t", "three".into());
        let next = rx.recv().await.unwrap();
        assert_eq!(
            (next.message.as_str(), next.level.as_str()),
            ("three", "WARN")
        );
        assert_eq!(next.seq, 2);
    }

    #[test]
    fn the_layer_records_the_message_and_extra_fields() {
        let subscriber = tracing_subscriber::registry().with(LogRingLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(node = "n1", "could not reach");
        });
        let line = ring()
            .tail(CAPACITY)
            .into_iter()
            .rev()
            .find(|l| l.message.starts_with("could not reach"))
            .expect("the line was recorded");
        assert_eq!(line.level, "WARN");
        assert_eq!(line.message, "could not reach node=n1");
    }
}
