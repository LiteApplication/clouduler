//! Which destinations an alert goes to, read from the settings of the moment.

use cl_alert::{AlertSettings, Destination, Route, Severity};
use cl_raft::alerts::{AlertDelivery, AlertRecord};

use super::sev;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Targets {
    /// Sent the moment the alert fires.
    pub immediate: Vec<String>,
    /// Included in the daily digest (info alerts only).
    pub digest: Vec<String>,
}

/// A muted event is neither recorded nor sent.
pub fn muted(settings: &AlertSettings, event: &str) -> bool {
    matches!(
        settings.events.get(event).and_then(|o| o.route.as_ref()),
        Some(Route::Muted)
    )
}

fn passes_filters(d: &Destination, category: &str, stack: Option<&str>) -> bool {
    (d.categories.is_empty() || d.categories.iter().any(|c| c == category))
        && (d.stacks.is_empty() || stack.is_none_or(|s| d.stacks.iter().any(|x| x == s)))
}

pub fn targets(
    settings: &AlertSettings,
    event: &str,
    category: &str,
    severity: Severity,
    stack: Option<&str>,
) -> Targets {
    let mut out = Targets::default();
    if !settings.enabled {
        return out;
    }
    match settings.events.get(event).and_then(|o| o.route.as_ref()) {
        Some(Route::Muted) | Some(Route::InApp) => return out,
        Some(Route::Destinations(names)) => {
            out.immediate = settings
                .destinations
                .iter()
                .filter(|d| d.enabled && names.contains(&d.name))
                .map(|d| d.name.clone())
                .collect();
            return out;
        }
        None => {}
    }
    for d in settings.destinations.iter().filter(|d| d.enabled) {
        if !passes_filters(d, category, stack) {
            continue;
        }
        if d.severities.contains(&severity) {
            out.immediate.push(d.name.clone());
        } else if severity == Severity::Info && d.digest {
            out.digest.push(d.name.clone());
        }
    }
    out
}

pub fn targets_for_record(settings: &AlertSettings, r: &AlertRecord) -> Targets {
    targets(
        settings,
        &r.event,
        &r.category,
        sev(r.severity),
        r.stack.as_deref(),
    )
}

/// Destinations that received the opening message of `r`, still configured
/// and enabled: they get the resolved message too.
pub fn resolved_targets(settings: &AlertSettings, deliveries: &[AlertDelivery]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in deliveries {
        if d.ok
            && matches!(d.phase.as_str(), "opened" | "escalated" | "event")
            && settings
                .destinations
                .iter()
                .any(|x| x.enabled && x.name == d.destination)
            && !out.contains(&d.destination)
        {
            out.push(d.destination.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_alert::{DestinationKind, EventOverride};
    use std::collections::BTreeSet;

    fn dest(name: &str, severities: &[Severity]) -> Destination {
        Destination {
            name: name.into(),
            enabled: true,
            kind: DestinationKind::Webhook,
            severities: severities.iter().copied().collect::<BTreeSet<_>>(),
            digest: false,
            categories: vec![],
            stacks: vec![],
        }
    }

    #[test]
    fn severity_category_and_stack_filters_apply() {
        let mut s = AlertSettings::default();
        let mut a = dest("a", &[Severity::Critical]);
        a.categories = vec!["workloads".into()];
        a.stacks = vec!["web".into()];
        s.destinations = vec![a, dest("b", &[Severity::Critical, Severity::Warning])];
        let t = targets(
            &s,
            "instance.unhealthy",
            "workloads",
            Severity::Critical,
            Some("web"),
        );
        assert_eq!(t.immediate, vec!["a", "b"]);
        let t = targets(
            &s,
            "instance.unhealthy",
            "workloads",
            Severity::Critical,
            Some("db"),
        );
        assert_eq!(t.immediate, vec!["b"]);
        let t = targets(&s, "node.disk_low", "nodes", Severity::Critical, None);
        assert_eq!(t.immediate, vec!["b"]);
        let t = targets(&s, "node.disk_low", "nodes", Severity::Warning, None);
        assert_eq!(t.immediate, vec!["b"]);
    }

    #[test]
    fn info_goes_to_digest_destinations_only() {
        let mut s = AlertSettings::default();
        let mut d = dest("d", &[Severity::Critical]);
        d.digest = true;
        s.destinations = vec![d, dest("e", &[Severity::Critical])];
        let t = targets(&s, "cluster.node_joined", "cluster", Severity::Info, None);
        assert!(t.immediate.is_empty());
        assert_eq!(t.digest, vec!["d"]);
    }

    #[test]
    fn overrides_replace_default_routing() {
        let mut s = AlertSettings {
            destinations: vec![dest("a", &[Severity::Critical]), dest("b", &[])],
            ..Default::default()
        };
        s.events.insert(
            "instance.exited".into(),
            EventOverride {
                severity: None,
                route: Some(Route::Destinations(vec!["b".into()])),
            },
        );
        let t = targets(&s, "instance.exited", "workloads", Severity::Warning, None);
        assert_eq!(t.immediate, vec!["b"]);
        s.events.get_mut("instance.exited").unwrap().route = Some(Route::InApp);
        assert_eq!(
            targets(&s, "instance.exited", "workloads", Severity::Critical, None),
            Targets::default()
        );
        s.events.get_mut("instance.exited").unwrap().route = Some(Route::Muted);
        assert!(muted(&s, "instance.exited"));
        s.enabled = false;
        s.events.clear();
        assert_eq!(
            targets(&s, "x", "workloads", Severity::Critical, None),
            Targets::default()
        );
    }
}
