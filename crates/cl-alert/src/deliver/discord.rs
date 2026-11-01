//! Discord channel webhooks. See
//! <https://docs.discord.com/developers/resources/webhook#execute-webhook>.
//!
//! Every message sets `allowed_mentions` explicitly, so text coming from a
//! log line or a stack name can never ping anyone.

use std::time::Duration;

use reqwest::header::USER_AGENT;
use serde_json::{json, Value};

use super::util::*;
use super::webhook::{retry_after, transport_error};
use super::{DeliveryError, TIMEOUT};
use crate::message::{AlertMessage, Body, Notification, Phase};
use crate::settings::{DiscordNotify, DiscordOptions, DiscordStyle, Severity};

const TITLE_MAX: usize = 256;
const DESCRIPTION_MAX: usize = 4096;
const FIELD_VALUE_MAX: usize = 1024;
const FIELDS_MAX: usize = 25;
const EMBED_TOTAL_MAX: usize = 6000;
const EMBEDS_PER_MESSAGE: usize = 10;
const CONTENT_MAX: usize = 2000;
const BUTTONS_MAX: usize = 5;
const FOOTER_MAX: usize = 2048;

const FLAG_SUPPRESS_EMBEDS: u64 = 1 << 2;
const FLAG_SUPPRESS_NOTIFICATIONS: u64 = 1 << 12;

const COLOR_CRITICAL: u32 = 0xE5_48_4D;
const COLOR_WARNING: u32 = 0xF5_A6_23;
const COLOR_INFO: u32 = 0x8E_8A_9C;
const COLOR_RESOLVED: u32 = 0x2F_D0_A8;
const COLOR_TEST: u32 = 0xFF_5C_AE;

