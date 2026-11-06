//! The `clouduler secret` commands. These talk to a running daemon over the
//! node RPC channel, the same way the `stack` commands do.

use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_raft::state::Request;
use cl_secret::{ClusterKey, Sealed};
use tracing::debug;

use cl_raft::client::client_for;

/// Where a secret's new value comes from.
pub enum NewValue {
    Literal(String),
    File(std::path::PathBuf),
    Generate { length: usize, charset: String },
    Stdin,
}

impl NewValue {
    pub fn from_args(
        value: Option<String>,
        file: Option<std::path::PathBuf>,
        generate: Option<usize>,
        charset: &str,
    ) -> Result<Self> {
        Ok(match (value, file, generate) {
            (Some(v), _, _) => NewValue::Literal(v),
            (None, Some(f), _) => NewValue::File(f),
            (None, None, Some(length)) => {
                charset_of(charset)?;
                NewValue::Generate {
                    length,
                    charset: charset.to_string(),
                }
            }
            (None, None, None) => NewValue::Stdin,
        })
    }

    fn plaintext(self, no_trim: bool) -> Result<Vec<u8>> {
        let trim = |mut buf: Vec<u8>| {
            if !no_trim && buf.last() == Some(&b'\n') {
                buf.pop();
            }
            buf
        };
        Ok(match self {
            NewValue::Literal(v) => v.into_bytes(),
            NewValue::File(path) => {
                trim(std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
            }
            NewValue::Stdin => {
                let mut buf = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut buf)
                    .context("reading value from stdin")?;
                trim(buf)
            }
            NewValue::Generate { length, charset } => {
                cl_api::secrets::generate(length, charset_of(&charset)?)
                    .map_err(|e| anyhow!("{e}"))?
            }
        })
    }
}

fn charset_of(name: &str) -> Result<cl_api::secrets::Charset> {
    use cl_api::secrets::Charset;
    match name {
        "alphanumeric" => Ok(Charset::Alphanumeric),
        "hex" => Ok(Charset::Hex),
        "base64" => Ok(Charset::Base64),
        "printable" => Ok(Charset::Printable),
        other => bail!("unknown charset {other}: use alphanumeric, hex, base64 or printable"),
    }
}

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
    let source = match (value, file) {
        (Some(v), _) => NewValue::Literal(v),
        (None, Some(path)) => NewValue::File(path.to_path_buf()),
        (None, None) => NewValue::Stdin,
    };
    store(cfg, name, source.plaintext(no_trim)?, None, via).await?;
    println!("set {name}");
    Ok(())
}

async fn store(
    cfg: &ClusterConfig,
    name: &str,
    plaintext: Vec<u8>,
    owner: Option<String>,
    via: Option<&str>,
) -> Result<()> {
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
            owner,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    if let Some(error) = response.error {
        bail!("cluster rejected {name}: {error}");
    }
    Ok(())
}

/// Replace an existing secret's value and, with `restart`, roll the instances
/// that run an older revision. The restart is scheduled in the daemon and runs
/// after this returns.
pub async fn rotate(
    cfg: &ClusterConfig,
    name: &str,
    value: NewValue,
    no_trim: bool,
    restart: bool,
    via: Option<&str>,
) -> Result<()> {
    let state = client_for(cfg, via)?
        .state()
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let owner = state
        .secrets
        .get(name)
        .ok_or_else(|| anyhow!("no secret named {name}; `secret set` creates one"))?
        .owner
        .clone();

    store(cfg, name, value.plaintext(no_trim)?, owner, via).await?;
    println!("rotated {name}");
    if !restart {
        println!(
            "running containers keep the old value until they restart; `clouduler secret rotate {name} --restart` or the web ui rolls them"
        );
        return Ok(());
    }
    restart_users(cfg, name, via).await
}

async fn restart_users(cfg: &ClusterConfig, name: &str, via: Option<&str>) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Skipped {
        instance: String,
        reason: String,
    }
    #[derive(serde::Deserialize)]
    struct Started {
        restarting: Vec<String>,
        skipped: Vec<Skipped>,
    }

    let node = crate::update_cli::pick_node(cfg, via)?;
    let base = cl_update::peers::base_url(cfg, node)
        .ok_or_else(|| anyhow!("node {node} has no address on the internal network"))?;
    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {}",
            cfg.secrets.key_file.display()
        )
    })?;
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building the http client")?;
    let path = "/api/secrets/internal/restart";
    let body = serde_json::to_vec(&serde_json::json!({ "name": name }))?;
    let resp = cl_update::internal_client::authenticate(
        http.post(format!("{base}{path}")),
        &key,
        "POST",
        path,
        &body,
    )
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(body)
    .send()
    .await
    .with_context(|| format!("asking {node}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{node} answered {status}: {body}");
    }
    let started: Started = resp.json().await.context("decoding the daemon's answer")?;
    if started.restarting.is_empty() {
        println!("nothing to restart");
    } else {
        println!(
            "restarting {} instance(s), one at a time per service:",
            started.restarting.len()
        );
        for id in &started.restarting {
            println!("  {id}");
        }
        println!("watch progress with `clouduler status`; the roll stops at the first failure");
    }
    for s in started.skipped {
        println!("skipped {}: {}", s.instance, s.reason);
    }
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
