//! `clouduler self-update`: fetch the web ui now and run the cluster self-update, from the
//! command line. Uses the node-to-node channel (HMAC over the cluster key) like `update`.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cl_config::ClusterConfig;
use cl_secret::ClusterKey;
use cl_update::internal_client;
use cl_update::plan::{NodePhase, UpdateRunState};
use serde::{Deserialize, Serialize};

use crate::update_cli::pick_node;

const UI_PATH: &str = "/api/self-update/internal/ui";
const RUN_PATH: &str = "/api/self-update/internal/run";
const POLL: Duration = Duration::from_secs(3);
/// How long the progress poll tolerates the cluster being unreachable (a leader restarting).
const GIVE_UP_AFTER: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Install this release tag instead of the configured source's latest.
    pub tag: Option<String>,
    /// Reinstall even where the nodes already run the release, and fetch the ui again.
    pub force: bool,
    /// Allow a tag older than the running version.
    pub downgrade: bool,
    /// Only download the ui on the chosen manager; start no run.
    pub ui_only: bool,
    /// Return once the run is started instead of following it.
    pub no_wait: bool,
}

#[derive(Serialize)]
struct FetchUiBody {
    force: bool,
}

#[derive(Deserialize)]
struct FetchedUi {
    version: String,
    hash: String,
}

#[derive(Serialize)]
struct StartBody<'a> {
    tag: Option<&'a str>,
    downgrade: bool,
    force: bool,
}

struct Peer {
    name: String,
    base: String,
}

struct Client {
    http: reqwest::Client,
    key: ClusterKey,
}

impl Client {
    async fn send(
        &self,
        peer: &Peer,
        method: reqwest::Method,
        path: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<reqwest::Response> {
        let request = self
            .http
            .request(method.clone(), format!("{}{path}", peer.base))
            .timeout(timeout);
        let resp = internal_client::authenticate(request, &self.key, method.as_str(), path, &body)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| format!("asking {}", peer.name))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("{} answered {status}: {text}", peer.name);
        }
        Ok(resp)
    }
}

fn peers(cfg: &ClusterConfig, via: Option<&str>) -> Result<Vec<Peer>> {
    let first = pick_node(cfg, via)?.to_string();
    let mut names = vec![first];
    for (name, _) in cfg.managers() {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    Ok(names
        .into_iter()
        .filter_map(|name| {
            let base = cl_update::peers::base_url(cfg, &name)?;
            Some(Peer { name, base })
        })
        .collect())
}

pub async fn run(cfg: &ClusterConfig, via: Option<&str>, options: Options) -> Result<()> {
    let peers = peers(cfg, via)?;
    let first = peers
        .first()
        .ok_or_else(|| anyhow!("no manager has an address on the internal network"))?;
    let key = ClusterKey::load(&cfg.secrets.key_file).with_context(|| {
        format!(
            "loading the cluster key from {} to authenticate to {}",
            cfg.secrets.key_file.display(),
            first.name
        )
    })?;
    let client = Client {
        http: reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .context("building the http client")?,
        key,
    };

    // The ui of the running version first: it is what the dashboard serves right now.
    println!("downloading the web ui on {}...", first.name);
    let fetched = client
        .send(
            first,
            reqwest::Method::POST,
            UI_PATH,
            serde_json::to_vec(&FetchUiBody {
                force: options.force,
            })?,
            Duration::from_secs(300),
        )
        .await;
    match fetched {
        Ok(resp) => {
            let ui: FetchedUi = resp.json().await.context("decoding the ui answer")?;
            println!("web ui for {} is in place ({})", ui.version, ui.hash);
        }
        Err(e) if options.ui_only => return Err(e.context("downloading the web ui")),
        Err(e) => eprintln!("could not download the web ui: {e:#}"),
    }
    if options.ui_only {
        return Ok(());
    }

    let body = serde_json::to_vec(&StartBody {
        tag: options.tag.as_deref(),
        downgrade: options.downgrade,
        force: options.force,
    })?;
    let started: UpdateRunState = client
        .send(
            first,
            reqwest::Method::POST,
            RUN_PATH,
            body,
            Duration::from_secs(30),
        )
        .await
        .context("starting the self-update")?
        .json()
        .await
        .context("decoding the run")?;
    println!(
        "started {}{}",
        started.id,
        if options.force {
            " (forced reinstall)"
        } else {
            ""
        }
    );
    if options.no_wait {
        return Ok(());
    }
    follow(&client, &peers, &started.id).await
}

fn describe(phase: &NodePhase) -> String {
    match phase {
        NodePhase::Idle => "waiting".into(),
        NodePhase::Fetching => "downloading".into(),
        NodePhase::FetchFailed { reason } => format!("download failed: {reason}"),
        NodePhase::Ready => "ready".into(),
        NodePhase::Restarting => "restarting".into(),
        NodePhase::Done => "done".into(),
        NodePhase::RestartFailed { reason } => format!("restart failed: {reason}"),
    }
}

async fn follow(client: &Client, peers: &[Peer], id: &str) -> Result<()> {
    let mut shown = std::collections::BTreeMap::new();
    let mut failing_since: Option<std::time::Instant> = None;
    loop {
        tokio::time::sleep(POLL).await;
        // Any manager will do: the one that was asked may be the node restarting.
        let mut latest = None;
        for peer in peers {
            let answer = client
                .send(
                    peer,
                    reqwest::Method::GET,
                    RUN_PATH,
                    Vec::new(),
                    Duration::from_secs(10),
                )
                .await;
            if let Ok(resp) = answer {
                if let Ok(run) = resp.json::<Option<UpdateRunState>>().await {
                    latest = Some(run);
                    break;
                }
            }
        }
        let Some(run) = latest else {
            let since = *failing_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() > GIVE_UP_AFTER {
                bail!(
                    "no manager answered for {}s; check `clouduler status`",
                    GIVE_UP_AFTER.as_secs()
                );
            }
            continue;
        };
        failing_since = None;
        let Some(run) = run.filter(|r| r.id == id) else {
            bail!("the run {id} is no longer the current one");
        };
        for (node, phase) in &run.per_node {
            let line = describe(phase);
            if shown.get(node) != Some(&line) {
                println!("  {node}: {line}");
                shown.insert(node.clone(), line);
            }
        }
        if run.finished_at.is_some() {
            return match run.failure_reason {
                Some(reason) => Err(anyhow!("self-update failed: {reason}")),
                None => {
                    println!(
                        "self-update finished{}",
                        run.release_tag
                            .map(|t| format!(" ({t})"))
                            .unwrap_or_default()
                    );
                    Ok(())
                }
            };
        }
    }
}