fn color(m: &AlertMessage) -> u32 {
    if m.phase == Phase::Resolved {
        return COLOR_RESOLVED;
    }
    match m.severity {
        Severity::Critical => COLOR_CRITICAL,
        Severity::Warning => COLOR_WARNING,
        Severity::Info => COLOR_INFO,
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

/// Keep a block of text from closing or opening a markdown code fence.
fn defence(s: &str) -> String {
    s.replace("```", "``\u{200B}`")
}

/// Escape markdown so a title like `a_b_c` or `**x**` shows as written.
fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// What a ping resolves to: the text to put in `content` and the exact
/// `allowed_mentions` that makes it fire.
struct Mention {
    text: String,
    allowed: Value,
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.len() <= 25 && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_mention(raw: &str) -> Option<Mention> {
    let m = raw.trim();
    if m == "@everyone" || m == "@here" {
        return Some(Mention {
            text: m.to_string(),
            allowed: json!({ "parse": ["everyone"] }),
        });
    }
    let inner = m.strip_prefix("<@")?.strip_suffix('>')?;
    if let Some(id) = inner.strip_prefix('&') {
        return all_digits(id).then(|| Mention {
            text: format!("<@&{id}>"),
            allowed: json!({ "parse": [], "roles": [id] }),
        });
    }
    let id = inner.strip_prefix('!').unwrap_or(inner);
    all_digits(id).then(|| Mention {
        text: format!("<@{id}>"),
        allowed: json!({ "parse": [], "users": [id] }),
    })
}

fn notify_level(opts: &DiscordOptions, body: &Body) -> DiscordNotify {
    match body {
        Body::Alert(m) if m.phase == Phase::Resolved => opts.notify.resolved,
        Body::Alert(m) => match m.severity {
            Severity::Critical => opts.notify.critical,
            Severity::Warning => opts.notify.warning,
            Severity::Info => opts.notify.info,
        },
        Body::Digest { .. } => opts.notify.info,
        // A test must never wake anyone up, and must be visible.
        Body::Test => DiscordNotify::Normal,
    }
}

/// One request to the webhook.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub payload: Value,
}

impl Message {
    fn has_components(&self) -> bool {
        self.payload.get("components").is_some()
    }
}

struct Envelope {
    mention: Option<Mention>,
    flags: u64,
}

impl Envelope {
    fn new(opts: &DiscordOptions, body: &Body) -> Self {
        match notify_level(opts, body) {
            DiscordNotify::Ping => Envelope {
                mention: opts.mention.as_deref().and_then(parse_mention),
                flags: 0,
            },
            DiscordNotify::Normal => Envelope {
                mention: None,
                flags: 0,
            },
            DiscordNotify::Silent => Envelope {
                mention: None,
                flags: FLAG_SUPPRESS_NOTIFICATIONS,
            },
        }
    }

    fn finish(&self, opts: &DiscordOptions, ui_url: Option<&str>, mut payload: Value) -> Message {
        let o = payload.as_object_mut().expect("payload is an object");
        o.insert(
            "allowed_mentions".into(),
            match &self.mention {
                Some(m) => m.allowed.clone(),
                None => json!({ "parse": [] }),
            },
        );
        if self.flags != 0 {
            let f = o.get("flags").and_then(Value::as_u64).unwrap_or(0) | self.flags;
            o.insert("flags".into(), f.into());
        }
        if let Some(name) = opts
            .username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            o.insert("username".into(), truncate(name, 80).into());
        }
        if let Some(u) = ui_url.filter(|u| is_http(u)) {
            o.insert("avatar_url".into(), format!("{u}/icon-192.png").into());
        }
        Message { payload }
    }

    fn content_prefix(&self) -> String {
        self.mention
            .as_ref()
            .map(|m| format!("{} ", m.text))
            .unwrap_or_default()
    }
}

fn buttons(opts: &DiscordOptions, links: &[(String, String)]) -> Option<Value> {
    if !opts.buttons {
        return None;
    }
    let comps: Vec<Value> = links
        .iter()
        .filter(|(_, url)| is_http(url) && url.len() <= 512)
        .take(BUTTONS_MAX)
        .map(|(label, url)| {
            json!({ "type": 2, "style": 5, "label": truncate(label, 80), "url": url })
        })
        .collect();
    (!comps.is_empty()).then(|| json!([{ "type": 1, "components": comps }]))
}

fn alert_links(m: &AlertMessage) -> Vec<(String, String)> {
    m.links
        .iter()
        .map(|l| (l.label.clone(), l.url.clone()))
        .collect()
}

fn embed_chars(e: &Value) -> usize {
    let s = |v: &Value| v.as_str().map(|s| s.chars().count()).unwrap_or(0);
    let mut n = s(&e["title"]) + s(&e["description"]) + s(&e["footer"]["text"]);
    if let Some(fs) = e["fields"].as_array() {
        for f in fs {
            n += s(&f["name"]) + s(&f["value"]);
        }
    }
    n
}

fn field(name: &str, value: &str, inline: bool) -> Value {
    json!({ "name": name, "value": truncate(value, FIELD_VALUE_MAX), "inline": inline })
}

fn alert_embed(m: &AlertMessage, cluster: &str) -> Value {
    let mut fields = Vec::new();
    if let Some(n) = m.node.as_deref().filter(|s| !s.is_empty()) {
        fields.push(field("Node", n, true));
    }
    if let Some(s) = m.stack.as_deref().filter(|s| !s.is_empty()) {
        fields.push(field("Stack", s, true));
    }
    if let Some(i) = m.instance.as_deref().filter(|s| !s.is_empty()) {
        fields.push(field("Instance", i, true));
    }
    let shown = [&m.node, &m.stack, &m.instance]
        .iter()
        .any(|o| o.as_deref() == Some(m.subject.as_str()));
    if !m.subject.is_empty() && !shown {
        fields.push(field("Subject", &m.subject, true));
    }
    if let Some(d) = open_for(m) {
        fields.push(field("Was open for", &d, true));
    }
    if m.count > 1 {
        fields.push(field("Occurrences", &m.count.to_string(), true));
    }
    if let Phase::Escalated { previous } = m.phase {
        fields.push(field("Was", severity_word(previous), true));
    }
    fields.truncate(FIELDS_MAX);

    let title = truncate(&title_with_phase(m), TITLE_MAX);
    let footer = truncate(&format!("{cluster} \u{b7} {}", m.event), FOOTER_MAX);
    let fixed: usize = title.chars().count()
        + footer.chars().count()
        + fields
            .iter()
            .map(|f| embed_chars(&json!({ "fields": [f] })))
            .sum::<usize>();

    let mut excerpt = String::new();
    if !m.excerpt.is_empty() {
        excerpt = defence(&m.excerpt.join("\n"));
    }
    // The description is what is left of the 6000 character budget.
    let budget = DESCRIPTION_MAX.min(EMBED_TOTAL_MAX.saturating_sub(fixed));
    let description = if excerpt.is_empty() {
        truncate(&m.detail, budget)
    } else {
        let code_overhead = "\n```\n\n```".chars().count();
        let code_budget = budget.saturating_sub(code_overhead).min(1500);
        let code = truncate(&excerpt, code_budget);
        let detail_budget = budget.saturating_sub(code.chars().count() + code_overhead);
        let detail = truncate(&m.detail, detail_budget);
        if detail.is_empty() {
            format!("```\n{code}\n```")
        } else {
            format!("{detail}\n```\n{code}\n```")
        }
    };

    json!({
        "title": title,
        "description": description,
        "color": color(m),
        "fields": fields,
        "footer": { "text": footer },
        "timestamp": rfc3339(m.at),
    })
}

fn short_line(m: &AlertMessage, prefix: &str, link: Option<&str>) -> String {
    let word = if m.phase == Phase::Resolved {
        "RESOLVED".to_string()
    } else {
        severity_word(m.severity).to_uppercase()
    };
    let tail_len = link.map(|l| l.chars().count() + 1).unwrap_or(0);
    let head = format!("{prefix}{} **{word}** ", emoji(m));
    let mut body = escape_markdown(&m.title);
    if !m.detail.is_empty() {
        body.push_str(" \u{2014} ");
        body.push_str(&escape_markdown(&m.detail.replace('\n', " ")));
    }
    if let Some(n) = m.node.as_deref().filter(|n| !m.title.contains(n)) {
        body.push_str(&format!(" ({})", escape_markdown(n)));
    }
    let room = CONTENT_MAX
        .saturating_sub(head.chars().count() + tail_len)
        .max(1);
    let mut out = format!("{head}{}", truncate(&body, room));
    if let Some(l) = link {
        out.push(' ');
        out.push_str(l);
    }
    out
}

fn test_messages(opts: &DiscordOptions, n: &Notification) -> Vec<Message> {
    let env = Envelope::new(opts, &n.body);
    let text = format!("Alerts for cluster {} will arrive here.", n.cluster);
    let payload = match opts.style {
        DiscordStyle::Embed => json!({
            "embeds": [{
                "title": "Clouduler test message",
                "description": text,
                "color": COLOR_TEST,
                "footer": { "text": truncate(&format!("{} \u{b7} test", n.cluster), FOOTER_MAX) },
            }],
        }),
        DiscordStyle::Short => json!({
            "content": format!("\u{2705} **TEST** Clouduler test message \u{2014} {}", escape_markdown(&text)),
            "flags": FLAG_SUPPRESS_EMBEDS,
        }),
    };
    vec![env.finish(opts, n.ui_url.as_deref(), payload)]
}

fn alert_message(opts: &DiscordOptions, n: &Notification, m: &AlertMessage) -> Message {
    let env = Envelope::new(opts, &n.body);
    let links = alert_links(m);
    let mut payload = match opts.style {
        DiscordStyle::Embed => {
            let mut p = json!({ "embeds": [alert_embed(m, &n.cluster)] });
            if !env.content_prefix().is_empty() {
                p["content"] = env.content_prefix().trim_end().into();
            }
            p
        }
        DiscordStyle::Short => {
            // With buttons the link is redundant; without them it is the only way in.
            let inline = if opts.buttons {
                None
            } else {
                primary_link(m).map(|l| l.url.as_str())
            };
            json!({
                "content": short_line(m, &env.content_prefix(), inline),
                "flags": FLAG_SUPPRESS_EMBEDS,
            })
        }
    };
    if let Some(c) = buttons(opts, &links) {
        payload["components"] = c;
    }
    env.finish(opts, n.ui_url.as_deref(), payload)
}

fn digest_line(m: &AlertMessage) -> String {
    let mut s = format!("`{}` {}", clock(m.at), escape_markdown(&m.title));
    if !m.subject.is_empty() && !m.title.contains(&m.subject) {
        s.push_str(&format!(" \u{2014} {}", escape_markdown(&m.subject)));
    }
    if m.count > 1 {
        s.push_str(&format!(" \u{d7}{}", m.count));
    }
    truncate(&s, 300)
}

fn digest_messages(
    opts: &DiscordOptions,
    n: &Notification,
    alerts: &[AlertMessage],
) -> Vec<Message> {
    let env = Envelope::new(opts, &n.body);
    let total = alerts.len();
    let heading = format!(
        "Daily digest \u{b7} {total} event{}",
        if total == 1 { "" } else { "s" }
    );
    let button = n
        .ui_url
        .as_deref()
        .filter(|u| is_http(u))
        .and_then(|u| buttons(opts, &[("Open alerts".to_string(), format!("{u}/alerts"))]));
    let last_at = alerts.iter().map(|a| a.at).max();

    match opts.style {
        DiscordStyle::Short => {
            // Lines of plain text, split on the 2000 character limit.
            let mut chunks: Vec<String> = Vec::new();
            let mut cur = format!("{} **{heading}**", emoji_info());
            for a in alerts {
                let line = format!("\n\u{2022} {}", digest_line(a));
                if cur.chars().count() + line.chars().count() > CONTENT_MAX {
                    chunks.push(std::mem::take(&mut cur));
                    cur = line.trim_start().to_string();
                } else {
                    cur.push_str(&line);
                }
            }
            chunks.push(cur);
            let count = chunks.len();
            chunks
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut p = json!({ "content": c, "flags": FLAG_SUPPRESS_EMBEDS });
                    if i + 1 == count {
                        if let Some(b) = &button {
                            p["components"] = b.clone();
                        }
                    }
                    env.finish(opts, n.ui_url.as_deref(), p)
                })
                .collect()
        }
        DiscordStyle::Embed => {
            let footer = truncate(&format!("{} \u{b7} digest", n.cluster), FOOTER_MAX);
            // One embed per run of lines that fits a description.
            let mut descriptions: Vec<String> = Vec::new();
            let mut cur = String::new();
            for a in alerts {
                let line = format!("\u{2022} {}", digest_line(a));
                if !cur.is_empty() && cur.chars().count() + 1 + line.chars().count() > 3500 {
                    descriptions.push(std::mem::take(&mut cur));
                }
                if !cur.is_empty() {
                    cur.push('\n');
                }
                cur.push_str(&line);
            }
            descriptions.push(cur);

            let mut messages: Vec<Vec<Value>> = vec![Vec::new()];
            let mut size = 0usize;
            for (i, d) in descriptions.into_iter().enumerate() {
                let title = if i == 0 {
                    heading.clone()
                } else {
                    format!("{heading} (continued)")
                };
                let mut e = json!({
                    "title": truncate(&title, TITLE_MAX),
                    "description": d,
                    "color": COLOR_INFO,
                    "footer": { "text": footer },
                });
                if let Some(t) = last_at {
                    e["timestamp"] = rfc3339(t).into();
                }
                let sz = embed_chars(&e);
                let full = messages.last().map(Vec::len).unwrap_or(0) >= EMBEDS_PER_MESSAGE;
                if full || size + sz > EMBED_TOTAL_MAX {
                    messages.push(Vec::new());
                    size = 0;
                }
                size += sz;
                messages.last_mut().expect("one message").push(e);
            }
            let count = messages.len();
            messages
                .into_iter()
                .enumerate()
                .map(|(i, embeds)| {
                    let mut p = json!({ "embeds": embeds });
                    if i + 1 == count {
                        if let Some(b) = &button {
                            p["components"] = b.clone();
                        }
                    }
                    env.finish(opts, n.ui_url.as_deref(), p)
                })
                .collect()
        }
    }
}

