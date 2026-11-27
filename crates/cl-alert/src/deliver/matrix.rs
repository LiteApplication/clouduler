//! Matrix rooms, through the client-server API:
//! <https://spec.matrix.org/latest/client-server-api/#put_matrixclientv3roomsroomidsendeventtypetxnid>.
//!
//! The homeserver URL is the destination's URL secret and the access token its
//! auth secret. The room (an ID like `!abc:example.org` or an alias like
//! `#ops:example.org`) is not secret and lives in the settings. Messages are
//! `m.text` with an HTML body; the plain `body` is the fallback clients show
//! when they do not render HTML.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderValue, AUTHORIZATION, USER_AGENT};
use serde_json::{json, Value};

use super::email::escape;
use super::util::*;
use super::webhook::{retry_after, transport_error};
use super::{DeliveryError, TIMEOUT};
use crate::message::{AlertMessage, Body, Notification, Phase};
use crate::settings::{MatrixOptions, MatrixStyle, Severity};

const BODY_MAX: usize = 12_000;
const DETAIL_MAX: usize = 1000;
const EXCERPT_MAX: usize = 1500;
const DIGEST_LINES_MAX: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub body: String,
    pub formatted_body: String,
}

impl Message {
    pub fn payload(&self) -> Value {
        json!({
            "msgtype": "m.text",
            "body": self.body,
            "format": "org.matrix.custom.html",
            "formatted_body": self.formatted_body,
        })
    }
}

impl Message {
    /// An `m.replace` edit of the event `event_id`; clients show the new content in its place.
    pub fn edit_payload(&self, event_id: &str) -> Value {
        json!({
            "msgtype": "m.text",
            "body": format!("* {}", self.body),
            "format": "org.matrix.custom.html",
            "formatted_body": format!("* {}", self.formatted_body),
            "m.new_content": self.payload(),
            "m.relates_to": { "rel_type": "m.replace", "event_id": event_id },
        })
    }
}

fn emoji(m: &AlertMessage) -> &'static str {
    if m.phase == Phase::Resolved {
        return "\u{2705}";
    }
    match m.severity {
        Severity::Critical => "\u{1F534}",
        Severity::Warning => "\u{1F7E0}",
        Severity::Info => "\u{26AA}",
    }
}

fn word(m: &AlertMessage) -> String {
    if m.phase == Phase::Resolved {
        "RESOLVED".into()
    } else {
        severity_word(m.severity).to_uppercase()
    }
}

/// Stack, instance and node, the facts the short style keeps.
fn where_facts(m: &AlertMessage) -> Vec<String> {
    let mut facts: Vec<String> = Vec::new();
    if let Some(s) = &m.stack {
        facts.push(format!("stack {s}"));
    }
    if let Some(i) = &m.instance {
        facts.push(format!("instance {i}"));
    }
    if let Some(node) = &m.node {
        facts.push(format!("node {node}"));
    }
    facts
}

/// One line: emoji, severity word, title and where it happened.
fn short_alert(m: &AlertMessage) -> Message {
    let mut text = format!("{} {} {}", emoji(m), word(m), m.title.replace('\n', " "));
    let mut html = format!(
        "{} <strong>{}</strong> {}",
        emoji(m),
        word(m),
        escape(&m.title.replace('\n', " "))
    );
    let facts = where_facts(m);
    if !facts.is_empty() {
        let line = facts.join(" \u{b7} ");
        text.push_str(&format!(" \u{2014} {line}"));
        html.push_str(&format!(" \u{2014} <em>{}</em>", escape(&line)));
    }
    Message {
        body: truncate(&text, BODY_MAX),
        formatted_body: html,
    }
}

