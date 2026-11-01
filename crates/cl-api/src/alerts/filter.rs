//! Per-event mutes and severity overrides: the settings' per-event table and
//! the stacks' own `x-clouduler.alerts`.

use cl_alert::catalog;
use cl_alert::AlertSettings;
use cl_config::{AlertSeverity, StackAlerts, StackFile};
use cl_raft::{ClusterState, InstanceId, VolumeId};

use super::sev_back;

fn stack_block(state: &ClusterState, stack: &str, service: Option<&str>) -> Option<StackAlerts> {
    let spec = &state.stacks.get(stack)?.spec;
    let file = StackFile::parse(spec).ok()?;
    Some(match service {
        Some(s) if file.services.contains_key(s) => file.effective_alerts(s),
        _ => StackAlerts::merged(
            file.x_clouduler.as_ref().and_then(|x| x.alerts.as_ref()),
            None,
            "",
        ),
    })
}

/// Stacks (with the service, when it is one instance) an alert is about.
fn scopes(
    state: &ClusterState,
    event: &str,
    stack: Option<&str>,
    instance: Option<&str>,
    subject: &str,
) -> Vec<(String, Option<String>)> {
    if let Some(stack) = stack {
        let service = instance
            .and_then(|i| i.parse::<InstanceId>().ok())
            .map(|i| i.service);
        return vec![(stack.to_string(), service)];
    }
    if event.starts_with("volume.") {
        if let Ok(id) = subject.parse::<VolumeId>() {
            let mut out: Vec<(String, Option<String>)> = Vec::new();
            for i in cl_raft::instances_using_volume(state, &id) {
                let scope = (i.stack, Some(i.service));
                if !out.contains(&scope) {
                    out.push(scope);
                }
            }
            return out;
        }
    }
    Vec::new()
}

/// The severity an alert is raised at, or `None` when it is muted.
/// `default` is the severity the producer computed. Precedence: the stack or
/// service block, then the settings' per-event override, then `default`.
pub fn effective_severity(
    state: &ClusterState,
    settings: &AlertSettings,
    event: &str,
    default: AlertSeverity,
    stack: Option<&str>,
    instance: Option<&str>,
    subject: &str,
) -> Option<AlertSeverity> {
    if super::route::muted(settings, event) {
        return None;
    }
    let mut severity = default;
    if let Some(o) = settings.events.get(event).and_then(|o| o.severity) {
        severity = sev_back(o);
    }
    if catalog::find(event).is_some_and(|e| e.stack_scoped) {
        let blocks: Vec<StackAlerts> = scopes(state, event, stack, instance, subject)
            .into_iter()
            .filter_map(|(s, svc)| stack_block(state, &s, svc.as_deref()))
            .collect();
        if !blocks.is_empty() && blocks.iter().all(|b| b.mutes(event)) {
            return None;
        }
        if blocks.len() == 1 {
            if let Some(s) = blocks[0].severity_for(event) {
                severity = s;
            }
        }
    }
    Some(severity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_alert::{EventOverride, Severity};
    use cl_raft::Request;

    fn state_with_stack(spec: &str) -> ClusterState {
        let mut st = ClusterState::default();
        assert!(st
            .apply(&Request::DeployStack {
                name: "web".into(),
                spec: spec.into(),
                at: 0
            })
            .is_ok());
        st
    }

    const SPEC: &str = r#"
services:
  app:
    image: nginx
  db:
    image: postgres
    x-clouduler:
      alerts: false
x-clouduler:
  alerts:
    mute: ["update.*"]
    severity:
      instance.exited: info
"#;

    #[test]
    fn stack_and_service_blocks_mute_and_override() {
        let st = state_with_stack(SPEC);
        let s = AlertSettings::default();
        let w = AlertSeverity::Warning;
        let eff = |ev: &str, inst: &str| {
            effective_severity(&st, &s, ev, w, Some("web"), Some(inst), inst)
        };
        assert_eq!(
            eff("instance.exited", "web.app.0"),
            Some(AlertSeverity::Info)
        );
        assert_eq!(eff("update.failed", "web.app.0"), None);
        assert_eq!(eff("instance.unhealthy", "web.db.0"), None);
        assert_eq!(eff("instance.unhealthy", "web.app.0"), Some(w));
    }

    #[test]
    fn settings_override_and_mute() {
        let st = ClusterState::default();
        let mut s = AlertSettings::default();
        s.events.insert(
            "node.disk_low".into(),
            EventOverride {
                severity: Some(Severity::Critical),
                route: None,
            },
        );
        let got = effective_severity(
            &st,
            &s,
            "node.disk_low",
            AlertSeverity::Warning,
            None,
            None,
            "n1",
        );
        assert_eq!(got, Some(AlertSeverity::Critical));
        s.events.get_mut("node.disk_low").unwrap().route = Some(cl_alert::Route::Muted);
        assert_eq!(
            effective_severity(
                &st,
                &s,
                "node.disk_low",
                AlertSeverity::Warning,
                None,
                None,
                "n1"
            ),
            None
        );
    }
}
