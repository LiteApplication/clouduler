//! Talking to a running daemon from outside the cluster, for the cli.
//!
//! Uses the same RPC channel the nodes use between themselves. Writes are
//! submitted to whichever node is asked and forwarded from there to the leader,
//! so the caller does not have to know who leads.

use crate::network::Peer;
use crate::rpc::{Query, QueryResult, RpcRequest, RpcResponse, StatusReport};
use crate::state::{ClusterState, Request, Response};

pub struct Client {
    peer: Peer,
    addr: String,
}

impl Client {
    pub fn connect(addr: impl Into<String>) -> Self {
        let addr = addr.into();
        Self {
            peer: Peer::connect(&addr),
            addr,
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Submit a write. The node forwards it to the leader.
    pub async fn submit(&self, request: Request) -> Result<Response, String> {
        match self.call(RpcRequest::Forward(request)).await? {
            RpcResponse::Forward(result) => result,
            other => Err(unexpected(&other)),
        }
    }

    pub async fn status(&self) -> Result<StatusReport, String> {
        match self.query(Query::Status).await? {
            QueryResult::Status(status) => Ok(status),
            other => Err(format!("unexpected query result {other:?}")),
        }
    }

    /// The applied state, with secrets blanked.
    pub async fn state(&self) -> Result<ClusterState, String> {
        match self
            .query(Query::State {
                include_secrets: false,
            })
            .await?
        {
            QueryResult::State(state) => Ok(*state),
            other => Err(format!("unexpected query result {other:?}")),
        }
    }

    async fn query(&self, query: Query) -> Result<QueryResult, String> {
        match self.call(RpcRequest::Query(query)).await? {
            RpcResponse::Query(result) => Ok(*result),
            other => Err(unexpected(&other)),
        }
    }

    async fn call(&self, request: RpcRequest) -> Result<RpcResponse, String> {
        self.peer
            .call(&request)
            .await
            .map_err(|e| format!("{}: {e}", self.addr))
    }
}

fn unexpected(response: &RpcResponse) -> String {
    match response {
        RpcResponse::Error(msg) => msg.clone(),
        other => format!("unexpected reply {other:?}"),
    }
}