fn alert_message(m: &AlertMessage) -> Message {
    let mut text = format!("{} {} {}", emoji(m), word(m), m.title);
    let mut html = format!(
        "{} <strong>{}</strong> {}",
        emoji(m),
        word(m),
        escape(&m.title)
    );
    // The details sit in a quote under the title: the detail, the facts and the log excerpt.
    let mut quoted: Vec<String> = Vec::new();
    let mut quote_html = String::new();
    if !m.detail.is_empty() {
        let d = truncate(&m.detail, DETAIL_MAX);
        quote_html.push_str(&escape(&d).replace('\n', "<br>"));
        quoted.push(d);
    }
    let mut facts = where_facts(m);
    if m.count > 1 {
        facts.push(format!("x{}", m.count));
    }
    if let Some(d) = open_for(m) {
        facts.push(format!("open for {d}"));
    }
    if !facts.is_empty() {
        let line = facts.join(" \u{b7} ");
        if !quote_html.is_empty() {
            quote_html.push_str("<br>");
        }
        quote_html.push_str(&format!("<em>{}</em>", escape(&line)));
        quoted.push(line);
    }
    if !m.excerpt.is_empty() {
        let ex = truncate(&m.excerpt.join("\n"), EXCERPT_MAX);
        quote_html.push_str(&format!("<pre><code>{}</code></pre>", escape(&ex)));
        quoted.push(ex);
    }
    if !quoted.is_empty() {
        for line in quoted.join("\n").lines() {
            text.push_str(&format!("\n> {line}"));
        }
        html.push_str(&format!("<blockquote>{quote_html}</blockquote>"));
    }
    for l in m.links.iter().filter(|l| is_http(&l.url)) {
        text.push_str(&format!("\n{}: {}", l.label, l.url.trim()));
        html.push_str(&format!(
            "<br><a href=\"{}\">{}</a>",
            escape(l.url.trim()),
            escape(&l.label)
        ));
    }
    Message {
        body: truncate(&text, BODY_MAX),
        formatted_body: html,
    }
}

fn short_test(n: &Notification) -> Message {
    let line = format!(
        "{} Clouduler test message \u{2014} alerts for cluster {} will arrive here.",
        emoji_test(),
        n.cluster
    );
    Message {
        formatted_body: format!(
            "{} <strong>Clouduler test message</strong> \u{2014} {}",
            emoji_test(),
            escape(&format!(
                "alerts for cluster {} will arrive here.",
                n.cluster
            ))
        ),
        body: line,
    }
}

fn test_message(n: &Notification) -> Message {
    let title = "Clouduler test message";
    let detail = format!("Alerts for cluster {} will arrive here.", n.cluster);
    let mut text = format!("{} {title}\n{detail}", emoji_test());
    let mut html = format!(
        "{} <strong>{}</strong><br>{}",
        emoji_test(),
        escape(title),
        escape(&detail)
    );
    if let Some(u) = n.ui_url.as_deref().filter(|u| is_http(u)) {
        text.push_str(&format!("\n{u}"));
        html.push_str(&format!("<br><a href=\"{0}\">{0}</a>", escape(u.trim())));
    }
    Message {
        body: text,
        formatted_body: html,
    }
}

fn emoji_test() -> &'static str {
    "\u{1F9EA}"
}

fn digest_message(n: &Notification, alerts: &[AlertMessage]) -> Message {
    let head = format!(
        "\u{1F4CB} Daily digest: {} info alert{}",
        alerts.len(),
        if alerts.len() == 1 { "" } else { "s" }
    );
    let mut text = head.clone();
    let mut html = format!("<strong>{}</strong><ul>", escape(&head));
    for a in alerts.iter().take(DIGEST_LINES_MAX) {
        let mut line = format!("{} {}", clock(a.at), a.title);
        if !a.subject.is_empty() && !a.title.contains(&a.subject) {
            line.push_str(&format!(" \u{2014} {}", a.subject));
        }
        if a.count > 1 {
            line.push_str(&format!(" \u{d7}{}", a.count));
        }
        let line = truncate(&line, 300);
        text.push_str(&format!("\n- {line}"));
        html.push_str(&format!("<li>{}</li>", escape(&line)));
    }
    html.push_str("</ul>");
    if alerts.len() > DIGEST_LINES_MAX {
        let more = format!("and {} more", alerts.len() - DIGEST_LINES_MAX);
        text.push_str(&format!("\n{more}"));
        html.push_str(&escape(&more));
    }
    if let Some(u) = n.ui_url.as_deref().filter(|u| is_http(u)) {
        text.push_str(&format!("\n{u}"));
        html.push_str(&format!("<a href=\"{0}\">{0}</a>", escape(u.trim())));
    }
    Message {
        body: truncate(&text, BODY_MAX),
        formatted_body: html,
    }
}

