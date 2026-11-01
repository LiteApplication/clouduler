//! Getting a notification to a destination: unsealing the secrets just in
//! time, retrying with backoff, and reporting the outcome back to the engine.
//!
//! Nothing here captures settings. Every attempt re-reads the destination,
//! the SMTP server and the sealed secrets from the replicated state, so an
//! edit (or a deleted destination) is honoured by the very next attempt.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cl_alert::settings::{auth_secret, url_secret, SMTP_PASSWORD_SECRET};
use cl_alert::{
    deliver, AlertSettings, DeliveryError, Destination, DestinationKind, DestinationSecrets,
    Notification,
};
use cl_raft::{ClusterNode, ClusterState};
use cl_secret::{ClusterKey, Sealed};
use tokio::sync::{mpsc, RwLock, Semaphore};

use super::{settings, unix_now};

pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
pub const BACKOFF: [Duration; 4] = [
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];
pub const MAX_ATTEMPTS: u32 = 5;
/// Messages one destination is sent per window before the rest is held back,
/// so a bug or an outage that fires hundreds of alerts cannot flood a channel.
const RATE_MAX: usize = 30;
const RATE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// What happened to one delivery, reported to the engine loop.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Records the message was about (several for a digest).
    pub records: Vec<String>,
    pub destination: String,
    pub phase: &'static str,
    pub at: i64,
    pub ok: bool,
    pub error: Option<String>,
    pub attempts: u32,
    /// Count towards the destination's health.
    pub counts_for_health: bool,
    /// Provider message ID (e.g. from Discord webhook ?wait=true)
    pub message_id: Option<String>,
}

#[derive(Clone)]
pub struct Sender {
    cluster: Arc<ClusterNode>,
    key: Arc<RwLock<Option<Arc<ClusterKey>>>>,
    http: reqwest::Client,
    gate: Arc<Semaphore>,
    rate: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
}

impl Sender {
    pub fn new(cluster: Arc<ClusterNode>, key: Arc<RwLock<Option<Arc<ClusterKey>>>>) -> Self {
        // Not `AppState::http`: that one trusts any certificate, which is
        // right for peers and wrong for the internet.
        let http = reqwest::Client::builder()
            .timeout(ATTEMPT_TIMEOUT)
            .user_agent(format!("clouduler/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            cluster,
            key,
            http,
            gate: Arc::new(Semaphore::new(16)),
            rate: Default::default(),
        }
    }

    /// One attempt with explicitly given values, for the "send test" button.
    pub async fn send_once(
        &self,
        dest: &Destination,
        secrets: &DestinationSecrets,
        smtp: Option<&cl_alert::SmtpSettings>,
        n: &Notification,
    ) -> Result<(), DeliveryError> {
        match tokio::time::timeout(
            ATTEMPT_TIMEOUT + Duration::from_secs(2),
            deliver(&self.http, dest, secrets, smtp, n, None),
        )
        .await
        {
            Ok(r) => r.map(|_| ()).map_err(scrub_error),
            Err(_) => Err(DeliveryError {
                message: "timed out after 10 seconds".into(),
                retryable: true,
                retry_after: None,
            }),
        }
    }

    fn allow(&self, destination: &str) -> bool {
        let mut map = self.rate.lock().unwrap_or_else(|e| e.into_inner());
        let q = map.entry(destination.to_string()).or_default();
        let now = Instant::now();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) > RATE_WINDOW)
        {
            q.pop_front();
        }
        if q.len() >= RATE_MAX {
            return false;
        }
        q.push_back(now);
        true
    }

    /// Send in the background; the outcome arrives on `tx`.
    pub fn spawn(
        &self,
        records: Vec<String>,
        destination: String,
        phase: &'static str,
        notification: Notification,
        edit_message_id: Option<String>,
        tx: mpsc::UnboundedSender<Outcome>,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let outcome = this
                .deliver_with_retries(records, destination, phase, &notification, edit_message_id.as_deref())
                .await;
            let _ = tx.send(outcome);
        });
    }

    pub async fn deliver_with_retries(
        &self,
        records: Vec<String>,
        destination: String,
        phase: &'static str,
        notification: &Notification,
        edit_message_id: Option<&str>,
    ) -> Outcome {
        let mut outcome = Outcome {
            records,
            destination: destination.clone(),
            phase,
            at: unix_now(),
            ok: false,
            error: None,
            attempts: 0,
            counts_for_health: true,
            message_id: None,
        };
        if !self.allow(&destination) {
            outcome.error =
                Some("held back: too many messages to this destination in 10 minutes".into());
            outcome.counts_for_health = false;
            return outcome;
        }
        let Ok(_permit) = self.gate.clone().acquire_owned().await else {
            outcome.error = Some("shutting down".into());
            return outcome;
        };
        for attempt in 1..=MAX_ATTEMPTS {
            outcome.attempts = attempt;
            let result = self.attempt(&destination, notification, edit_message_id).await;
            outcome.at = unix_now();
            match result {
                Ok(msg_id) => {
                    outcome.ok = true;
                    outcome.error = None;
                    outcome.message_id = msg_id;
                    return outcome;
                }
                Err(e) => {
                    outcome.error = Some(e.message.clone());
                    if !e.retryable || attempt == MAX_ATTEMPTS {
                        return outcome;
                    }
                    let wait =
                        BACKOFF[(attempt - 1) as usize].max(e.retry_after.unwrap_or_default());
                    tokio::time::sleep(wait.min(Duration::from_secs(600))).await;
                }
            }
        }
        outcome
    }

    async fn attempt(&self, name: &str, n: &Notification, edit_message_id: Option<&str>) -> Result<Option<String>, DeliveryError> {
        let fatal = |m: &str| DeliveryError {
            message: m.to_string(),
            retryable: false,
            retry_after: None,
        };
        let state = self.cluster.state().await;
        let settings = settings::load(&state);
        let Some(dest) = settings.destinations.iter().find(|d| d.name == name) else {
            return Err(fatal("the destination was removed"));
        };
        if !dest.enabled {
            return Err(fatal("the destination was disabled"));
        }
        let key = self.key.read().await.clone();
        let Some(key) = key else {
            return Err(fatal(
                "this node holds no cluster key to open the destination's secrets",
            ));
        };
        let secrets = unseal_for(&state, &key, dest, &settings).map_err(|m| fatal(&m))?;
        let fut = deliver(&self.http, dest, &secrets, settings.smtp.as_ref(), n, edit_message_id);
        match tokio::time::timeout(ATTEMPT_TIMEOUT + Duration::from_secs(2), fut).await {
            Ok(r) => r.map_err(scrub_error),
            Err(_) => Err(DeliveryError {
                message: "timed out".into(),
                retryable: true,
                retry_after: None,
            }),
        }
    }
}

