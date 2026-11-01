//! Email through SMTP (lettre, rustls only). Every message is multipart:
//! plain text for clients and filters that want it, and a table-based HTML
//! rendering in the Clouduler colours (the same tokens as `ui/src/theme.css`,
//! light variant, since mail clients rarely honour dark schemes).

use std::fmt::Write as _;

use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};
use rust_embed::RustEmbed;

use super::util::*;
use super::{DeliveryError, TIMEOUT};
use crate::message::{AlertMessage, Body, Notification, Phase};
use crate::settings::{Severity, SmtpSecurity, SmtpSettings};

/// The HTML fragments, one file each, embedded like the web UI is.
#[derive(RustEmbed)]
#[folder = "templates/email"]
struct Templates;

/// One template file by name. They are compiled in; a missing one is a bug
/// that the tests catch.
fn tpl(name: &str) -> String {
    Templates::get(name)
        .map(|f| String::from_utf8_lossy(&f.data).into_owned())
        .unwrap_or_default()
}

/// Fill the template file `name`.
fn render_tpl(name: &str, values: &[(&str, &str)]) -> String {
    fill(&tpl(name), values)
}

/// A rendered message, before it is wrapped in MIME.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub subject: String,
    pub text: String,
    pub html: String,
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// Single-pass `{{KEY}}` substitution. Substituted text is never scanned
/// again, so a log line containing `{{BODY}}` stays a log line.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 512);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = &after[..end];
                if let Some((_, v)) = values.iter().find(|(k, _)| *k == key) {
                    out.push_str(v);
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pill(m: &AlertMessage) -> (&'static str, &'static str, &'static str) {
    if m.phase == Phase::Resolved {
        return ("Resolved", "#d5efe6", "#00695a");
    }
    match m.severity {
        Severity::Critical => ("Critical", "#fbdcd2", "#b4300a"),
        Severity::Warning => ("Warning", "#ffe8b0", "#7a5200"),
        Severity::Info => ("Info", "#f6e8cc", "#5a4c73"),
    }
}

pub fn subject(n: &Notification) -> String {
    let c = one_line(&n.cluster);
    let s = match &n.body {
        Body::Alert(m) => {
            let title = one_line(&m.title);
            match m.phase {
                Phase::Resolved => format!("[{c}] Resolved: {title}"),
                Phase::Escalated { .. } => format!(
                    "[{c}] Escalated to {}: {title}",
                    severity_word(m.severity).to_uppercase()
                ),
                _ => format!(
                    "[{c}] {}: {title}",
                    severity_word(m.severity).to_uppercase()
                ),
            }
        }
        Body::Digest { alerts } => format!("[{c}] Daily digest: {} events", alerts.len()),
        Body::Test => format!("[{c}] Test message from Clouduler"),
    };
    truncate(&s, 200)
}

fn logo(ui_url: Option<&str>) -> String {
    match ui_url.filter(|u| is_http(u)) {
        Some(u) => render_tpl("logo_img.html", &[("URL", &escape(u))]),
        None => tpl("logo_fallback.html"),
    }
}

fn rows(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| render_tpl("row.html", &[("KEY", &escape(k)), ("VALUE", &escape(v))]))
        .collect()
}

fn buttons(links: &[(String, String)]) -> String {
    let mut ok = links.iter().filter(|(_, u)| is_http(u));
    let Some((label, url)) = ok.next() else {
        return String::new();
    };
    let secondary: String = ok
        .take(3)
        .map(|(l, u)| {
            render_tpl(
                "button_secondary.html",
                &[("URL", &escape(u)), ("LABEL", &escape(l))],
            )
        })
        .collect();
    render_tpl(
        "button.html",
        &[
            ("URL", &escape(url)),
            ("LABEL", &escape(label)),
            ("SECONDARY", &secondary),
        ],
    )
}

fn footer(n: &Notification, tag: &str) -> (String, String) {
    let manage = n
        .ui_url
        .as_deref()
        .filter(|u| is_http(u))
        .map(|u| format!("{u}/alerts"));
    let manage_html = manage
        .as_deref()
        .map(|u| render_tpl("footer_manage.html", &[("URL", &escape(u))]))
        .unwrap_or_default();
    let html = render_tpl(
        "footer.html",
        &[
            ("CLUSTER", &escape(&n.cluster)),
            ("TAG", &escape(tag)),
            ("MANAGE", &manage_html),
        ],
    );
    let mut text = format!("Sent by Clouduler on {} - {}", n.cluster, tag);
    if let Some(u) = manage {
        let _ = write!(text, "\nManage alert destinations: {u}");
    }
    (html, text)
}