/// A digest is already one compact list, so both styles share it.
pub fn message(n: &Notification, style: MatrixStyle) -> Message {
    match (&n.body, style) {
        (Body::Test, MatrixStyle::Full) => test_message(n),
        (Body::Test, MatrixStyle::Short) => short_test(n),
        (Body::Alert(m), MatrixStyle::Full) => alert_message(m),
        (Body::Alert(m), MatrixStyle::Short) => short_alert(m),
        (Body::Digest { alerts }, _) => digest_message(n, alerts),
    }
}

/// Percent-encode everything but the RFC 3986 unreserved set, so a room ID's
/// `!`, `:` and an alias's `#` survive as one path segment.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Unique per call, so the homeserver never mistakes two messages for a retry of one.
fn txn_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "clouduler-{nanos}-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn api(homeserver: &str, path: &str) -> String {
    format!(
        "{}/_matrix/client/v3/{path}",
        homeserver.trim().trim_end_matches('/')
    )
}

/// What the homeserver says went wrong, without anything that could carry the token.
fn explain(status: u16, body: &str) -> String {
    let v = serde_json::from_str::<Value>(body).ok();
    let code = v.as_ref().and_then(|v| v["errcode"].as_str()).unwrap_or("");
    let said = v
        .as_ref()
        .and_then(|v| v["error"].as_str().map(|s| truncate(s, 200)))
        .filter(|s| !s.contains("http"));
    match (status, code) {
        (401, _) => {
            "the homeserver rejected the access token (HTTP 401); it may be wrong or logged out"
                .into()
        }
        (403, _) => match said {
            Some(m) => format!(
                "Matrix refused the message (HTTP 403): {m}; is the user joined to the room?"
            ),
            None => "Matrix refused the message (HTTP 403); is the user joined to the room?".into(),
        },
        (404, _) => {
            "the homeserver or room was not found (HTTP 404); check the homeserver URL and the room"
                .into()
        }
        (_, _) => match said {
            Some(m) => format!("Matrix rejected the message (HTTP {status}): {m}"),
            None => format!("the homeserver answered HTTP {status}"),
        },
    }
}

async fn failure(resp: reqwest::Response) -> DeliveryError {
    let status = resp.status();
    let header_wait = retry_after(resp.headers());
    let body = resp.text().await.unwrap_or_default();
    let code = status.as_u16();
    let wait = header_wait.or_else(|| {
        serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["retry_after_ms"].as_f64())
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .map(|ms| Duration::from_secs_f64((ms / 1000.0).min(3600.0)))
    });
    DeliveryError {
        message: if code == 429 {
            "the homeserver is rate limiting this user".to_string()
        } else {
            explain(code, &body)
        },
        retryable: code == 429 || code == 408 || status.is_server_error(),
        retry_after: wait,
    }
}

fn bearer(token: &str) -> Result<HeaderValue, DeliveryError> {
    let mut v = HeaderValue::from_str(&format!(
        "Bearer {}",
        token.trim().trim_start_matches("Bearer ").trim()
    ))
    .map_err(|_| DeliveryError {
        message: "the access token contains characters a header cannot hold".into(),
        retryable: false,
        retry_after: None,
    })?;
    v.set_sensitive(true);
    Ok(v)
}

