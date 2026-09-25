//! Server-Sent Events helpers.
//!
//! Every stream here is built from a `futures::Stream` that ends when the
//! underlying source ends (docker log stream closing) or, for `/api/events`,
//! stops emitting on its own since it is driven by `Sse`: the client
//! dropping the connection frees the whole chain, per axum's normal SSE
//! handling, so there is nothing extra to clean up.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{Stream, StreamExt};

use cl_docker::{LogLine, LogStream};
use cl_raft::ClusterNode;

/// How often `/api/events` checks whether the applied state moved.
///
/// cl-raft exposes the applied state only via a plain read (`ClusterNode::state`),
/// with no watch channel this crate can subscribe to without editing cl-raft,
/// which is out of bounds here. Polling on a short interval and only emitting
/// when the version actually changes gets the same externally visible
/// behaviour as a push-based watch: the client is not woken on every tick,
/// only on a real change.
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// SSE stream of `state` events, one per version bump of the applied state.
pub fn cluster_state_events(
    cluster: std::sync::Arc<ClusterNode>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures::stream::unfold(
        (cluster, None::<u64>),
        |(cluster, last_version)| async move {
            let mut interval = tokio::time::interval(STATE_POLL_INTERVAL);
            loop {
                interval.tick().await;
                let state = cluster.state().await;
                if Some(state.version) != last_version {
                    let redacted = state.redacted();
                    let event = Event::default()
                        .event("state")
                        .json_data(&redacted)
                        .unwrap_or_else(|_| Event::default().event("state").data("{}"));
                    return Some((Ok(event), (cluster, Some(state.version))));
                }
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// SSE stream of container log lines.
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

/// One sample of a container's resource usage, as `GET .../stats` documents:
/// cpu, memory, network, io.
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

/// SSE stream of `StatSample`s.
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

/// Compute cpu percent the same way `docker stats` does: the container's cpu
/// delta over the system's cpu delta, scaled by the number of cpus.
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

/// How often `/api/instances/events` recomputes every instance's summary and
/// checks for a change. Migration sync progress and docker's own state are
/// read live off each instance's node (update and redeploy steps come from
/// replicated state, which the ui also gets pushed over `/api/events`); the
/// cost of polling is those docker inspects, which `summarize_local` already
/// does cheaply enough for the existing 4-5s ui poll to call on every
/// instance, every tick.
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// One instance's live progress, as sent over `/api/instances/events`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InstanceProgressEvent {
    pub instance_id: String,
    pub state: String,
    pub reason: Option<String>,
    pub step: Option<(u8, u8)>,
}

/// SSE stream of `progress` events, one per instance whose `(state, reason,
/// step)` changed since the last time this connection emitted anything for
/// it. A steady instance is sent once (its current value) and then never
/// again until something about it actually changes.
///
/// Every change found in one snapshot is sent before the next snapshot is
/// taken. Returning after the first one used to make each event cost a whole
/// cluster-wide recompute (a docker inspect, or a request to the owning node,
/// per instance), so a burst of changes trickled out one per round trip and
/// short-lived steps were routinely never seen at all.
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
        // Container used the whole delta on one of four cpus: 100% of one core.
        let pct = cpu_percent(200, 100, 1000, 900, 4);
        assert!((pct - 400.0).abs() < 0.01);
    }

    #[test]
    fn cpu_percent_never_goes_negative_on_a_reset_counter() {
        // precpu larger than cpu can happen if the container restarted between
        // samples; the delta must not go negative and wrap.
        assert_eq!(cpu_percent(10, 100, 1000, 900, 4), 0.0);
    }
}