fn alert_rows(m: &AlertMessage) -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = Vec::new();
    v.push(("Node", m.node.clone().unwrap_or_default()));
    v.push(("Stack", m.stack.clone().unwrap_or_default()));
    v.push(("Instance", m.instance.clone().unwrap_or_default()));
    let shown = [&m.node, &m.stack, &m.instance]
        .iter()
        .any(|o| o.as_deref() == Some(m.subject.as_str()));
    if !shown {
        v.push(("Subject", m.subject.clone()));
    }
    v.push(("Event", m.event.clone()));
    if m.phase == Phase::Resolved {
        v.push(("Opened", human_time(m.opened_at)));
        v.push(("Resolved", human_time(m.at)));
        v.push(("Was open for", open_for(m).unwrap_or_default()));
    } else if m.phase == Phase::Event {
        v.push(("When", human_time(m.at)));
    } else {
        v.push(("Since", human_time(m.opened_at)));
    }
    if m.count > 1 {
        v.push(("Occurrences", m.count.to_string()));
    }
    v
}

fn render_alert(n: &Notification, m: &AlertMessage) -> Rendered {
    let (word, bg, fg) = pill(m);
    let extra = match m.phase {
        Phase::Escalated { previous } => render_tpl(
            "pill_escalated.html",
            &[("PREVIOUS", severity_word(previous))],
        ),
        _ => String::new(),
    };
    let pairs = alert_rows(m);
    let excerpt = if m.excerpt.is_empty() {
        String::new()
    } else {
        let lines = m
            .excerpt
            .iter()
            .map(|l| escape(&truncate(l, 1000)))
            .collect::<Vec<_>>()
            .join(&tpl("linebreak.html"));
        render_tpl("excerpt.html", &[("LINES", &lines)])
    };
    let links: Vec<(String, String)> = m
        .links
        .iter()
        .map(|l| (l.label.clone(), l.url.clone()))
        .collect();
    let (footer_html, footer_text) = footer(n, &m.event);
    let title = title_with_phase(m);
    let body = render_tpl(
        "alert.html",
        &[
            ("PILL_BG", bg),
            ("PILL_FG", fg),
            ("PILL", word),
            ("PILL_EXTRA", &extra),
            ("TITLE", &escape(&m.title)),
            (
                "DETAIL",
                &escape(&m.detail).replace('\n', &tpl("linebreak.html")),
            ),
            ("ROWS", &rows(&pairs)),
            ("EXCERPT", &excerpt),
            ("BUTTONS", &buttons(&links)),
        ],
    );
    let html = shell(n, &title, &m.detail, &body, &footer_html);

    let mut text = format!("[{}] {}\n", word.to_uppercase(), title);
    if !m.detail.is_empty() {
        let _ = write!(text, "\n{}\n", m.detail);
    }
    text.push('\n');
    for (k, v) in pairs.iter().filter(|(_, v)| !v.is_empty()) {
        let _ = writeln!(text, "{k}: {v}");
    }
    if !m.excerpt.is_empty() {
        text.push_str("\nLog:\n");
        for l in &m.excerpt {
            let _ = writeln!(text, "  {}", truncate(l, 1000));
        }
    }
    let mut seen = false;
    for l in m.links.iter().filter(|l| is_http(&l.url)) {
        if !seen {
            text.push('\n');
            seen = true;
        }
        let _ = writeln!(text, "{}: {}", l.label, l.url);
    }
    let _ = write!(text, "\n-- \n{footer_text}\n");
    Rendered {
        subject: subject(n),
        text,
        html,
    }
}