/// A room alias (`#ops:example.org`) becomes the room ID the send endpoint needs.
async fn resolve_alias(
    http: &reqwest::Client,
    homeserver: &str,
    auth: &HeaderValue,
    alias: &str,
) -> Result<String, DeliveryError> {
    let url = api(
        homeserver,
        &format!("directory/room/{}", encode_segment(alias)),
    );
    let resp = http
        .get(url)
        .timeout(TIMEOUT)
        .header(USER_AGENT, format!("clouduler/{}", cl_config::version()))
        .header(AUTHORIZATION, auth.clone())
        .send()
        .await
        .map_err(transport_error)?;
    if resp.status().as_u16() == 404 {
        return Err(DeliveryError {
            message: "the room alias does not exist on the homeserver".into(),
            retryable: false,
            retry_after: None,
        });
    }
    if !resp.status().is_success() {
        return Err(failure(resp).await);
    }
    let v: Value = resp.json().await.map_err(|_| DeliveryError {
        message: "the homeserver sent an unreadable answer to the alias lookup".into(),
        retryable: true,
        retry_after: None,
    })?;
    v["room_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| DeliveryError {
            message: "the homeserver did not return a room for the alias".into(),
            retryable: false,
            retry_after: None,
        })
}

/// What a rejected login means, never quoting the homeserver's text (it may echo the username).
fn explain_login(status: u16, body: &str) -> String {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["errcode"].as_str().map(str::to_string))
        .unwrap_or_default();
    match (status, code.as_str()) {
        (_, "M_USER_DEACTIVATED") => "the homeserver says this account is deactivated".into(),
        (_, "M_UNRECOGNIZED") | (405, _) => {
            "the homeserver does not offer password login; use an access token instead".into()
        }
        (400 | 401 | 403, _) => "the homeserver rejected the username or password".into(),
        (404, _) => {
            "the homeserver was not found (HTTP 404); check the homeserver URL".into()
        }
        (429, _) => "the homeserver is rate limiting logins".into(),
        _ => format!("the homeserver answered HTTP {status} to the login"),
    }
}

/// One login attempt. The username and password only go into the request body.
async fn login_once(
    http: &reqwest::Client,
    homeserver: &str,
    username: &str,
    password: &str,
) -> Result<String, DeliveryError> {
    let resp = http
        .post(api(homeserver, "login"))
        .timeout(TIMEOUT)
        .header(USER_AGENT, format!("clouduler/{}", cl_config::version()))
        .json(&json!({
            "type": "m.login.password",
            "identifier": { "type": "m.id.user", "user": username.trim() },
            "password": password,
            "initial_device_display_name": "Clouduler alerts",
        }))
        .send()
        .await
        .map_err(transport_error)?;
    if !resp.status().is_success() {
        let status = resp.status();
        let header_wait = retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        let code = status.as_u16();
        let wait = header_wait.or_else(|| {
            serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["retry_after_ms"].as_f64())
                .filter(|ms| ms.is_finite() && *ms >= 0.0)
                .map(|ms| Duration::from_secs_f64((ms / 1000.0).min(3600.0)))
        });
        return Err(DeliveryError {
            message: explain_login(code, &body),
            retryable: code == 429 || code == 408 || status.is_server_error(),
            retry_after: wait,
        });
    }
    let v: Value = resp.json().await.map_err(|_| DeliveryError {
        message: "the homeserver sent an unreadable answer to the login".into(),
        retryable: false,
        retry_after: None,
    })?;
    v["access_token"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| DeliveryError {
            message: "the homeserver did not return an access token".into(),
            retryable: false,
            retry_after: None,
        })
}

