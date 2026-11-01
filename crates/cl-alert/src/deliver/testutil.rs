//! Shared fixtures for the renderer tests.

use crate::message::AlertMessage;
pub use crate::message::{Body, Link, Notification, Phase};
pub use crate::settings::Severity;

pub const OPENED_AT: i64 = 1_782_983_520 - 600;
pub const AT: i64 = 1_782_983_520;

pub fn alert(severity: Severity, phase: Phase) -> AlertMessage {
    AlertMessage {
        id: "a1".into(),
        event: "instance.crash_loop".into(),
        category: "workloads".into(),
        severity,
        phase,
        title: "media-radarr-1 keeps crashing".into(),
        detail: "Restarted 3 times in 10 minutes.".into(),
        subject: "media-radarr-1".into(),
        stack: Some("media".into()),
        instance: None,
        node: Some("pisoa".into()),
        opened_at: OPENED_AT,
        at: AT,
        count: 3,
        links: vec![
            Link {
                label: "Open in Clouduler".into(),
                url: "https://cloud.test/stacks/media".into(),
            },
            Link {
                label: "Logs".into(),
                url: "https://cloud.test/stacks/media/logs".into(),
            },
        ],
        excerpt: vec!["panic: out of memory".into()],
    }
}

pub fn notification(body: Body) -> Notification {
    Notification {
        cluster: "piosny".into(),
        ui_url: Some("https://cloud.test".into()),
        body,
    }
}
