//! Docker event stream, filtered to clouduler-managed containers. The reconcile
//! loop subscribes to this instead of polling `list_managed` on a timer.

use std::collections::HashMap;

use bollard::query_parameters::EventsOptionsBuilder;
use futures::{Stream, StreamExt};

use crate::client::Docker;
use crate::container::{LABEL_MANAGED, MANAGED_VALUE};
use crate::error::{classify, Result};

/// A container lifecycle event, mapped from docker's raw event stream down to
/// the cases the reconcile loop actually needs to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerEvent {
    Started {
        id: String,
        name: String,
    },
    Stopped {
        id: String,
        name: String,
    },
    Died {
        id: String,
        name: String,
        exit_code: i64,
    },
    Removed {
        id: String,
        name: String,
    },
    HealthStatus {
        id: String,
        name: String,
        status: String,
    },
}

impl Docker {
    /// Subscribe to docker events for clouduler-managed containers. Events that
    /// don't map to a `ContainerEvent` variant (attach, exec, rename, ...) are
    /// silently dropped from the stream.
    pub fn events(&self) -> impl Stream<Item = Result<ContainerEvent>> + '_ {
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        filters.insert(
            "label".to_string(),
            vec![format!("{LABEL_MANAGED}={MANAGED_VALUE}")],
        );
        let options = EventsOptionsBuilder::new().filters(&filters).build();

        self.inner
            .events(Some(options))
            .filter_map(|res| async move {
                match res {
                    Ok(msg) => map_event(msg).map(Ok),
                    Err(e) => Some(Err(classify(e))),
                }
            })
    }
}

fn map_event(msg: bollard::models::EventMessage) -> Option<ContainerEvent> {
    let actor = msg.actor?;
    let action = msg.action?;
    let id = actor.id.unwrap_or_default();
    let name = actor
        .attributes
        .as_ref()
        .and_then(|attrs| attrs.get("name"))
        .cloned()
        .unwrap_or_default();

    match action.as_str() {
        "start" => Some(ContainerEvent::Started { id, name }),
        "stop" => Some(ContainerEvent::Stopped { id, name }),
        "die" => {
            let exit_code = actor
                .attributes
                .as_ref()
                .and_then(|attrs| attrs.get("exitCode"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(-1);
            Some(ContainerEvent::Died {
                id,
                name,
                exit_code,
            })
        }
        "destroy" => Some(ContainerEvent::Removed { id, name }),
        other => other
            .strip_prefix("health_status: ")
            .map(|status| ContainerEvent::HealthStatus {
                id,
                name,
                status: status.to_string(),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::EventActor;

    fn actor_with(id: &str, name: &str, extra: &[(&str, &str)]) -> EventActor {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), name.to_string());
        for (k, v) in extra {
            attrs.insert(k.to_string(), v.to_string());
        }
        EventActor {
            id: Some(id.to_string()),
            attributes: Some(attrs),
        }
    }

    fn msg_with(action: &str, actor: EventActor) -> bollard::models::EventMessage {
        bollard::models::EventMessage {
            action: Some(action.to_string()),
            actor: Some(actor),
            ..Default::default()
        }
    }

    #[test]
    fn maps_start_action() {
        let msg = msg_with("start", actor_with("abc", "web-1", &[]));
        assert_eq!(
            map_event(msg),
            Some(ContainerEvent::Started {
                id: "abc".to_string(),
                name: "web-1".to_string(),
            })
        );
    }

    #[test]
    fn maps_die_action_with_exit_code() {
        let msg = msg_with("die", actor_with("abc", "web-1", &[("exitCode", "137")]));
        assert_eq!(
            map_event(msg),
            Some(ContainerEvent::Died {
                id: "abc".to_string(),
                name: "web-1".to_string(),
                exit_code: 137,
            })
        );
    }

    #[test]
    fn maps_destroy_to_removed() {
        let msg = msg_with("destroy", actor_with("abc", "web-1", &[]));
        assert_eq!(
            map_event(msg),
            Some(ContainerEvent::Removed {
                id: "abc".to_string(),
                name: "web-1".to_string(),
            })
        );
    }

    #[test]
    fn maps_health_status_action() {
        let msg = msg_with("health_status: healthy", actor_with("abc", "web-1", &[]));
        assert_eq!(
            map_event(msg),
            Some(ContainerEvent::HealthStatus {
                id: "abc".to_string(),
                name: "web-1".to_string(),
                status: "healthy".to_string(),
            })
        );
    }

    #[test]
    fn unrecognised_action_is_dropped() {
        let msg = msg_with("rename", actor_with("abc", "web-1", &[]));
        assert_eq!(map_event(msg), None);
    }

    #[test]
    fn event_without_actor_is_dropped() {
        let msg = bollard::models::EventMessage {
            action: Some("start".to_string()),
            actor: None,
            ..Default::default()
        };
        assert_eq!(map_event(msg), None);
    }
}
