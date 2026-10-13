//! Talking to a running daemon from outside the cluster, for the cli.
//!
//! Uses the same RPC channel the nodes use between themselves. Writes are
//! submitted to whichever node is asked and forwarded from there to the leader,
//! so the caller does not have to know who leads.

use std::sync::Arc;

use anyhow::{anyhow, Result as AnyResult};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use tracing::debug;

use crate::network::{fixed_key, KeyWatch, Peer};
use crate::rpc::{Query, QueryResult, RpcRequest, RpcResponse, StatusReport};
use crate::state::{ClusterState, Request, Response};

/// Pick a node to talk to. Any node will do for a write, since it forwards to
/// the leader, so prefer the one asked for and otherwise take the first manager.
pub fn client_for(cfg: &ClusterConfig, via: Option<&str>) -> AnyResult<Client> {
    let (name, node) = match via {
        Some(name) => (
            name,
            cfg.node(name)
                .ok_or_else(|| anyhow!("no node named {name} in the config"))?,
        ),
        None => {
            let (name, node) = cfg
                .managers()
                .next()
                .ok_or_else(|| anyhow!("config has no manager to talk to"))?;
            (name.as_str(), node)
        }
    };

    let addr = cfg
        .rpc_addr(node)
        .ok_or_else(|| anyhow!("node {name} has no address on the internal network"))?;
    debug!(node = name, %addr, "submitting through");

    // The rpc channel is gated on the cluster key, so the cli needs it just
    // as it already does to seal and unseal secrets locally.
    let key = ClusterKey::load(&cfg.secrets.key_file).map_err(|e| {
        anyhow!(
            "the node channel needs the cluster key, and {} cannot be read: {e}. Run this from a node that holds the key",
            cfg.secrets.key_file.display()
        )
    })?;
    Ok(Client::connect(addr.to_string(), fixed_key(Arc::new(key))))
}

pub struct Client {
    peer: Peer,
    addr: String,
}

impl Client {
    pub fn connect(addr: impl Into<String>, keys: KeyWatch) -> Self {
        let addr = addr.into();
        Self {
            peer: Peer::connect(&addr, keys),
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
        self.fetch_state(false).await
    }

    /// The applied state with sealed secrets intact, for the cli paths that
    /// open them locally with the cluster key (listing and restoring from a
    /// backup destination). The rpc channel is already gated on that key.
    pub async fn state_with_secrets(&self) -> Result<ClusterState, String> {
        self.fetch_state(true).await
    }

    async fn fetch_state(&self, include_secrets: bool) -> Result<ClusterState, String> {
        match self.query(Query::State { include_secrets }).await? {
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
