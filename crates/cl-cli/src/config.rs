//! The `clouduler config` commands. These talk to a running daemon over the
//! node RPC channel, the same way the `stack` commands do.

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::Request;
use tracing::debug;

use cl_raft::client::client_for;

/// Push the on-disk config into raft. `cfg` is already read and validated by
/// the time this runs (`ClusterConfig::load` in `main`, before any command
/// dispatches), so a permission or parse problem with the file has already
/// failed loudly and no network call has happened yet.
pub async fn reload(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let yaml = serde_yaml::to_string(cfg).context("re-encoding the config")?;
    let client = client_for(cfg, via)?;

    let expect_generation = client
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?
        .cluster_config
        .map(|c| c.generation)
        .unwrap_or(0);

    debug!(
        expect_generation,
        bytes = yaml.len(),
        "submitting PutClusterConfig"
    );
    let response = client
        .submit(Request::PutClusterConfig {
            yaml,
            expect_generation,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if let Some(error) = response.error {
        bail!("cluster rejected the config: {error}");
    }

    println!(
        "reloaded config into the cluster, generation {}",
        expect_generation + 1
    );
    Ok(())
}
