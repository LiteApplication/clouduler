//! Alert inbox, record and destination health handlers.

use super::*;

impl ClusterState {
    /// Keep at most `max` alert records: resolved and one-shot first, oldest first, open ones last.
    pub(super) fn trim_alerts(&mut self, max: usize) {
        while self.alerts.len() > max {
            let victim = self
                .alerts
                .values()
                .min_by(|a, b| {
                    (a.state.is_open(), a.last_at, &a.id).cmp(&(
                        b.state.is_open(),
                        b.last_at,
                        &b.id,
                    ))
                })
                .map(|a| a.id.clone());
            match victim {
                Some(id) => self.alerts.remove(&id),
                None => break,
            };
        }
    }

    pub(super) fn apply_alert(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::PushAlertSignal(signal) => {
                self.alert_inbox.insert(signal.id.clone(), signal.clone());
                while self.alert_inbox.len() > MAX_ALERT_INBOX {
                    let oldest = self
                        .alert_inbox
                        .iter()
                        .min_by(|a, b| (a.1.at, a.0).cmp(&(b.1.at, b.0)))
                        .map(|(id, _)| id.clone());
                    match oldest {
                        Some(id) => self.alert_inbox.remove(&id),
                        None => break,
                    };
                }
                Ok(())
            }

            Request::TakeAlertSignals { ids } => {
                for id in ids {
                    self.alert_inbox.remove(id);
                }
                Ok(())
            }

            Request::PutAlert(record) => {
                let mut record = record.clone();
                let excess = record
                    .deliveries
                    .len()
                    .saturating_sub(MAX_RECORD_DELIVERIES);
                record.deliveries.drain(..excess);
                self.alerts.insert(record.id.clone(), record);
                self.trim_alerts(MAX_ALERT_RECORDS);
                Ok(())
            }

            Request::RemoveAlert { id } => {
                self.alerts.remove(id);
                Ok(())
            }

            Request::PruneAlerts { before, max } => {
                self.alerts
                    .retain(|_, a| a.state.is_open() || a.last_at >= *before);
                self.trim_alerts(*max);
                Ok(())
            }

            Request::SetDestinationHealth { name, health } => {
                self.alert_destinations.insert(name.clone(), health.clone());
                Ok(())
            }

            Request::RemoveDestinationHealth { name } => {
                self.alert_destinations.remove(name);
                Ok(())
            }

            _ => unreachable!("apply_alert was handed a request routed elsewhere"),
        }
    }
}
