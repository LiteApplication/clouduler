//! Sending one [`Notification`] to one destination. One attempt per call:
//! retries, backoff and recording the outcome are the engine's job, guided by
//! [`DeliveryError::retryable`] and [`DeliveryError::retry_after`].

use std::time::Duration;

use crate::message::Notification;
use crate::settings::{Destination, DestinationKind, SmtpSettings};

mod discord;
mod email;
mod matrix;
#[cfg(test)]
mod testutil;
mod util;
mod webhook;

pub use discord::messages as discord_messages;
pub use email::{render as render_email, Rendered};
pub use matrix::{
    login as matrix_login, logout as matrix_logout, message as matrix_message,
    Message as MatrixMessage,
};
pub use webhook::payload as webhook_payload;

/// Every attempt gives up after this long.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Plaintext secrets a destination needs, unsealed by the caller just before
/// the call and dropped right after.
#[derive(Default)]
pub struct DestinationSecrets {
    /// Webhook / discord URL, or the Matrix homeserver URL.
    pub url: Option<String>,
    /// Generic webhook `Authorization` header value, or the Matrix access token.
    pub auth: Option<String>,
    /// SMTP password for an email destination.
    pub smtp_password: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DeliveryError {
    /// Safe to show in the UI: never contains a URL token or password.
    pub message: String,
    /// A later attempt may succeed (timeout, 5xx, 429).
    pub retryable: bool,
    /// The receiver asked to wait this long (Discord's 429 `retry_after`).
    pub retry_after: Option<Duration>,
}

/// Deliver `n` to `dest`. `smtp` is the cluster's SMTP settings, needed only
/// for email destinations.
pub async fn deliver(
    http: &reqwest::Client,
    dest: &Destination,
    secrets: &DestinationSecrets,
    smtp: Option<&SmtpSettings>,
    n: &Notification,
    edit_message_id: Option<&str>,
) -> Result<Option<String>, DeliveryError> {
    let missing_url = || DeliveryError {
        message: "the destination has no URL".into(),
        retryable: false,
        retry_after: None,
    };
    match &dest.kind {
        DestinationKind::Webhook => {
            let url = secrets.url.as_deref().ok_or_else(missing_url)?;
            webhook::send(http, url, secrets.auth.as_deref(), n)
                .await
                .map(|()| None)
        }
        DestinationKind::Discord(opts) => {
            let url = secrets.url.as_deref().ok_or_else(missing_url)?;
            discord::send(http, url, opts, n, edit_message_id).await
        }
        DestinationKind::Matrix(opts) => {
            let url = secrets.url.as_deref().ok_or_else(|| DeliveryError {
                message: "the destination has no homeserver URL".into(),
                retryable: false,
                retry_after: None,
            })?;
            let token = secrets.auth.as_deref().ok_or_else(|| DeliveryError {
                message: "the destination has no access token".into(),
                retryable: false,
                retry_after: None,
            })?;
            matrix::send(http, url, token, opts, n).await.map(|()| None)
        }
        DestinationKind::Email { to } => email::send(smtp, secrets.smtp_password.as_deref(), to, n)
            .await
            .map(|()| None),
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use crate::settings::{Destination, DiscordOptions, MatrixOptions};
    use std::collections::BTreeSet;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Answers every request with `response` and hands back the raw requests.
    async fn serve(
        response: &'static str,
        requests: usize,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook/token-123", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..requests {
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

    fn dest(kind: DestinationKind) -> Destination {
        Destination {
            name: "x".into(),
            enabled: true,
            kind,
            severities: BTreeSet::new(),
            digest: false,
            categories: vec![],
            stacks: vec![],
        }
    }

    #[tokio::test]
    async fn webhook_sends_json_and_headers() {
        let (url, h) = serve("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n", 1).await;
        let secrets = DestinationSecrets {
            url: Some(url),
            auth: Some("Bearer s3cret".into()),
            ..Default::default()
        };
        let n = notification(Body::Alert(alert(Severity::Critical, Phase::Opened)));
        deliver(
            &reqwest::Client::new(),
            &dest(DestinationKind::Webhook),
            &secrets,
            None,
            &n,
            None,
        )
        .await
        .unwrap();
        let req = h.await.unwrap().remove(0);
        let lower = req.to_ascii_lowercase();
        assert!(req.starts_with("POST /hook/token-123 "));
        assert!(lower.contains("authorization: bearer s3cret"));
        assert!(lower.contains("x-clouduler-event: instance.crash_loop"));
        assert!(lower.contains("x-clouduler-severity: critical"));
        assert!(lower.contains("user-agent: clouduler/"));
        assert!(req.contains("\"type\":\"alert\""));
    }

    #[tokio::test]
    async fn discord_retry_after_and_no_url_in_errors() {
        let (url, h) = serve(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: 55\r\n\r\n{\"message\":\"rate limited\",\"retry_after\":1.5,\"global\":0}",
            1,
        )
        .await;
        let secrets = DestinationSecrets {
            url: Some(url),
            ..Default::default()
        };
        let n = notification(Body::Alert(alert(Severity::Critical, Phase::Opened)));
        let e = deliver(
            &reqwest::Client::new(),
            &dest(DestinationKind::Discord(DiscordOptions::default())),
            &secrets,
            None,
            &n,
            None,
        )
        .await
        .unwrap_err();
        assert!(e.retryable);
        assert_eq!(e.retry_after, Some(Duration::from_millis(1500)));
        assert!(!e.message.contains("token-123"));
        let req = h.await.unwrap().remove(0);
        // Link buttons are on by default, so the execute call must ask for them.
        assert!(
            req.starts_with("POST /hook/token-123?wait=true&with_components=true "),
            "{}",
            &req[..60]
        );
    }

    #[tokio::test]
    async fn matrix_puts_an_html_message_with_a_bearer_token() {
        let (url, h) = serve(
            "HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n{\"event_id\":\"$e\"}",
            1,
        )
        .await;
        let base = url.trim_end_matches("/hook/token-123").to_string();
        let secrets = DestinationSecrets {
            url: Some(base),
            auth: Some("syt_s3cret".into()),
            ..Default::default()
        };
        let n = notification(Body::Test);
        deliver(
            &reqwest::Client::new(),
            &dest(DestinationKind::Matrix(MatrixOptions {
                room: "!abc:example.org".into(),
                style: Default::default(),
            })),
            &secrets,
            None,
            &n,
            None,
        )
        .await
        .unwrap();
        let req = h.await.unwrap().remove(0);
        let lower = req.to_ascii_lowercase();
        assert!(
            req.starts_with(
                "PUT /_matrix/client/v3/rooms/%21abc%3Aexample.org/send/m.room.message/clouduler-"
            ),
            "{}",
            &req[..100]
        );
        assert!(lower.contains("authorization: bearer syt_s3cret"));
        assert!(req.contains("\"msgtype\":\"m.text\""));
        assert!(req.contains("org.matrix.custom.html"));
    }

    #[tokio::test]
    async fn matrix_needs_a_token_and_hides_it_in_errors() {
        let (url, _h) = serve("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 44\r\n\r\n{\"errcode\":\"M_UNKNOWN_TOKEN\",\"error\":\"bad\"}  ", 1).await;
        let base = url.trim_end_matches("/hook/token-123").to_string();
        let kind = DestinationKind::Matrix(MatrixOptions {
            room: "!abc:example.org".into(),
            style: Default::default(),
        });
        let e = deliver(
            &reqwest::Client::new(),
            &dest(kind.clone()),
            &DestinationSecrets {
                url: Some(base.clone()),
                ..Default::default()
            },
            None,
            &notification(Body::Test),
            None,
        )
        .await
        .unwrap_err();
        assert!(!e.retryable);
        let e = deliver(
            &reqwest::Client::new(),
            &dest(kind),
            &DestinationSecrets {
                url: Some(base),
                auth: Some("syt_s3cret".into()),
                ..Default::default()
            },
            None,
            &notification(Body::Test),
            None,
        )
        .await
        .unwrap_err();
        assert!(!e.retryable);
        assert!(!e.message.contains("syt_s3cret"));
    }

    #[tokio::test]
    async fn client_errors_are_final_and_server_errors_retry() {
        let (url, _h) = serve("HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n", 1).await;
        let secrets = DestinationSecrets {
            url: Some(url),
            ..Default::default()
        };
        let n = notification(Body::Test);
        let e = deliver(
            &reqwest::Client::new(),
            &dest(DestinationKind::Webhook),
            &secrets,
            None,
            &n,
            None,
        )
        .await
        .unwrap_err();
        assert!(!e.retryable);
        let (url, _h) = serve(
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n",
            1,
        )
        .await;
        let secrets = DestinationSecrets {
            url: Some(url),
            ..Default::default()
        };
        let e = deliver(
            &reqwest::Client::new(),
            &dest(DestinationKind::Webhook),
            &secrets,
            None,
            &n,
            None,
        )
        .await
        .unwrap_err();
        assert!(e.retryable);
    }

    #[tokio::test]
    async fn connection_errors_hide_the_url() {
        let secrets = DestinationSecrets {
            url: Some("http://127.0.0.1:1/hook/token-123".into()),
            ..Default::default()
        };
        let e = deliver(
            &reqwest::Client::new(),
            &dest(DestinationKind::Webhook),
            &secrets,
            None,
            &notification(Body::Test),
            None,
        )
        .await
        .unwrap_err();
        assert!(e.retryable);
        assert!(
            !e.message.contains("token-123") && !e.message.contains("127.0.0.1"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn missing_url_is_final() {
        let e = deliver(
            &reqwest::Client::new(),
            &dest(DestinationKind::Webhook),
            &DestinationSecrets::default(),
            None,
            &notification(Body::Test),
            None,
        )
        .await
        .unwrap_err();
        assert!(!e.retryable);
    }
}
