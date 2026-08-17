//! The RPC listener. Serves raft messages and forwarded writes on the internal
//! network address.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use crate::rpc::{decode, encode, frame, RpcRequest, RpcResponse};
use crate::types::Raft;

/// Accept connections until the task is dropped. Each connection handles
/// requests sequentially, matching how the client side uses one socket per peer.
pub async fn serve(addr: SocketAddr, raft: Arc<Raft>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    debug!(%addr, "rpc listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let raft = raft.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, raft).await {
                // A peer going away mid request is routine, so keep it quiet.
                debug!(%peer, error = %e, "rpc connection closed");
            }
        });
    }
}

async fn handle(stream: TcpStream, raft: Arc<Raft>) -> std::io::Result<()> {
    let mut conn = frame(stream);

    while let Some(raw) = conn.next().await {
        let raw = raw?;
        let request: RpcRequest = match decode(&raw) {
            Ok(request) => request,
            Err(e) => {
                warn!(error = %e, "undecodable rpc frame");
                conn.send(encode(&RpcResponse::Error(e.to_string()))?).await?;
                continue;
            }
        };

        let response = dispatch(&raft, request).await;
        conn.send(encode(&response)?).await?;
    }

    Ok(())
}

async fn dispatch(raft: &Raft, request: RpcRequest) -> RpcResponse {
    match request {
        RpcRequest::AppendEntries(rpc) => match raft.append_entries(rpc).await {
            Ok(response) => RpcResponse::AppendEntries(response),
            Err(e) => RpcResponse::Error(e.to_string()),
        },
        RpcRequest::InstallSnapshot(rpc) => match raft.install_snapshot(rpc).await {
            Ok(response) => RpcResponse::InstallSnapshot(response),
            Err(e) => RpcResponse::Error(e.to_string()),
        },
        RpcRequest::Vote(rpc) => match raft.vote(rpc).await {
            Ok(response) => RpcResponse::Vote(response),
            Err(e) => RpcResponse::Error(e.to_string()),
        },
        // Only the leader can accept a write. A follower that has been sent one
        // reports the error so the caller can retry against the new leader.
        RpcRequest::Forward(state_request) => {
            let result = raft
                .client_write(state_request)
                .await
                .map(|written| written.data)
                .map_err(|e| e.to_string());
            RpcResponse::Forward(result)
        }
        RpcRequest::Ping => RpcResponse::Pong,
    }
}