fn render_digest(n: &Notification, alerts: &[AlertMessage]) -> Rendered {
    let count = alerts.len();
    let title = format!(
        "{count} event{} on {}",
        if count == 1 { "" } else { "s" },
        n.cluster
    );
    let intro = "Things that happened but did not need you right away.";
    let mut list = String::new();
    let mut text = format!("Daily digest: {title}\n{intro}\n\n");
    for a in alerts {
        let subj = if a.subject.is_empty() || a.title.contains(&a.subject) {
            String::new()
        } else {
            a.subject.clone()
        };
        let sub_html = if subj.is_empty() {
            String::new()
        } else {
            render_tpl("digest_subject.html", &[("SUBJECT", &escape(&subj))])
        };
        list.push_str(&render_tpl(
            "digest_row.html",
            &[
                ("TIME", &clock(a.at)),
                ("TITLE", &escape(&a.title)),
                ("SUBJECT", &sub_html),
            ],
        ));
        let _ = write!(text, "{}  {}", clock(a.at), a.title);
        if !subj.is_empty() {
            let _ = write!(text, " ({subj})");
        }
        if a.count > 1 {
            let _ = write!(text, " x{}", a.count);
        }
        text.push('\n');
    }
    let open = n
        .ui_url
        .as_deref()
        .filter(|u| is_http(u))
        .map(|u| vec![("Open alerts".to_string(), format!("{u}/alerts"))])
        .unwrap_or_default();
    if let Some((_, u)) = open.first() {
        let _ = write!(text, "\nOpen alerts: {u}\n");
    }
    let (footer_html, footer_text) = footer(n, "digest");
    let body = render_tpl(
        "digest.html",
        &[
            ("TITLE", &escape(&title)),
            ("INTRO", intro),
            ("ROWS", &list),
            ("BUTTONS", &buttons(&open)),
        ],
    );
    let _ = write!(text, "\n-- \n{footer_text}\n");
    Rendered {
        subject: subject(n),
        text,
        html: shell(
            n,
            &format!("Daily digest: {title}"),
            intro,
            &body,
            &footer_html,
        ),
    }
}

fn render_test(n: &Notification) -> Rendered {
    let detail = format!(
        "This is a test from Clouduler. Alerts for cluster {} will arrive in this inbox.",
        n.cluster
    );
    let open = n
        .ui_url
        .as_deref()
        .filter(|u| is_http(u))
        .map(|u| vec![("Open in Clouduler".to_string(), format!("{u}/alerts"))])
        .unwrap_or_default();
    let (footer_html, footer_text) = footer(n, "test");
    let body = render_tpl(
        "alert.html",
        &[
            ("PILL_BG", "#d5efe6"),
            ("PILL_FG", "#00695a"),
            ("PILL", "Test"),
            ("PILL_EXTRA", ""),
            ("TITLE", "Clouduler test message"),
            ("DETAIL", &escape(&detail)),
            ("ROWS", ""),
            ("EXCERPT", ""),
            ("BUTTONS", &buttons(&open)),
        ],
    );
    let mut text = format!("[TEST] Clouduler test message\n\n{detail}\n");
    if let Some((_, u)) = open.first() {
        let _ = write!(text, "\nOpen in Clouduler: {u}\n");
    }
    let _ = write!(text, "\n-- \n{footer_text}\n");
    Rendered {
        subject: subject(n),
        text,
        html: shell(n, "Clouduler test message", &detail, &body, &footer_html),
    }
}

fn shell(n: &Notification, title: &str, preheader: &str, body: &str, footer: &str) -> String {
    render_tpl(
        "shell.html",
        &[
            ("TITLE_TEXT", &escape(title)),
            ("PREHEADER", &escape(&truncate(&one_line(preheader), 140))),
            ("LOGO", &logo(n.ui_url.as_deref())),
            ("CLUSTER", &escape(&n.cluster)),
            ("BODY", body),
            ("FOOTER", footer),
        ],
    )
}

pub fn render(n: &Notification) -> Rendered {
    match &n.body {
        Body::Alert(m) => render_alert(n, m),
        Body::Digest { alerts } => render_digest(n, alerts),
        Body::Test => render_test(n),
    }
}

fn permanent(message: String) -> DeliveryError {
    DeliveryError {
        message,
        retryable: false,
        retry_after: None,
    }
}

fn mailbox(raw: &str, what: &str) -> Result<Mailbox, DeliveryError> {
    raw.trim().parse::<Mailbox>().map_err(|_| {
        permanent(format!(
            "{what} is not a valid email address: {}",
            truncate(raw, 80)
        ))
    })
}

/// The MIME message. Pure, so tests can inspect the headers.
pub fn build(
    smtp: &SmtpSettings,
    to: &[String],
    n: &Notification,
) -> Result<Message, DeliveryError> {
    let r = render(n);
    let mut b = Message::builder()
        .from(mailbox(&smtp.from, "the sender")?)
        .subject(r.subject);
    if to.is_empty() {
        return Err(permanent("the email destination has no recipients".into()));
    }
    for t in to {
        b = b.to(mailbox(t, "a recipient")?);
    }
    b.multipart(MultiPart::alternative_plain_html(r.text, r.html))
        .map_err(|e| permanent(format!("could not build the message: {e}")))
}

