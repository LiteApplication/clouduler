//! The RPC listener. Serves raft messages and forwarded writes on the internal
//! network address.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::RwLock;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use crate::contact::Contact;
use crate::ids::NodeId;
use crate::network::Peer;
use crate::quorum::QuorumWatch;
use crate::rpc::{decode, encode, frame, Query, QueryResult, RpcRequest, RpcResponse, StatusReport};
use crate::state::{Request as StateRequest, Response as StateResponse};
use crate::store::StateMachine;
use crate::types::Raft;

/// Everything the RPC handlers need. Built before the node itself exists, since
/// the listener starts as part of bringing the node up.
#[derive(Clone)]
pub struct Context {
    pub name: String,
    pub id: NodeId,
    pub raft: Arc<Raft>,
    pub shared: Arc<RwLock<StateMachine>>,
    pub quorum: QuorumWatch,
    pub contact: Contact,
}

/// Accept connections until the task is dropped. Each connection handles
/// requests sequentially, matching how the client side uses one socket per peer.
pub async fn serve(addr: SocketAddr, ctx: Context) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    debug!(%addr, "rpc listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, ctx).await {
                // A peer going away mid request is routine, so keep it quiet.
                debug!(%peer, error = %e, "rpc connection closed");
            }
        });
    }
}

async fn handle(stream: TcpStream, ctx: Context) -> std::io::Result<()> {
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

        let response = dispatch(&ctx, request).await;
        conn.send(encode(&response)?).await?;
    }

    Ok(())
}

async fn dispatch(ctx: &Context, request: RpcRequest) -> RpcResponse {
    let raft = &ctx.raft;
    let contact = &ctx.contact;

    match request {
        // Leader traffic, so the quorum gate knows we are still being reached.
        RpcRequest::AppendEntries(rpc) => {
            contact.touch();
            match raft.append_entries(rpc).await {
                Ok(response) => RpcResponse::AppendEntries(response),
                Err(e) => RpcResponse::Error(e.to_string()),
            }
        }
        RpcRequest::InstallSnapshot(rpc) => {
            contact.touch();
            match raft.install_snapshot(rpc).await {
                Ok(response) => RpcResponse::InstallSnapshot(response),
                Err(e) => RpcResponse::Error(e.to_string()),
            }
        }
        RpcRequest::Vote(rpc) => match raft.vote(rpc).await {
            Ok(response) => RpcResponse::Vote(response),
            Err(e) => RpcResponse::Error(e.to_string()),
        },
        RpcRequest::Forward(state_request) => RpcResponse::Forward(write(ctx, state_request).await),
        RpcRequest::Query(query) => RpcResponse::Query(Box::new(answer(ctx, query).await)),
        RpcRequest::Ping => RpcResponse::Pong,
    }
}

/// Apply a write, relaying it to the leader when this node is not it.
///
/// The caller should not have to know who leads, so a follower passes the
/// request on rather than bouncing it back. One hop is always enough: the next
/// stop is the leader itself, and if it has since lost the term the error comes
/// back to the client to retry.
async fn write(ctx: &Context, request: StateRequest) -> Result<StateResponse, String> {
    let status = ctx.quorum.status();

    if status.is_leader(ctx.id) {
        return ctx
            .raft
            .client_write(request)
            .await
            .map(|written| written.data)
            .map_err(|e| e.to_string());
    }

    let leader = status.leader.ok_or("no leader reachable")?;
    let addr = ctx
        .raft
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .get_node(&leader)
        .map(|node| node.addr.clone())
        .ok_or_else(|| format!("leader {leader} has no known address"))?;

    match Peer::connect(&addr).call(&RpcRequest::Forward(request)).await {
        Ok(RpcResponse::Forward(result)) => result,
        Ok(RpcResponse::Error(msg)) => Err(msg),
        Ok(other) => Err(format!("unexpected reply from leader {leader}: {other:?}")),
        Err(e) => Err(format!("cannot reach leader {leader} at {addr}: {e}")),
    }
}

/// Reads come from this node's own applied state, no consensus round trip.
async fn answer(ctx: &Context, query: Query) -> QueryResult {
    match query {
        Query::Status => {
            let status = ctx.quorum.status();
            let version = ctx.shared.read().await.data.version;
            QueryResult::Status(StatusReport {
                node: ctx.name.clone(),
                id: ctx.id,
                leader: status.leader,
                is_leader: status.is_leader(ctx.id),
                has_quorum: status.has_quorum,
                voters: status.voters,
                reachable: status.reachable.len(),
                version,
            })
        }
        Query::State { include_secrets } => {
            let guard = ctx.shared.read().await;
            let state = if include_secrets {
                guard.data.clone()
            } else {
                guard.data.redacted()
            };
            QueryResult::State(Box::new(state))
        }
    }
}
