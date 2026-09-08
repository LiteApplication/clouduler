//! The `clouduler secret` commands. These talk to a running daemon over the
//! node RPC channel, the same way the `stack` commands do.

use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::Request;
use cl_secret::{ClusterKey, Sealed};
use tracing::debug;

use crate::stack::client_for;

/// Seal `value` (or `file`, or stdin if neither is given) and store it as a
/// cluster-wide secret, not owned by any stack.
///
/// Sealing happens here rather than on the cluster, for the same reason it
/// does for a stack's bundled secrets: plaintext must never travel through
/// the raft log, so this needs the cluster key.
pub async fn set(
    cfg: &ClusterConfig,
    name: &str,
    value: Option<String>,
    file: Option<&Path>,
    no_trim: bool,
    via: Option<&str>,
) -> Result<()> {
    let plaintext = match (value, file) {
        (Some(v), _) => v.into_bytes(),
        (None, Some(path)) => {
            let mut buf = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            if !no_trim && buf.last() == Some(&b'\n') {
                buf.pop();
            }
            buf
        }
        (None, None) => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .context("reading value from stdin")?;
            if !no_trim && buf.last() == Some(&b'\n') {
                buf.pop();
            }
            buf
        }
    };

    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "setting a secret needs the cluster key at {}. Run this from a node that holds it, or use the web ui",
            cfg.secrets.key_file.display()
        )
    })?;
    let sealed = Sealed::seal(&key, name, &plaintext).with_context(|| format!("sealing {name}"))?;

    let client = client_for(cfg, via)?;
    debug!(name, "submitting PutSecret");
    let response = client
        .submit(Request::PutSecret {
            name: name.to_string(),
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if let Some(error) = response.error {
        bail!("cluster rejected {name}: {error}");
    }
    println!("set {name}");
    Ok(())
}

pub async fn list(cfg: &ClusterConfig, via: Option<&str>) -> Result<()> {
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if state.secrets.is_empty() {
        println!("no secrets stored");
        return Ok(());
    }

    for (name, secret) in &state.secrets {
        let owner = secret.owner.as_deref().unwrap_or("-");
        println!("{name}\trevision={}\towner={owner}", secret.revision);
    }
    Ok(())
}
