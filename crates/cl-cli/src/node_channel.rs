//! The signed node-to-node http channel the imperative commands use: pick a manager,
//! authenticate with the cluster key (HMAC over method, path and body) and call its
//! internal api.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use cl_update::internal_client;

/// The node to talk to: `via` when given (it must be in the config), else the first manager.
pub fn pick_node<'a>(cfg: &'a ClusterConfig, via: Option<&'a str>) -> Result<&'a str> {
    match via {
        Some(name) => {
            cfg.node(name)
                .ok_or_else(|| anyhow!("no node named {name} in the config"))?;
            Ok(name)
        }
        None => cfg
            .managers()
            .next()
            .map(|(name, _)| name.as_str())
            .ok_or_else(|| anyhow!("config has no manager to talk to")),
    }
}

/// Loads this node's cluster key; `to` completes "to authenticate to ...".
pub fn load_key(cfg: &ClusterConfig, to: &str) -> Result<ClusterKey> {
    ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {} to authenticate to {to}",
            cfg.secrets.key_file.display()
        )
    })
}

/// The http client for node-to-node calls: nodes serve self-signed certificates, the
/// request signature is what authenticates them.
pub fn internal_http_client(timeout: Option<Duration>) -> Result<reqwest::Client> {
    let mut http = reqwest::Client::builder().danger_accept_invalid_certs(true);
    if let Some(timeout) = timeout {
        http = http.timeout(timeout);
    }
    http.build().context("building the http client")
}

/// One node's internal api, with the cluster key and an http client ready to sign requests.
pub struct NodeChannel<'a> {
    pub node: &'a str,
    base: String,
    key: ClusterKey,
    http: reqwest::Client,
}

impl<'a> NodeChannel<'a> {
    /// Picks the node (see [`pick_node`]) and loads the key; `timeout` bounds every request.
    pub fn open(
        cfg: &'a ClusterConfig,
        via: Option<&'a str>,
        timeout: Option<Duration>,
    ) -> Result<Self> {
        let node = pick_node(cfg, via)?;
        let base = cl_update::peers::base_url(cfg, node)
            .ok_or_else(|| anyhow!("node {node} has no address on the internal network"))?;
        let key = load_key(cfg, node)?;
        let http = internal_http_client(timeout)?;
        Ok(Self {
            node,
            base,
            key,
            http,
        })
    }

    /// Sends a signed request. A `body` is sent as json and covered by the signature.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response> {
        let signed = internal_client::authenticate(
            self.http
                .request(method.clone(), format!("{}{path}", self.base)),
            &self.key,
            method.as_str(),
            path,
            body.as_deref().unwrap_or(b""),
        );
        let request = match body {
            Some(body) => signed
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body),
            None => signed,
        };
        request
            .send()
            .await
            .with_context(|| format!("asking {}", self.node))
    }

    /// Passes a success response through; otherwise fails with the node's status and body.
    pub async fn ensure_success(&self, resp: reqwest::Response) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{} answered {status}: {body}", self.node);
    }
}
