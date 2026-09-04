//! `clouduler capture` — run `tcpdump` on the bridge of a stack's overlay
//! network, on whichever node the target instance actually runs on.
//!
//! Same node-resolution and node-to-node auth as `clouduler exec`: the
//! instance's node is read out of the replicated state, then this connects
//! directly to that node's api over a websocket
//! (`cl-api::routes::instances::internal_capture`), which spawns `tcpdump`
//! as the daemon's own root process (it already needs `CAP_NET_ADMIN` for the
//! overlay bridges) and relays its output back frame by frame. This is what
//! used to mean SSHing into a cluster node and reasoning out the right
//! bridge interface name by hand.

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use cl_update::internal_client::AUTH_HEADER;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::Connector;

use crate::logs::resolve_target;
use crate::stack::client_for;

pub async fn run(
    cfg: &ClusterConfig,
    target: &str,
    network: Option<String>,
    write: Option<std::path::PathBuf>,
    filter: Vec<String>,
    via: Option<&str>,
) -> Result<()> {
    let state = client_for(cfg, via)?.state().await.map_err(|e| anyhow!("{e}"))?;
    let mut assignments = resolve_target(&state, target)?;
    if assignments.len() > 1 {
        let ids = assignments.iter().map(|a| a.instance.to_string()).collect::<Vec<_>>().join(", ");
        bail!("{target} has {} running instances; name one directly: {ids}", assignments.len());
    }
    let assignment = assignments.remove(0);

    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {} to authenticate to the node running this",
            cfg.secrets.key_file.display()
        )
    })?;

    let base = cl_update::peers::base_url(cfg, &assignment.node)
        .ok_or_else(|| anyhow!("node {} has no address on the internal network", assignment.node))?;
    let ws_base = base.replacen("https://", "wss://", 1);

    let pcap = write.is_some();
    let qs = serde_urlencoded::to_string(CaptureQuery {
        network,
        filter: (!filter.is_empty()).then(|| filter.join(" ")),
        pcap,
    })
    .expect("CaptureQuery always serializes");
    let path = format!("/api/instances/internal/{}/capture?{qs}", assignment.instance);
    let signature = cl_secret::internal_auth::sign(&key, &path);

    let uri: Uri = format!("{ws_base}{path}")
        .parse()
        .with_context(|| format!("building the capture url for {}", assignment.node))?;
    let request = ClientRequestBuilder::new(uri).with_header(AUTH_HEADER, signature);

    let tls_config = crate::exec::insecure_tls_config();
    let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(Connector::Rustls(tls_config)))
        .await
        .with_context(|| format!("connecting to {} for {}'s capture session", assignment.node, assignment.instance))?;

    match write {
        Some(path) => save_pcap(ws_stream, &path).await,
        None => print_text(ws_stream).await,
    }
}

#[derive(Serialize)]
struct CaptureQuery {
    network: Option<String>,
    filter: Option<String>,
    pcap: bool,
}

async fn print_text<S>(mut ws_stream: tokio_tungstenite::WebSocketStream<S>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(Message::Text(text)) => print!("{text}"),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    Ok(())
}

async fn save_pcap<S>(mut ws_stream: tokio_tungstenite::WebSocketStream<S>, path: &std::path::Path) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut file = tokio::fs::File::create(path)
        .await
        .with_context(|| format!("creating {}", path.display()))?;
    let mut bytes = 0u64;
    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                file.write_all(&data).await.with_context(|| format!("writing to {}", path.display()))?;
                bytes += data.len() as u64;
            }
            Ok(Message::Text(text)) => eprintln!("{text}"),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    file.flush().await.with_context(|| format!("flushing {}", path.display()))?;
    eprintln!("wrote {bytes} bytes to {}", path.display());
    Ok(())
}
