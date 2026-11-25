//! Server-Sent Events helpers. Each stream ends when its source ends or the client drops the connection.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{Stream, StreamExt};

use cl_docker::{LogLine, LogStream};
use cl_raft::ClusterNode;

/// cl-raft exposes the applied state only via a plain read, so this polls and emits only on a version change.
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub fn cluster_state_events(
    cluster: std::sync::Arc<ClusterNode>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures::stream::unfold(
        (cluster, None::<u64>),
        |(cluster, last_version)| async move {
            let mut interval = tokio::time::interval(STATE_POLL_INTERVAL);
            loop {
                interval.tick().await;
                // Check the version under the lock; only build a snapshot on change.
                let version = cluster.read(|s| s.version).await;
                if Some(version) != last_version {
                    let state = cluster.snapshot().await;
                    let version = state.version;
                    let redacted = state.redacted();
                    let event = Event::default()
                        .event("state")
                        .json_data(&redacted)
                        .unwrap_or_else(|_| Event::default().event("state").data("{}"));
                    return Some((Ok(event), (cluster, Some(version))));
                }
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn container_log_events(
    logs: impl Stream<Item = cl_docker::Result<LogLine>> + Send + 'static,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = logs.map(|item| {
        let event = match item {
            Ok(line) => Event::default()
                .event(match line.stream {
                    LogStream::Stdout => "stdout",
                    LogStream::Stderr => "stderr",
                })
                .json_data(serde_json::json!({
                    "timestamp": line.timestamp,
                    "text": line.text,
                }))
                .unwrap_or_else(|_| Event::default().data(line.text)),
            Err(e) => Event::default().event("error").data(e.to_string()),
        };
        Ok(event)
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StatSample {
    pub cpu_percent: f64,
    pub memory_used_bytes: u64,
    pub memory_limit_bytes: u64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
}

pub fn container_stat_events(
    samples: impl Stream<Item = StatSample> + Send + 'static,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = samples.map(|sample| {
        Ok(Event::default()
            .event("stats")
            .json_data(&sample)
            .unwrap_or_else(|_| Event::default().data("{}")))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn cpu_percent(
    cpu_total: u64,
    precpu_total: u64,
    system_cpu: u64,
    presystem_cpu: u64,
    online_cpus: u64,
) -> f64 {
    let cpu_delta = cpu_total.saturating_sub(precpu_total) as f64;
    let system_delta = system_cpu.saturating_sub(presystem_cpu) as f64;
    if system_delta > 0.0 && online_cpus > 0 {
        (cpu_delta / system_delta) * online_cpus as f64 * 100.0
    } else {
        0.0
    }
}

/// Docker state and migration sync progress are read live off each node; the cost is the docker
/// inspects, which `summarize_local` already does cheaply.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct InstanceProgressEvent {
    pub instance_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub step: Option<(u8, u8)>,
}

/// One event per instance whose `(state, reason, step)` changed since this connection last emitted it.
///
/// Every change in a snapshot is sent before the next snapshot: returning after the first would make
/// each event cost a whole cluster-wide recompute, so bursts would trickle out and short steps never be seen.
pub fn instance_progress_events(
    state: crate::server::AppState,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    struct Cursor {
        state: crate::server::AppState,
        last: std::collections::HashMap<String, InstanceProgressEvent>,
        ready: std::collections::VecDeque<InstanceProgressEvent>,
        interval: tokio::time::Interval,
    }
    let mut interval = tokio::time::interval(PROGRESS_POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let cursor = Cursor {
        state,
        last: std::collections::HashMap::new(),
        ready: std::collections::VecDeque::new(),
        interval,
    };
    let stream = futures::stream::unfold(cursor, |mut cursor| async move {
        loop {
            if let Some(event) = cursor.ready.pop_front() {
                let sse_event = Event::default()
                    .event("progress")
                    .json_data(&event)
                    .unwrap_or_else(|_| Event::default().event("progress").data("{}"));
                return Some((Ok(sse_event), cursor));
            }
            cursor.interval.tick().await;
            let cluster_state = cursor.state.cluster.state().await;
            for (id, event) in
                crate::routes::instances::progress_snapshot(&cursor.state, &cluster_state).await
            {
                if cursor.last.get(&id) == Some(&event) {
                    continue;
                }
                cursor.last.insert(id, event.clone());
                cursor.ready.push_back(event);
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_is_zero_with_no_system_delta() {
        assert_eq!(cpu_percent(100, 50, 0, 0, 4), 0.0);
    }

    #[test]
    fn cpu_percent_scales_by_online_cpus() {
        let pct = cpu_percent(200, 100, 1000, 900, 4);
        assert!((pct - 400.0).abs() < 0.01);
    }

    #[test]
    fn cpu_percent_never_goes_negative_on_a_reset_counter() {
        assert_eq!(cpu_percent(10, 100, 1000, 900, 4), 0.0);
    }
}
