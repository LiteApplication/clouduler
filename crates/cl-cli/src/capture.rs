//! `clouduler capture`: run `tcpdump` on the bridge of a stack's overlay network,
//! on the node the target instance runs on. Same node resolution and auth as
//! `clouduler exec`; output is relayed over a websocket to
//! `cl-api::routes::instances::internal_capture`.

use anyhow::{Context, Result};
use cl_config::ClusterConfig;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::Message;

use crate::exec::connect_ws;
use crate::logs::resolve_single;
use crate::node_channel::load_key;

pub async fn run(
    cfg: &ClusterConfig,
    target: &str,
    network: Option<String>,
    write: Option<std::path::PathBuf>,
    filter: Vec<String>,
    via: Option<&str>,
) -> Result<()> {
    let assignment = resolve_single(cfg, target, via).await?;

    let key = load_key(cfg, "the node running this")?;

    let pcap = write.is_some();
    let qs = serde_urlencoded::to_string(CaptureQuery {
        network,
        filter: (!filter.is_empty()).then(|| filter.join(" ")),
        pcap,
    })?;
    let path = format!(
        "/api/instances/internal/{}/capture?{qs}",
        assignment.instance
    );

    let ws_stream = connect_ws(cfg, &key, &assignment, &path, "capture").await?;

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

async fn save_pcap<S>(
    mut ws_stream: tokio_tungstenite::WebSocketStream<S>,
    path: &std::path::Path,
) -> Result<()>
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
                file.write_all(&data)
                    .await
                    .with_context(|| format!("writing to {}", path.display()))?;
                bytes += data.len() as u64;
            }
            Ok(Message::Text(text)) => eprintln!("{text}"),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    file.flush()
        .await
        .with_context(|| format!("flushing {}", path.display()))?;
    eprintln!("wrote {bytes} bytes to {}", path.display());
    Ok(())
}
