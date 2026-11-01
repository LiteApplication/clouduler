//! Generic webhook: one JSON POST, a documented schema (`docs/alerts.md`).

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, RETRY_AFTER, USER_AGENT};
use serde_json::{json, Map, Value};

use super::util::{phase_word, severity_word};
use super::{DeliveryError, TIMEOUT};
use crate::message::{AlertMessage, Body, Notification, Phase};

pub const SCHEMA_VERSION: u32 = 1;

fn alert_json(m: &AlertMessage) -> Value {
    let mut o = Map::new();
    o.insert("event".into(), m.event.clone().into());
    o.insert("category".into(), m.category.clone().into());
    o.insert("severity".into(), severity_word(m.severity).into());
    o.insert("phase".into(), phase_word(m.phase).into());
    if let Phase::Escalated { previous } = m.phase {
        o.insert("previous_severity".into(), severity_word(previous).into());
    }
    o.insert("id".into(), m.id.clone().into());
    o.insert("title".into(), m.title.clone().into());
    o.insert("detail".into(), m.detail.clone().into());
    o.insert("subject".into(), m.subject.clone().into());
    o.insert("stack".into(), json!(m.stack));
    o.insert("instance".into(), json!(m.instance));
    o.insert("node".into(), json!(m.node));
    o.insert("opened_at".into(), m.opened_at.into());
    o.insert("at".into(), m.at.into());
    o.insert("count".into(), m.count.into());
    o.insert("link".into(), json!(m.links.first().map(|l| &l.url)));
    o.insert(
        "links".into(),
        m.links
            .iter()
            .map(|l| json!({ "label": l.label, "url": l.url }))
            .collect::<Vec<_>>()
            .into(),
    );
    o.insert("excerpt".into(), json!(m.excerpt));
    Value::Object(o)
}

/// The request body. Pure, so the schema is testable.
pub fn payload(n: &Notification) -> Value {
    let mut o = Map::new();
    o.insert("version".into(), SCHEMA_VERSION.into());
    o.insert("cluster".into(), n.cluster.clone().into());
    match &n.body {
        Body::Alert(m) => {
            o.insert("type".into(), "alert".into());
            if let Value::Object(a) = alert_json(m) {
                o.extend(a);
            }
        }
        Body::Digest { alerts } => {
            o.insert("type".into(), "digest".into());
            o.insert("count".into(), alerts.len().into());
            o.insert(
                "alerts".into(),
                alerts.iter().map(alert_json).collect::<Vec<_>>().into(),
            );
        }
        Body::Test => {
            o.insert("type".into(), "test".into());
            o.insert("title".into(), "Clouduler test message".into());
            o.insert(
                "detail".into(),
                format!("Alerts for cluster {} will arrive here.", n.cluster).into(),
            );
        }
    }
    if let Some(u) = &n.ui_url {
        o.insert("ui_url".into(), u.clone().into());
    }
    Value::Object(o)
}

/// Extra headers describing the message, for receivers that route on them.
fn event_headers(n: &Notification) -> (String, &'static str) {
    match &n.body {
        Body::Alert(m) => (m.event.clone(), severity_word(m.severity)),
        Body::Digest { .. } => ("digest".into(), "info"),
        Body::Test => ("test".into(), "info"),
    }
}

pub(super) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let secs: f64 = v.parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs.min(3600.0)))
}

/// A transport error with the URL (and its token) stripped.
pub(super) fn transport_error(e: reqwest::Error) -> DeliveryError {
    let e = e.without_url();
    let what = if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "could not connect".to_string()
    } else {
        e.to_string()
    };
    DeliveryError {
        message: format!("request failed: {what}"),
        retryable: true,
        retry_after: None,
    }
}

pub async fn send(
    http: &reqwest::Client,
    url: &str,
    auth: Option<&str>,
    n: &Notification,
) -> Result<(), DeliveryError> {
    let (event, severity) = event_headers(n);
    let mut req = http
        .post(url)
        .timeout(TIMEOUT)
        .header(
            USER_AGENT,
            format!("clouduler/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("X-Clouduler-Event", event)
        .header("X-Clouduler-Severity", severity)
        .json(&payload(n));
    if let Some(a) = auth.map(str::trim).filter(|a| !a.is_empty()) {
        let mut v = HeaderValue::from_str(a).map_err(|_| DeliveryError {
            message: "the Authorization value contains characters a header cannot hold".into(),
            retryable: false,
            retry_after: None,
        })?;
        v.set_sensitive(true);
        req = req.header(AUTHORIZATION, v);
    }
    let resp = req.send().await.map_err(transport_error)?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    Err(DeliveryError {
        message: format!("the receiver answered HTTP {}", status.as_u16()),
        retryable: status.as_u16() == 429 || status.is_server_error() || status.as_u16() == 408,
        retry_after: retry_after(resp.headers()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deliver::testutil::*;

    #[test]
    fn alert_payload_has_documented_fields() {
        let v = payload(&notification(Body::Alert(alert(
            Severity::Critical,
            Phase::Opened,
        ))));
        assert_eq!(v["version"], 1);
        assert_eq!(v["type"], "alert");
        assert_eq!(v["cluster"], "piosny");
        assert_eq!(v["event"], "instance.crash_loop");
        assert_eq!(v["severity"], "critical");
        assert_eq!(v["phase"], "opened");
        assert_eq!(v["node"], "pisoa");
        assert_eq!(v["stack"], "media");
        assert_eq!(v["instance"], Value::Null);
        assert_eq!(v["count"], 3);
        assert_eq!(v["link"], "https://cloud.test/stacks/media");
        assert_eq!(v["links"][1]["label"], "Logs");
        assert_eq!(v["excerpt"][0], "panic: out of memory");
        assert_eq!(v["ui_url"], "https://cloud.test");
        assert!(v.get("previous_severity").is_none());
    }

    #[test]
    fn escalated_names_the_previous_severity() {
        let v = payload(&notification(Body::Alert(alert(
            Severity::Critical,
            Phase::Escalated {
                previous: Severity::Warning,
            },
        ))));
        assert_eq!(v["phase"], "escalated");
        assert_eq!(v["previous_severity"], "warning");
    }

    #[test]
    fn digest_and_test_payloads() {
        let a = alert(Severity::Info, Phase::Event);
        let v = payload(&notification(Body::Digest {
            alerts: vec![a.clone(), a],
        }));
        assert_eq!(v["type"], "digest");
        assert_eq!(v["count"], 2);
        assert_eq!(v["alerts"].as_array().unwrap().len(), 2);
        assert_eq!(v["alerts"][0]["phase"], "event");
        let t = payload(&notification(Body::Test));
        assert_eq!(t["type"], "test");
        assert!(t["detail"].as_str().unwrap().contains("piosny"));
    }

    #[test]
    fn parses_retry_after() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("2.5"));
        assert_eq!(retry_after(&h), Some(Duration::from_millis(2500)));
        h.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct"));
        assert_eq!(retry_after(&h), None);
    }
}