fn transport(
    smtp: &SmtpSettings,
    password: Option<&str>,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, DeliveryError> {
    let host = smtp.host.trim();
    let builder = match smtp.security {
        SmtpSecurity::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        SmtpSecurity::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
            host,
        )),
    }
    .map_err(|e| permanent(format!("cannot use SMTP server {host}: {e}")))?;
    let mut builder = builder.port(smtp.port).timeout(Some(TIMEOUT));
    if let Some(user) = smtp.username.as_deref().filter(|u| !u.is_empty()) {
        builder = builder.credentials(Credentials::new(
            user.to_string(),
            password.unwrap_or_default().to_string(),
        ));
    }
    Ok(builder.build())
}

pub async fn send(
    smtp: Option<&SmtpSettings>,
    password: Option<&str>,
    to: &[String],
    n: &Notification,
) -> Result<(), DeliveryError> {
    let smtp = smtp.ok_or_else(|| permanent("no email server is configured".into()))?;
    let message = build(smtp, to, n)?;
    let mailer = transport(smtp, password)?;
    let sent = tokio::time::timeout(TIMEOUT * 2, mailer.send(message))
        .await
        .map_err(|_| DeliveryError {
            message: "the mail server did not answer in time".into(),
            retryable: true,
            retry_after: None,
        })?;
    sent.map(|_| ()).map_err(|e| {
        let permanent_failure = e.is_permanent();
        DeliveryError {
            message: if permanent_failure {
                format!("the mail server refused the message: {e}")
            } else {
                format!("could not send the email: {e}")
            },
            retryable: !permanent_failure,
            retry_after: None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deliver::testutil::*;
    use crate::message::Link;

    fn smtp() -> SmtpSettings {
        SmtpSettings {
            host: "mail.test".into(),
            port: 587,
            security: SmtpSecurity::Starttls,
            username: Some("u".into()),
            from: "Clouduler <alerts@cloud.test>".into(),
        }
    }

    #[test]
    fn subjects() {
        let s = |b| subject(&notification(b));
        assert_eq!(
            s(Body::Alert(alert(Severity::Critical, Phase::Opened))),
            "[piosny] CRITICAL: media-radarr-1 keeps crashing"
        );
        assert_eq!(
            s(Body::Alert(alert(Severity::Critical, Phase::Resolved))),
            "[piosny] Resolved: media-radarr-1 keeps crashing"
        );
        assert_eq!(
            s(Body::Alert(alert(
                Severity::Critical,
                Phase::Escalated {
                    previous: Severity::Warning
                }
            ))),
            "[piosny] Escalated to CRITICAL: media-radarr-1 keeps crashing"
        );
        assert_eq!(
            s(Body::Digest {
                alerts: vec![alert(Severity::Info, Phase::Event); 4]
            }),
            "[piosny] Daily digest: 4 events"
        );
        assert_eq!(s(Body::Test), "[piosny] Test message from Clouduler");
    }

    #[test]
    fn subject_cannot_carry_a_header_injection() {
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.title = "x\r\nBcc: evil@test".into();
        let s = subject(&notification(Body::Alert(a)));
        assert!(!s.contains('\n') && !s.contains('\r'));
    }

    #[test]
    fn html_is_escaped_and_complete() {
        let mut a = alert(Severity::Critical, Phase::Opened);
        a.title = "<script>alert(1)</script>".into();
        a.detail = "a & b {{BODY}}".into();
        a.excerpt = vec!["<b>bold</b> \"quoted\"".into()];
        let r = render(&notification(Body::Alert(a)));
        assert!(!r.html.contains("<script>"));
        assert!(r.html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(r.html.contains("a &amp; b {{BODY}}"));
        assert!(r
            .html
            .contains("&lt;b&gt;bold&lt;/b&gt; &quot;quoted&quot;"));
        assert!(!r.html.contains("{{PILL"), "unfilled placeholder");
        for key in [
            "{{TITLE}}",
            "{{ROWS}}",
            "{{BUTTONS}}",
            "{{LOGO}}",
            "{{FOOTER}}",
            "{{BODY}}\n",
        ] {
            assert!(!r.html.contains(key), "{key} left in the output");
        }
        assert!(r.html.contains("Critical"));
        assert!(r.html.contains("https://cloud.test/icon-192.png"));
        assert!(r.html.contains("Open in Clouduler"));
        assert!(r.html.contains("href=\"https://cloud.test/stacks/media\""));
        assert!(r.html.contains("#fffdf8"), "theme colours");
    }

    #[test]
    fn plain_text_has_the_facts() {
        let r = render(&notification(Body::Alert(alert(
            Severity::Critical,
            Phase::Opened,
        ))));
        assert!(r
            .text
            .starts_with("[CRITICAL] media-radarr-1 keeps crashing\n"));
        assert!(r.text.contains("Node: pisoa"));
        assert!(r.text.contains("Stack: media"));
        assert!(r.text.contains("Occurrences: 3"));
        assert!(r
            .text
            .contains("Open in Clouduler: https://cloud.test/stacks/media"));
        assert!(r.text.contains("  panic: out of memory"));
        assert!(!r.text.contains('<'));
    }

    #[test]
    fn resolved_shows_duration() {
        let r = render(&notification(Body::Alert(alert(
            Severity::Warning,
            Phase::Resolved,
        ))));
        assert!(r.text.contains("Was open for: 10m"));
        assert!(r.html.contains("Resolved"));
        assert!(r.html.contains("#00695a"));
    }

    #[test]
    fn no_absolute_links_means_no_button() {
        let mut a = alert(Severity::Warning, Phase::Opened);
        a.links = vec![Link {
            label: "x".into(),
            url: "/stacks/media".into(),
        }];
        let mut n = notification(Body::Alert(a));
        n.ui_url = None;
        let r = render(&n);
        assert!(!r.html.contains("Open in Clouduler"));
        assert!(!r.html.contains("/stacks/media"));
        assert!(!r.html.contains("icon-192.png"));
    }

    #[test]
    fn digest_lists_everything() {
        let alerts: Vec<_> = (0..3)
            .map(|i| {
                let mut a = alert(Severity::Info, Phase::Event);
                a.title = format!("Deployed s{i}");
                a
            })
            .collect();
        let r = render(&notification(Body::Digest { alerts }));
        assert!(r.text.contains("Deployed s0") && r.text.contains("Deployed s2"));
        assert_eq!(r.html.matches("Deployed s").count(), 3);
        assert!(r.html.contains("3 events on piosny"));
        assert!(r.html.contains("https://cloud.test/alerts"));
    }

    #[test]
    fn test_message_renders() {
        let r = render(&notification(Body::Test));
        assert!(r.html.contains("Clouduler test message"));
        assert!(r.text.contains("[TEST]"));
    }

    #[test]
    fn builds_multipart_with_headers() {
        let m = build(
            &smtp(),
            &["ops@cloud.test".into(), "Alexis <a@cloud.test>".into()],
            &notification(Body::Alert(alert(Severity::Critical, Phase::Opened))),
        )
        .unwrap();
        let raw = String::from_utf8(m.formatted()).unwrap();
        assert!(raw.contains("From: Clouduler <alerts@cloud.test>"));
        assert!(raw.contains("ops@cloud.test"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("text/plain"));
        assert!(raw.contains("text/html"));
        assert!(raw.contains("Subject: [piosny] CRITICAL: media-radarr-1 keeps crashing"));
    }

    #[test]
    fn rejects_bad_addresses_without_retrying() {
        let n = notification(Body::Test);
        let e = build(&smtp(), &["nope".into()], &n).unwrap_err();
        assert!(!e.retryable);
        assert!(e.message.contains("recipient"));
        let mut s = smtp();
        s.from = "???".into();
        assert!(!build(&s, &["a@b.test".into()], &n).unwrap_err().retryable);
        assert!(build(&smtp(), &[], &n).is_err());
    }

    #[tokio::test]
    async fn missing_smtp_is_a_clear_error() {
        let e = send(None, None, &["a@b.test".into()], &notification(Body::Test))
            .await
            .unwrap_err();
        assert!(!e.retryable);
        assert!(e.message.contains("no email server"));
    }

    #[test]
    fn fill_does_not_rescan_values() {
        assert_eq!(
            fill("a{{X}}b{{Y}}", &[("X", "{{Y}}"), ("Y", "y")]),
            "a{{Y}}by"
        );
        assert_eq!(fill("a{{open", &[]), "a{{open");
    }
}

#[cfg(test)]
mod template_tests {
    use super::*;

    #[test]
    fn every_template_is_embedded_and_non_empty() {
        for name in [
            "shell.html",
            "alert.html",
            "row.html",
            "excerpt.html",
            "button.html",
            "button_secondary.html",
            "digest.html",
            "digest_row.html",
            "digest_subject.html",
            "footer.html",
            "footer_manage.html",
            "logo_img.html",
            "logo_fallback.html",
            "pill_escalated.html",
            "linebreak.html",
        ] {
            assert!(!tpl(name).is_empty(), "{name}");
        }
    }
}