/// Log in with a username and password and return the access token. Retries
/// a 429, 408, 5xx or transport failure twice, waiting what the homeserver asked
/// (at most 5 seconds), because the caller is a person waiting on a Save button.
/// The error message never holds the password, the token or the URL.
pub async fn login(
    http: &reqwest::Client,
    homeserver: &str,
    username: &str,
    password: &str,
) -> Result<String, DeliveryError> {
    let mut attempt = 0u32;
    loop {
        match login_once(http, homeserver, username, password).await {
            Ok(t) => return Ok(t),
            Err(e) if e.retryable && attempt < 2 => {
                attempt += 1;
                let wait = e
                    .retry_after
                    .unwrap_or(Duration::from_millis(500 * u64::from(attempt)))
                    .min(Duration::from_secs(5));
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Best effort: drop the device a test login created. Failures are ignored.
pub async fn logout(http: &reqwest::Client, homeserver: &str, token: &str) {
    let Ok(auth) = bearer(token) else { return };
    let _ = http
        .post(api(homeserver, "logout"))
        .timeout(TIMEOUT)
        .header(USER_AGENT, format!("clouduler/{}", cl_config::version()))
        .header(AUTHORIZATION, auth)
        .json(&json!({}))
        .send()
        .await;
}

/// Sends `n` and returns the new event's ID. A resolved alert is an edit of
/// `edit_message_id` when `edit_resolved` is on; if the homeserver refuses the edit
/// (the event is gone, or not ours to edit) a new message is posted instead.
pub async fn send(
    http: &reqwest::Client,
    homeserver: &str,
    token: &str,
    opts: &MatrixOptions,
    n: &Notification,
    edit_message_id: Option<&str>,
) -> Result<Option<String>, DeliveryError> {
    let auth = bearer(token)?;
    let room = opts.room.trim();
    let room_id = if room.starts_with('#') {
        resolve_alias(http, homeserver, &auth, room).await?
    } else {
        room.to_string()
    };
    let put = |payload: Value| {
        let url = api(
            homeserver,
            &format!(
                "rooms/{}/send/m.room.message/{}",
                encode_segment(&room_id),
                txn_id()
            ),
        );
        http.put(url)
            .timeout(TIMEOUT)
            .header(USER_AGENT, format!("clouduler/{}", cl_config::version()))
            .header(AUTHORIZATION, auth.clone())
            .json(&payload)
            .send()
    };
    let msg = message(n, opts.style);
    let is_resolved = matches!(&n.body, Body::Alert(m) if m.phase == Phase::Resolved);
    if let Some(id) = edit_message_id.filter(|_| opts.edit_resolved && is_resolved) {
        let resp = put(msg.edit_payload(id)).await.map_err(transport_error)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(Some(id.to_string()));
        }
        if status.as_u16() == 429 || status.as_u16() == 408 || status.is_server_error() {
            return Err(failure(resp).await);
        }
    }
    let resp = put(msg.payload()).await.map_err(transport_error)?;
    if resp.status().is_success() {
        let id = resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["event_id"].as_str().map(str::to_string));
        return Ok(id);
    }
    Err(failure(resp).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deliver::testutil::*;

    fn message_full(n: &Notification) -> Message {
        message(n, MatrixStyle::Full)
    }

    #[test]
    fn encodes_room_ids_and_aliases_as_one_segment() {
        assert_eq!(encode_segment("!abc:example.org"), "%21abc%3Aexample.org");
        assert_eq!(encode_segment("#ops:example.org"), "%23ops%3Aexample.org");
        assert_eq!(encode_segment("a/b?c"), "a%2Fb%3Fc");
    }

    #[test]
    fn txn_ids_are_unique() {
        assert_ne!(txn_id(), txn_id());
    }

    #[test]
    fn api_url_tolerates_a_trailing_slash_and_a_path() {
        assert_eq!(
            api("https://m.example.org/", "rooms/x"),
            "https://m.example.org/_matrix/client/v3/rooms/x"
        );
        assert_eq!(
            api("https://example.org/matrix", "rooms/x"),
            "https://example.org/matrix/_matrix/client/v3/rooms/x"
        );
    }

    #[test]
    fn alert_message_has_text_and_escaped_html() {
        let mut a = alert(Severity::Critical, Phase::Opened);
        a.title = "<b>x</b> & y".into();
        let m = message_full(&notification(Body::Alert(a)));
        let p = m.payload();
        assert_eq!(p["msgtype"], "m.text");
        assert_eq!(p["format"], "org.matrix.custom.html");
        assert!(p["body"]
            .as_str()
            .unwrap()
            .contains("CRITICAL <b>x</b> & y"));
        let html = p["formatted_body"].as_str().unwrap();
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt; &amp; y"));
        assert!(!html.contains("<b>x"));
        assert!(html.contains("<a href=\"https://cloud.test/stacks/media\">Open in Clouduler</a>"));
        assert!(html.contains("<pre><code>panic: out of memory</code></pre>"));
    }

    #[test]
    fn details_are_quoted_and_the_cluster_is_left_out() {
        let m = message_full(&notification(Body::Alert(alert(
            Severity::Critical,
            Phase::Opened,
        ))));
        assert!(m.body.starts_with("\u{1F534} CRITICAL media-radarr-1 keeps crashing\n> Restarted 3 times"));
        assert!(m.body.contains("\n> stack media"));
        assert!(m.body.contains("\n> panic: out of memory"));
        assert!(!m.body.contains("piosny") && !m.formatted_body.contains("piosny"));
        assert!(!m.formatted_body.contains("<sub>"));
        let h = &m.formatted_body;
        let (q0, q1) = (h.find("<blockquote>").unwrap(), h.find("</blockquote>").unwrap());
        assert!(h.find("<strong>CRITICAL</strong>").unwrap() < q0);
        assert!(h[q0..q1].contains("Restarted 3 times") && h[q0..q1].contains("<pre><code>"));
        assert!(h.find("Open in Clouduler").unwrap() > q1);
    }

    #[test]
    fn edit_payload_replaces_the_original_event() {
        let m = message_full(&notification(Body::Alert(alert(
            Severity::Critical,
            Phase::Resolved,
        ))));
        let p = m.edit_payload("$orig");
        assert_eq!(p["m.relates_to"]["rel_type"], "m.replace");
        assert_eq!(p["m.relates_to"]["event_id"], "$orig");
        assert_eq!(p["m.new_content"]["body"], m.body);
        assert_eq!(p["body"], format!("* {}", m.body));
    }

    fn opts_edit(edit_resolved: bool) -> MatrixOptions {
        MatrixOptions {
            room: "!r:example.org".into(),
            style: MatrixStyle::Full,
            edit_resolved,
        }
    }

    #[tokio::test]
    async fn send_returns_the_event_id_and_edits_a_resolved_alert() {
        let (url, h) = serve_seq(vec![reply("200 OK", r#"{"event_id":"$new"}"#)]).await;
        let opened = notification(Body::Alert(alert(Severity::Critical, Phase::Opened)));
        let id = send(&reqwest::Client::new(), &url, "t", &opts_edit(true), &opened, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$new"));
        drop(h);

        let (url, h) = serve_seq(vec![reply("200 OK", r#"{"event_id":"$edit"}"#)]).await;
        let resolved = notification(Body::Alert(alert(Severity::Critical, Phase::Resolved)));
        let id = send(&reqwest::Client::new(), &url, "t", &opts_edit(true), &resolved, Some("$orig"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$orig"));
        let req = h.await.unwrap().remove(0);
        assert!(req.contains("\"m.replace\"") && req.contains("$orig"));

        // Off: a resolved alert is a new message.
        let (url, h) = serve_seq(vec![reply("200 OK", r#"{"event_id":"$fresh"}"#)]).await;
        let id = send(&reqwest::Client::new(), &url, "t", &opts_edit(false), &resolved, Some("$orig"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$fresh"));
        assert!(!h.await.unwrap().remove(0).contains("m.replace"));
    }

    #[tokio::test]
    async fn a_refused_edit_falls_back_to_a_new_message() {
        let (url, h) = serve_seq(vec![
            reply("404 Not Found", r#"{"errcode":"M_NOT_FOUND","error":"gone"}"#),
            reply("200 OK", r#"{"event_id":"$fresh"}"#),
        ])
        .await;
        let resolved = notification(Body::Alert(alert(Severity::Critical, Phase::Resolved)));
        let id = send(&reqwest::Client::new(), &url, "t", &opts_edit(true), &resolved, Some("$orig"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$fresh"));
        assert_eq!(h.await.unwrap().len(), 2);
    }

    #[test]
    fn non_http_links_are_dropped() {
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.links[0].url = "javascript:alert(1)".into();
        let m = message_full(&notification(Body::Alert(a)));
        assert!(!m.formatted_body.contains("javascript:"));
        assert!(!m.body.contains("javascript:"));
    }

    #[test]
    fn resolved_says_so_and_test_and_digest_render() {
        let m = message_full(&notification(Body::Alert(alert(
            Severity::Critical,
            Phase::Resolved,
        ))));
        assert!(m.body.contains("RESOLVED"));
        let t = message_full(&notification(Body::Test));
        assert!(t.body.contains("Clouduler test message"));
        assert!(t.body.contains("piosny"));
        let a = alert(Severity::Info, Phase::Event);
        let d = message_full(&notification(Body::Digest {
            alerts: vec![a.clone(), a],
        }));
        assert!(d.body.contains("2 info alerts"));
        assert_eq!(d.formatted_body.matches("<li>").count(), 2);
    }

    #[test]
    fn short_style_is_one_plain_line_with_where() {
        let mut a = alert(Severity::Critical, Phase::Opened);
        a.title = "<b>x</b> & y".into();
        a.instance = Some("media-radarr-1".into());
        let m = message(&notification(Body::Alert(a)), MatrixStyle::Short);
        assert_eq!(
            m.body,
            "\u{1F534} CRITICAL <b>x</b> & y \u{2014} stack media \u{b7} instance media-radarr-1 \u{b7} node pisoa"
        );
        assert!(!m.body.contains('\n'));
        assert!(!m.body.contains("Restarted"));
        assert!(!m.body.contains("cloud.test"));
        assert!(!m.body.contains("piosny"));
        assert!(m.formatted_body.contains("&lt;b&gt;x&lt;/b&gt; &amp; y"));
        assert!(!m.formatted_body.contains("<br>"));
        assert!(!m.formatted_body.contains("<pre>"));
        assert!(!m.formatted_body.contains("href"));
        assert!(m.formatted_body.contains("<strong>CRITICAL</strong>"));
    }

    #[test]
    fn short_style_resolved_without_facts_and_test_digest_stay_sensible() {
        let mut a = alert(Severity::Warning, Phase::Resolved);
        a.stack = None;
        a.node = None;
        let m = message(&notification(Body::Alert(a)), MatrixStyle::Short);
        assert_eq!(m.body, "\u{2705} RESOLVED media-radarr-1 keeps crashing");
        let t = message(&notification(Body::Test), MatrixStyle::Short);
        assert!(t.body.contains("Clouduler test message") && t.body.contains("piosny"));
        assert!(!t.body.contains('\n'));
        let a = alert(Severity::Info, Phase::Event);
        let d = notification(Body::Digest {
            alerts: vec![a.clone(), a],
        });
        assert_eq!(
            message(&d, MatrixStyle::Short),
            message(&d, MatrixStyle::Full)
        );
    }

    #[test]
    fn style_defaults_to_full_for_old_settings() {
        let o: MatrixOptions = serde_json::from_str(r#"{"room":"!r:example.org"}"#).unwrap();
        assert_eq!(o.style, MatrixStyle::Full);
        assert!(!o.edit_resolved);
        let o: MatrixOptions =
            serde_json::from_str(r#"{"room":"!r:example.org","style":"short"}"#).unwrap();
        assert_eq!(o.style, MatrixStyle::Short);
    }

    /// Serves one canned response per connection, in order, and returns the raw requests.
    async fn serve_seq(
        responses: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let mut seen = Vec::new();
            for response in responses {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut n = 0;
                loop {
                    let r = s.read(&mut buf[n..]).await.unwrap();
                    n += r;
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if n >= i + 4 + len || r == 0 {
                            break;
                        }
                    } else if r == 0 {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&buf[..n]).to_string());
                s.write_all(response.as_bytes()).await.unwrap();
            }
            seen
        });
        (url, h)
    }

    fn reply(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    fn login_ok() -> String {
        reply(
            "200 OK",
            r#"{"access_token":"syt_tok_123","user_id":"@bot:x","device_id":"D"}"#,
        )
    }

    #[tokio::test]
    async fn login_posts_the_password_request_and_returns_the_token() {
        let (url, h) = serve_seq(vec![login_ok()]).await;
        let t = login(&reqwest::Client::new(), &format!("{url}/"), " @bot:x ", "pw-secret")
            .await
            .unwrap();
        assert_eq!(t, "syt_tok_123");
        let req = h.await.unwrap().remove(0);
        assert!(req.starts_with("POST /_matrix/client/v3/login "));
        let body: Value = serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["type"], "m.login.password");
        assert_eq!(body["identifier"]["type"], "m.id.user");
        assert_eq!(body["identifier"]["user"], "@bot:x");
        assert_eq!(body["password"], "pw-secret");
        assert_eq!(body["initial_device_display_name"], "Clouduler alerts");
        assert!(!req.to_ascii_lowercase().contains("authorization:"));
    }

    #[tokio::test]
    async fn login_failures_are_clear_and_leak_nothing() {
        let forbidden = reply(
            "403 Forbidden",
            r#"{"errcode":"M_FORBIDDEN","error":"Invalid username or password pw-secret"}"#,
        );
        let (url, _h) = serve_seq(vec![forbidden]).await;
        let e = login(&reqwest::Client::new(), &url, "bot", "pw-secret")
            .await
            .unwrap_err();
        assert_eq!(e.message, "the homeserver rejected the username or password");
        assert!(!e.retryable);
        assert!(!e.message.contains("pw-secret") && !e.message.contains("127.0.0.1"));

        let no_token = reply("200 OK", "{}");
        let (url, _h) = serve_seq(vec![no_token]).await;
        let e = login(&reqwest::Client::new(), &url, "bot", "pw").await.unwrap_err();
        assert!(e.message.contains("did not return an access token"));

        // Nothing listens here: the error names no host, port or password.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let e = login(&reqwest::Client::new(), &dead, "bot", "pw-secret")
            .await
            .unwrap_err();
        assert!(e.message.starts_with("request failed"));
        assert!(!e.message.contains("pw-secret") && !e.message.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn login_retries_a_rate_limit_then_succeeds() {
        let limited = reply(
            "429 Too Many Requests",
            r#"{"errcode":"M_LIMIT_EXCEEDED","retry_after_ms":10,"error":"x"}"#,
        );
        let (url, h) = serve_seq(vec![limited, login_ok()]).await;
        let t = login(&reqwest::Client::new(), &url, "bot", "pw").await.unwrap();
        assert_eq!(t, "syt_tok_123");
        assert_eq!(h.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn logout_sends_the_bearer_token() {
        let (url, h) = serve_seq(vec![reply("200 OK", "{}")]).await;
        logout(&reqwest::Client::new(), &url, "syt_tok_123").await;
        let req = h.await.unwrap().remove(0);
        assert!(req.starts_with("POST /_matrix/client/v3/logout "));
        assert!(req.to_ascii_lowercase().contains("authorization: bearer syt_tok_123"));
    }

    #[test]
    fn login_errors_name_the_cause() {
        assert!(explain_login(403, "").contains("username or password"));
        assert!(explain_login(401, "").contains("username or password"));
        assert!(explain_login(404, "").contains("not found"));
        assert!(explain_login(400, r#"{"errcode":"M_UNRECOGNIZED"}"#).contains("does not offer"));
        assert!(explain_login(500, "").contains("HTTP 500"));
    }

    #[test]
    fn errors_never_carry_the_token_or_urls() {
        let e = explain(
            403,
            r#"{"errcode":"M_FORBIDDEN","error":"see https://x.test/tok-123"}"#,
        );
        assert!(!e.contains("tok-123"));
        assert!(explain(401, "").contains("access token"));
        assert!(explain(404, "").contains("not found"));
    }
}