fn emoji_info() -> &'static str {
    "\u{1F4CB}"
}

/// All requests needed to deliver `n`, in order. Pure.
pub fn messages(opts: &DiscordOptions, n: &Notification) -> Vec<Message> {
    match &n.body {
        Body::Test => test_messages(opts, n),
        Body::Alert(m) => vec![alert_message(opts, n, m)],
        Body::Digest { alerts } => digest_messages(opts, n, alerts),
    }
}

/// What Discord says went wrong, without anything that could carry the token.
fn explain(status: u16, body: &str) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(|s| truncate(s, 200)))
        .filter(|s| !s.contains("http"));
    match (status, said) {
        (401 | 404, _) => format!(
            "Discord says the webhook does not exist (HTTP {status}); it was probably deleted or the URL is wrong"
        ),
        (_, Some(m)) => format!("Discord rejected the message (HTTP {status}): {m}"),
        (_, None) => format!("Discord answered HTTP {status}"),
    }
}

pub async fn send(
    http: &reqwest::Client,
    url: &str,
    opts: &DiscordOptions,
    n: &Notification,
) -> Result<(), DeliveryError> {
    for message in messages(opts, n) {
        let mut req = http.post(url).timeout(TIMEOUT).header(
            USER_AGENT,
            format!("clouduler/{}", env!("CARGO_PKG_VERSION")),
        );
        // Link buttons are only accepted on a webhook when asked for.
        if message.has_components() {
            req = req.query(&[("with_components", "true")]);
        }
        let resp = req
            .json(&message.payload)
            .send()
            .await
            .map_err(transport_error)?;
        let status = resp.status();
        if status.is_success() {
            continue;
        }
        let header_wait = retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        let wait = header_wait.or_else(|| {
            serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["retry_after"].as_f64())
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(|s| Duration::from_secs_f64(s.min(3600.0)))
        });
        let code = status.as_u16();
        return Err(DeliveryError {
            message: if code == 429 {
                "Discord is rate limiting this webhook".to_string()
            } else {
                explain(code, &body)
            },
            retryable: code == 429 || code == 408 || status.is_server_error(),
            retry_after: wait,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deliver::testutil::*;

    fn one(opts: &DiscordOptions, body: Body) -> Value {
        let mut ms = messages(opts, &notification(body));
        assert_eq!(ms.len(), 1);
        ms.remove(0).payload
    }

    fn opts(style: DiscordStyle) -> DiscordOptions {
        DiscordOptions {
            style,
            mention: Some("<@&1234>".into()),
            ..DiscordOptions::default()
        }
    }

    #[test]
    fn critical_embed_pings_the_role() {
        let p = one(
            &opts(DiscordStyle::Embed),
            Body::Alert(alert(Severity::Critical, Phase::Opened)),
        );
        assert_eq!(p["content"], "<@&1234>");
        assert_eq!(
            p["allowed_mentions"],
            json!({ "parse": [], "roles": ["1234"] })
        );
        assert!(p.get("flags").is_none());
        let e = &p["embeds"][0];
        assert_eq!(e["color"], COLOR_CRITICAL);
        assert_eq!(e["title"], "media-radarr-1 keeps crashing");
        assert_eq!(e["footer"]["text"], "piosny \u{b7} instance.crash_loop");
        assert_eq!(e["timestamp"], "2026-07-02T09:12:00.000Z");
        let names: Vec<_> = e["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Node", "Stack", "Subject", "Occurrences"]);
        assert!(e["description"]
            .as_str()
            .unwrap()
            .contains("```\npanic: out of memory\n```"));
        assert_eq!(p["avatar_url"], "https://cloud.test/icon-192.png");
    }

    #[test]
    fn warning_is_normal_and_does_not_ping() {
        let p = one(
            &opts(DiscordStyle::Embed),
            Body::Alert(alert(Severity::Warning, Phase::Opened)),
        );
        assert!(p.get("content").is_none());
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
        assert!(p.get("flags").is_none());
        assert_eq!(p["embeds"][0]["color"], COLOR_WARNING);
    }

    #[test]
    fn info_and_resolved_are_silent_by_default() {
        let p = one(
            &opts(DiscordStyle::Embed),
            Body::Alert(alert(Severity::Info, Phase::Event)),
        );
        assert_eq!(p["flags"], FLAG_SUPPRESS_NOTIFICATIONS);
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
        let p = one(
            &opts(DiscordStyle::Embed),
            Body::Alert(alert(Severity::Critical, Phase::Resolved)),
        );
        assert_eq!(p["flags"], FLAG_SUPPRESS_NOTIFICATIONS);
        assert!(p.get("content").is_none());
        let e = &p["embeds"][0];
        assert_eq!(e["color"], COLOR_RESOLVED);
        assert_eq!(e["title"], "Resolved: media-radarr-1 keeps crashing");
        assert!(e["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "Was open for" && f["value"] == "10m"));
    }

    #[test]
    fn escalation_title_and_field() {
        let p = one(
            &opts(DiscordStyle::Embed),
            Body::Alert(alert(
                Severity::Critical,
                Phase::Escalated {
                    previous: Severity::Warning,
                },
            )),
        );
        assert_eq!(
            p["embeds"][0]["title"],
            "Escalated to critical: media-radarr-1 keeps crashing"
        );
        assert!(p["embeds"][0]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "Was" && f["value"] == "warning"));
    }

    #[test]
    fn mention_forms() {
        let user = parse_mention("<@!42>").unwrap();
        assert_eq!(user.text, "<@42>");
        assert_eq!(user.allowed, json!({ "parse": [], "users": ["42"] }));
        let here = parse_mention("@here").unwrap();
        assert_eq!(here.allowed, json!({ "parse": ["everyone"] }));
        assert!(parse_mention("@someone").is_none());
        assert!(parse_mention("<@&abc>").is_none());
        assert!(parse_mention("<@&1> <@&2>").is_none());
    }

    #[test]
    fn ping_without_a_valid_mention_falls_back_to_normal() {
        let mut o = opts(DiscordStyle::Embed);
        o.mention = None;
        let p = one(&o, Body::Alert(alert(Severity::Critical, Phase::Opened)));
        assert!(p.get("content").is_none());
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
        o.mention = Some("everyone please".into());
        let p = one(&o, Body::Alert(alert(Severity::Critical, Phase::Opened)));
        assert!(p.get("content").is_none());
    }

    #[test]
    fn alert_text_can_never_mention() {
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.title = "@everyone <@&99> look".into();
        a.detail = "@here".into();
        for style in [DiscordStyle::Embed, DiscordStyle::Short] {
            let p = one(&opts(style), Body::Alert(a.clone()));
            assert_eq!(p["allowed_mentions"], json!({ "parse": [] }), "{style:?}");
        }
        let mut o = opts(DiscordStyle::Embed);
        o.notify.warning = DiscordNotify::Silent;
        let p = one(&o, Body::Alert(a));
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
    }

    #[test]
    fn short_style_is_one_line() {
        let p = one(
            &opts(DiscordStyle::Short),
            Body::Alert(alert(Severity::Critical, Phase::Opened)),
        );
        let c = p["content"].as_str().unwrap();
        assert!(
            c.starts_with("<@&1234> \u{1F534} **CRITICAL** media-radarr-1 keeps crashing"),
            "{c}"
        );
        assert!(c.contains(" \u{2014} Restarted 3 times in 10 minutes."));
        assert!(c.contains("(pisoa)"));
        assert!(!c.contains('\n'));
        assert_eq!(p["flags"], FLAG_SUPPRESS_EMBEDS);
        assert!(p.get("embeds").is_none());
        // Buttons are on, so the link is not repeated in the text.
        assert!(!c.contains("https://"));
        assert_eq!(
            p["components"][0]["components"][0]["label"],
            "Open in Clouduler"
        );
    }

    #[test]
    fn short_style_without_buttons_carries_the_link_and_stays_silent_when_resolved() {
        let mut o = opts(DiscordStyle::Short);
        o.buttons = false;
        let p = one(&o, Body::Alert(alert(Severity::Warning, Phase::Resolved)));
        let c = p["content"].as_str().unwrap();
        assert!(c.starts_with("\u{2705} **RESOLVED**"), "{c}");
        assert!(c.ends_with(" https://cloud.test/stacks/media"));
        assert_eq!(
            p["flags"],
            FLAG_SUPPRESS_EMBEDS | FLAG_SUPPRESS_NOTIFICATIONS
        );
        assert!(p.get("components").is_none());
    }

    #[test]
    fn short_style_escapes_markdown_and_fits_2000() {
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.title = "**bold** _x_".into();
        a.detail = "d".repeat(5000);
        let p = one(&opts(DiscordStyle::Short), Body::Alert(a));
        let c = p["content"].as_str().unwrap();
        assert!(c.contains("\\*\\*bold\\*\\* \\_x\\_"));
        assert!(c.chars().count() <= CONTENT_MAX);
        assert!(c.contains('\u{2026}'));
    }

    #[test]
    fn buttons_are_link_buttons_and_only_http() {
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.links = (0..8)
            .map(|i| Link {
                label: format!("L{i}"),
                url: format!("https://cloud.test/{i}"),
            })
            .collect();
        a.links.insert(
            0,
            Link {
                label: "bad".into(),
                url: "javascript:alert(1)".into(),
            },
        );
        a.links.insert(
            1,
            Link {
                label: "rel".into(),
                url: "/stacks/x".into(),
            },
        );
        let p = one(&opts(DiscordStyle::Embed), Body::Alert(a));
        let row = &p["components"][0];
        assert_eq!(row["type"], 1);
        let comps = row["components"].as_array().unwrap();
        assert_eq!(comps.len(), 5);
        for c in comps {
            assert_eq!(c["type"], 2);
            assert_eq!(c["style"], 5);
            assert!(c["url"].as_str().unwrap().starts_with("https://"));
            assert!(c.get("custom_id").is_none());
        }
        assert_eq!(comps[0]["label"], "L0");
    }

    #[test]
    fn buttons_off_or_no_absolute_url_means_no_components() {
        let mut o = opts(DiscordStyle::Embed);
        o.buttons = false;
        let p = one(&o, Body::Alert(alert(Severity::Warning, Phase::Opened)));
        assert!(p.get("components").is_none());
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.links = vec![Link {
            label: "x".into(),
            url: "/stacks/media".into(),
        }];
        let p = one(&opts(DiscordStyle::Embed), Body::Alert(a));
        assert!(p.get("components").is_none());
    }

    #[test]
    fn username_and_avatar() {
        let mut o = opts(DiscordStyle::Embed);
        o.username = Some("  Piosny alerts ".into());
        let p = one(&o, Body::Alert(alert(Severity::Warning, Phase::Opened)));
        assert_eq!(p["username"], "Piosny alerts");
        let mut n = notification(Body::Alert(alert(Severity::Warning, Phase::Opened)));
        n.ui_url = None;
        let p = &messages(&o, &n)[0].payload;
        assert!(p.get("avatar_url").is_none());
    }

    #[test]
    fn embed_respects_limits() {
        let mut a = alert(Severity::Critical, Phase::Opened);
        a.title = "t".repeat(1000);
        a.detail = "d".repeat(9000);
        a.subject = "s".repeat(3000);
        a.excerpt = vec!["x".repeat(8000)];
        let p = one(&opts(DiscordStyle::Embed), Body::Alert(a));
        let e = &p["embeds"][0];
        assert!(e["title"].as_str().unwrap().chars().count() <= TITLE_MAX);
        assert!(e["description"].as_str().unwrap().chars().count() <= DESCRIPTION_MAX);
        assert!(embed_chars(e) <= EMBED_TOTAL_MAX, "{}", embed_chars(e));
        for f in e["fields"].as_array().unwrap() {
            assert!(f["value"].as_str().unwrap().chars().count() <= FIELD_VALUE_MAX);
        }
        // The code block stays well formed.
        let d = e["description"].as_str().unwrap();
        assert_eq!(d.matches("```").count(), 2);
    }

    #[test]
    fn excerpt_cannot_break_out_of_the_code_block() {
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.excerpt = vec!["before ``` @everyone after".into(), "second".into()];
        let p = one(&opts(DiscordStyle::Embed), Body::Alert(a));
        let d = p["embeds"][0]["description"].as_str().unwrap();
        assert_eq!(d.matches("```").count(), 2, "{d}");
        assert!(d.contains("second"));
    }

    #[test]
    fn test_message() {
        let p = one(&opts(DiscordStyle::Embed), Body::Test);
        assert_eq!(p["embeds"][0]["title"], "Clouduler test message");
        assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
        assert!(p.get("content").is_none());
        let p = one(&opts(DiscordStyle::Short), Body::Test);
        assert!(p["content"].as_str().unwrap().contains("TEST"));
    }

    fn digest_of(n: usize) -> Body {
        Body::Digest {
            alerts: (0..n)
                .map(|i| {
                    let mut a = alert(Severity::Info, Phase::Event);
                    a.title = format!("Stack deployed: stack-{i}");
                    a.subject = format!("stack-{i}");
                    a.at += i as i64 * 60;
                    a
                })
                .collect(),
        }
    }

    #[test]
    fn small_digest_is_one_embed_silent() {
        let ms = messages(&opts(DiscordStyle::Embed), &notification(digest_of(3)));
        assert_eq!(ms.len(), 1);
        let p = &ms[0].payload;
        let e = &p["embeds"][0];
        assert_eq!(e["title"], "Daily digest \u{b7} 3 events");
        assert_eq!(e["description"].as_str().unwrap().lines().count(), 3);
        assert_eq!(p["flags"], FLAG_SUPPRESS_NOTIFICATIONS);
        assert_eq!(
            p["components"][0]["components"][0]["url"],
            "https://cloud.test/alerts"
        );
    }

    #[test]
    fn big_digest_is_split_within_limits() {
        let ms = messages(&opts(DiscordStyle::Embed), &notification(digest_of(400)));
        assert!(ms.len() > 1, "{}", ms.len());
        let mut lines = 0;
        for (i, m) in ms.iter().enumerate() {
            let embeds = m.payload["embeds"].as_array().unwrap();
            assert!(embeds.len() <= EMBEDS_PER_MESSAGE);
            assert!(embeds.iter().map(embed_chars).sum::<usize>() <= EMBED_TOTAL_MAX);
            for e in embeds {
                assert!(e["description"].as_str().unwrap().chars().count() <= DESCRIPTION_MAX);
                lines += e["description"].as_str().unwrap().lines().count();
            }
            // The button sits on the last message only.
            assert_eq!(m.payload.get("components").is_some(), i + 1 == ms.len());
            assert_eq!(m.payload["allowed_mentions"], json!({ "parse": [] }));
        }
        assert_eq!(lines, 400);
    }

    #[test]
    fn short_digest_splits_on_2000_characters() {
        let ms = messages(&opts(DiscordStyle::Short), &notification(digest_of(200)));
        assert!(ms.len() > 1);
        for m in &ms {
            assert!(m.payload["content"].as_str().unwrap().chars().count() <= CONTENT_MAX);
        }
        let all: String = ms
            .iter()
            .map(|m| m.payload["content"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(all.matches('\u{2022}').count(), 200);
    }

    #[test]
    fn errors_never_carry_the_url() {
        assert!(explain(404, "{\"message\":\"Unknown Webhook\"}").contains("does not exist"));
        assert_eq!(
            explain(400, "{\"message\":\"Invalid Form Body\"}"),
            "Discord rejected the message (HTTP 400): Invalid Form Body"
        );
        assert_eq!(
            explain(400, "https://discord.com/api/webhooks/1/tok"),
            "Discord answered HTTP 400"
        );
        assert_eq!(
            explain(
                400,
                "{\"message\":\"see https://discord.com/api/webhooks/1/tok\"}"
            ),
            "Discord answered HTTP 400"
        );
    }
}
