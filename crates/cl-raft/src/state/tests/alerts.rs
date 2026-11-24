use super::*;

#[test]
fn alert_inbox_is_bounded_and_drops_the_oldest() {
    let mut state = ClusterState::default();
    for i in 0..(MAX_ALERT_INBOX + 5) {
        let mut sig = sample_signal(&format!("s{i:04}"));
        sig.at = i as i64;
        assert!(state.apply(&Request::PushAlertSignal(sig)).is_ok());
    }
    assert_eq!(state.alert_inbox.len(), MAX_ALERT_INBOX);
    assert!(!state.alert_inbox.contains_key("s0000"));
    assert!(state.alert_inbox.contains_key("s0504"));
    state.apply(&Request::TakeAlertSignals {
        ids: vec!["s0504".into(), "nope".into()],
    });
    assert_eq!(state.alert_inbox.len(), MAX_ALERT_INBOX - 1);
}

#[test]
fn pruning_alerts_keeps_open_ones_and_trims_oldest_resolved_first() {
    let mut state = ClusterState::default();
    state.apply(&Request::PutAlert(sample_alert(
        "open",
        AlertState::Active,
        1,
    )));
    state.apply(&Request::PutAlert(sample_alert(
        "old",
        AlertState::Resolved,
        2,
    )));
    state.apply(&Request::PutAlert(sample_alert(
        "mid",
        AlertState::Event,
        50,
    )));
    state.apply(&Request::PutAlert(sample_alert(
        "new",
        AlertState::Resolved,
        90,
    )));
    state.apply(&Request::PruneAlerts {
        before: 10,
        max: 10,
    });
    assert!(state.alerts.contains_key("open"));
    assert!(!state.alerts.contains_key("old"));
    state.apply(&Request::PruneAlerts { before: 0, max: 2 });
    assert_eq!(
        state.alerts.keys().cloned().collect::<Vec<_>>(),
        vec!["new".to_string(), "open".to_string()]
    );
    state.apply(&Request::RemoveAlert { id: "new".into() });
    assert_eq!(state.alerts.len(), 1);
}

#[test]
fn put_alert_caps_the_delivery_log() {
    let mut state = ClusterState::default();
    let mut rec = sample_alert("a", AlertState::Active, 1);
    for i in 0..30 {
        rec.deliveries.push(crate::alerts::AlertDelivery {
            destination: "d".into(),
            at: i,
            ok: true,
            error: None,
            attempts: 1,
            phase: "opened".into(),
            message_id: None,
        });
    }
    state.apply(&Request::PutAlert(rec));
    let kept = &state.alerts["a"].deliveries;
    assert_eq!(kept.len(), MAX_RECORD_DELIVERIES);
    assert_eq!(kept.last().unwrap().at, 29);
}
