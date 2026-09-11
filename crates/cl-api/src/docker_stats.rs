//! Container resource stats, for `GET /api/instances/:id/stats`.
//!
//! cl-docker wraps container lifecycle, logs and inspect, but has no stats
//! method, and this crate cannot add one without editing cl-docker. Bollard
//! itself has a `stats` call, so this module opens its own connection to the
//! docker socket and talks to it directly, translating the raw response into
//! [`crate::stream::StatSample`]. It is a second connection to the same
//! daemon cl-docker already talks to, which is a bit wasteful but keeps the
//! crate boundary clean.

use bollard::query_parameters::StatsOptionsBuilder;
use futures::{Stream, StreamExt};

use crate::error::ApiError;
use crate::stream::{cpu_percent, StatSample};

pub async fn stream(container_name: String) -> Result<impl Stream<Item = StatSample>, ApiError> {
    let docker = bollard::Docker::connect_with_socket_defaults().map_err(ApiError::internal)?;
    let options = StatsOptionsBuilder::default().stream(true).build();

    let raw = docker.stats(&container_name, Some(options));
    Ok(raw.filter_map(|item| async move {
        let resp = item.ok()?;
        Some(sample_from_response(resp))
    }))
}

fn sample_from_response(resp: bollard::models::ContainerStatsResponse) -> StatSample {
    let cpu = resp.cpu_stats.unwrap_or_default();
    let precpu = resp.precpu_stats.unwrap_or_default();
    let cpu_total = cpu
        .cpu_usage
        .as_ref()
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let precpu_total = precpu
        .cpu_usage
        .as_ref()
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let system_cpu = cpu.system_cpu_usage.unwrap_or(0);
    let presystem_cpu = precpu.system_cpu_usage.unwrap_or(0);
    let online_cpus = cpu.online_cpus.unwrap_or(1) as u64;

    let memory = resp.memory_stats.unwrap_or_default();
    let (rx, tx) = resp
        .networks
        .unwrap_or_default()
        .values()
        .fold((0u64, 0u64), |(rx, tx), n| {
            (rx + n.rx_bytes.unwrap_or(0), tx + n.tx_bytes.unwrap_or(0))
        });

    let (read, write) = resp
        .blkio_stats
        .unwrap_or_default()
        .io_service_bytes_recursive
        .unwrap_or_default()
        .into_iter()
        .fold((0u64, 0u64), |(read, write), entry| {
            match entry.op.as_deref() {
                Some("read") | Some("Read") => (read + entry.value.unwrap_or(0), write),
                Some("write") | Some("Write") => (read, write + entry.value.unwrap_or(0)),
                _ => (read, write),
            }
        });

    StatSample {
        cpu_percent: cpu_percent(
            cpu_total,
            precpu_total,
            system_cpu,
            presystem_cpu,
            online_cpus,
        ),
        memory_used_bytes: memory.usage.unwrap_or(0),
        memory_limit_bytes: memory.limit.unwrap_or(0),
        network_rx_bytes: rx,
        network_tx_bytes: tx,
        io_read_bytes: read,
        io_write_bytes: write,
    }
}