pub fn open(state: &ClusterState, key: &ClusterKey, name: &str) -> Result<Option<String>, String> {
    let Some(entry) = state.secrets.get(name) else {
        return Ok(None);
    };
    if entry.key_fingerprint != key.fingerprint() {
        return Err("a destination secret was sealed under another cluster key".into());
    }
    let bytes = Sealed::from_blob(entry.sealed.clone())
        .open(key, name)
        .map_err(|_| "a destination secret cannot be opened".to_string())?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "a destination secret is not text".to_string())
}

/// The plaintext secrets `dest` needs, read from `state` right now.
pub fn unseal_for(
    state: &ClusterState,
    key: &ClusterKey,
    dest: &Destination,
    settings: &AlertSettings,
) -> Result<DestinationSecrets, String> {
    let mut out = DestinationSecrets::default();
    match &dest.kind {
        DestinationKind::Webhook => {
            out.url = open(state, key, &url_secret(&dest.name))?;
            out.auth = open(state, key, &auth_secret(&dest.name))?;
            if out.url.is_none() {
                return Err("the destination has no URL".into());
            }
        }
        DestinationKind::Discord(_) => {
            out.url = open(state, key, &url_secret(&dest.name))?;
            if out.url.is_none() {
                return Err("the destination has no URL".into());
            }
        }
        DestinationKind::Email { .. } => {
            if settings.smtp.as_ref().is_some_and(|s| s.username.is_some()) {
                out.smtp_password = open(state, key, SMTP_PASSWORD_SECRET)?;
            }
        }
    }
    Ok(out)
}

/// Replace every URL in `text` by its scheme and host, so a webhook path or
/// token that a transport error quotes never reaches the feed, the health
/// record or a log line.
pub fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = ["https://", "http://"]
        .iter()
        .filter_map(|p| rest.find(p))
        .min()
    {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>' | '<' | ','))
            .unwrap_or(tail.len());
        out.push_str(&crate::routes::alerts::mask_url(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn scrub_error(mut e: DeliveryError) -> DeliveryError {
    e.message = scrub_urls(&e.message);
    e
}

#[cfg(test)]
mod scrub_tests {
    use super::*;

    #[test]
    fn urls_in_errors_lose_path_and_token() {
        let m = scrub_urls("error sending request for url (https://discord.com/api/webhooks/1/TOKEN?wait=true): boom");
        assert!(!m.contains("TOKEN") && !m.contains("webhooks"), "{m}");
        assert!(m.contains("https://discord.com/…") && m.contains("boom"));
    }
}
