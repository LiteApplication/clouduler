//! Telling someone the cluster has lost quorum.
//!
//! Without quorum nothing can be written to raft, so the leader's engine
//! cannot say it. Every manager that stays without quorum for a minute sends
//! one direct critical message, from the settings and secrets in its own
//! (still readable) copy of the state, once per outage. When quorum is back
//! the same node records the outage in the feed as resolved; the leader then
//! sends the "resolved" message to the destinations that heard about it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cl_alert::{Body, Notification, Phase, Severity};
use cl_config::AlertSeverity;
use cl_raft::alerts::{AlertDelivery, AlertRecord, AlertState};
use cl_raft::{Request, Role};

use super::send::{Outcome, Sender};
use super::{route, settings, ui_url, unix_now};
use crate::server::AppState;

pub const GRACE: Duration = Duration::from_secs(60);
const EVENT: &str = "cluster.quorum_lost";

struct Outage {
    id: String,
    started: i64,
    reported_at: i64,
    detail: String,
    deliveries: Arc<Mutex<Vec<AlertDelivery>>>,
}

pub struct QuorumReporter {
    state: AppState,
    sender: Sender,
    lost_since: Option<(Instant, i64)>,
    outage: Option<Outage>,
}

impl QuorumReporter {
    pub fn new(state: AppState) -> Self {
        Self {
            sender: Sender::new(state.cluster.clone(), state.key.clone()),
            state,
            lost_since: None,
            outage: None,
        }
    }

    pub async fn tick(&mut self) {
        let status = self.state.cluster.quorum().status();
        if status.has_quorum {
            self.lost_since = None;
            if self.outage.is_some() {
                self.record_recovery().await;
            }
            return;
        }
        let (since, since_unix) = *self
            .lost_since
            .get_or_insert_with(|| (Instant::now(), unix_now()));
        if self.outage.is_some() || since.elapsed() < GRACE || status.voters == 0 {
            return;
        }
        let st = self.state.cluster.state().await;
        let Some(me) = st.nodes.get(&self.state.node_name) else {
            return;
        };
        if me.role != Role::Manager {
            return;
        }
        let settings = settings::load(&st);
        let detail = format!(
            "{} can reach {} of the {} managers and needs {}. Nothing can be changed until a majority is back; containers that are running are left alone.",
            self.state.node_name,
            status.reachable.len(),
            status.voters,
            status.needed()
        );
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let id = format!("quorum-{}-{}", self.state.node_name, since_unix);
        self.outage = Some(Outage {
            id: id.clone(),
            started: since_unix,
            reported_at: unix_now(),
            detail: detail.clone(),
            deliveries: deliveries.clone(),
        });
        let targets = route::targets(&settings, EVENT, "cluster", Severity::Critical, None);
        if route::muted(&settings, EVENT) || targets.immediate.is_empty() {
            return;
        }
        let ui = ui_url(&self.state, &settings).await;
        let cluster = self.state.cluster_config.read().await.cluster.name.clone();
        let message = cl_alert::AlertMessage {
            id,
            event: EVENT.into(),
            category: "cluster".into(),
            severity: Severity::Critical,
            phase: Phase::Opened,
            title: "Quorum lost".into(),
            detail,
            subject: self.state.node_name.clone(),
            stack: None,
            instance: None,
            node: Some(self.state.node_name.clone()),
            opened_at: since_unix,
            at: unix_now(),
            count: 1,
            links: ui
                .as_ref()
                .map(|u| {
                    vec![cl_alert::Link {
                        label: "Open nodes".into(),
                        url: format!("{u}/nodes"),
                    }]
                })
                .unwrap_or_default(),
            excerpt: Vec::new(),
        };
        let notification = Notification {
            cluster,
            ui_url: ui,
            body: Body::Alert(message),
        };
        for dest in targets.immediate {
            let sender = self.sender.clone();
            let n = notification.clone();
            let sink = deliveries.clone();
            tokio::spawn(async move {
                let o: Outcome = sender
                    .deliver_with_retries(Vec::new(), dest, "opened", &n, None)
                    .await;
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(AlertDelivery {
                        destination: o.destination,
                        at: o.at,
                        ok: o.ok,
                        error: o.error,
                        attempts: o.attempts,
                        phase: o.phase.to_string(),
                        message_id: o.message_id,
                    });
            });
        }
    }

    async fn record_recovery(&mut self) {
        let Some(outage) = &self.outage else { return };
        let now = unix_now();
        let lasted =
            humantime::format_duration(Duration::from_secs((now - outage.started).max(0) as u64));
        let deliveries = outage
            .deliveries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let record = AlertRecord {
            id: outage.id.clone(),
            event: EVENT.into(),
            dedupe: format!("{EVENT}:{}", self.state.node_name),
            category: "cluster".into(),
            severity: AlertSeverity::Critical,
            state: AlertState::Resolved,
            title: "Quorum lost".into(),
            detail: format!(
                "Reported by {} after {}s without quorum; it lasted {lasted}. {}",
                self.state.node_name,
                outage.reported_at - outage.started,
                outage.detail
            ),
            subject: self.state.node_name.clone(),
            stack: None,
            instance: None,
            node: Some(self.state.node_name.clone()),
            opened_at: outage.started,
            last_at: now,
            resolved_at: Some(now),
            count: 1,
            escalated_from: None,
            sent_severity: Some(AlertSeverity::Critical),
            // The leader sends the resolved message to whoever got this one.
            resolved_sent: false,
            digested: true,
            links: Vec::new(),
            excerpt: Vec::new(),
            deliveries,
        };
        // Settings in the record's routing were those of the outage; the
        // leader re-reads the live ones before sending the resolved message.
        if self
            .state
            .cluster
            .submit(Request::PutAlert(record))
            .await
            .is_ok()
        {
            self.outage = None;
        }
    }
}
