//! Container resource stats, for `GET /api/instances/:id/stats`.
//!
//! cl-docker has no stats method and this crate cannot edit it, so this opens its own bollard connection to the docker socket
//! and translates the response into [`crate::stream::StatSample`].

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

/// One reading with the share of the whole host's CPU, for rebalancing.
pub struct OneSample {
    pub memory_used_bytes: u64,
    /// 0 to 100 across all of the host's CPUs (`StatSample::cpu_percent` is per core).
    pub host_cpu_percent: f64,
}

/// A single stats reading (docker waits about a second to fill in the CPU delta).
pub async fn read_once(container_name: String) -> Result<OneSample, ApiError> {
    let docker = bollard::Docker::connect_with_socket_defaults().map_err(ApiError::internal)?;
    let options = StatsOptionsBuilder::default().stream(false).build();
    let resp = docker
        .stats(&container_name, Some(options))
        .next()
        .await
        .ok_or_else(|| ApiError::Unavailable(format!("no stats for {container_name}")))?
        .map_err(ApiError::internal)?;
    let online_cpus = resp
        .cpu_stats
        .as_ref()
        .and_then(|c| c.online_cpus)
        .unwrap_or(1)
        .max(1);
    let sample = sample_from_response(resp);
    Ok(OneSample {
        memory_used_bytes: sample.memory_used_bytes,
        host_cpu_percent: sample.cpu_percent / f64::from(online_cpus),
    })
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
