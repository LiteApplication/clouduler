//! Auto-rollback for `PUT /api/config`.
//!
//! Applying a new config arms a timer, but only when the edit touches a field
//! that could cut off the connection the edit arrived over (see
//! `connection_sensitive_change`). Unless `confirm` arrives before the timer
//! fires, the previous config is restored. This is the part of a config edit
//! that must never depend on a live clock: it is the safety net for when the
//! edit breaks the very network the ui is talking over, so it has to be
//! provable without spinning up a real timer in a test.

use std::time::Duration;

use cl_config::ClusterConfig;

/// Default grace period before an unconfirmed edit rolls back.
pub const DEFAULT_ROLLBACK_AFTER: Duration = Duration::from_secs(60);

/// Whether two configs differ in a field that can cut off the connection an
/// operator is editing over: which networks and node addresses exist, where
/// the web ui and reverse proxy listen, and the Cloudflare records that route
/// to them from outside. Everything else (cluster name, drives, failover
/// timers, secrets, auth, self-update) takes effect immediately, unwatched:
/// getting those wrong is a bad config, not a lost connection.
pub fn connection_sensitive_change(previous: &ClusterConfig, next: &ClusterConfig) -> bool {
    previous.networks != next.networks
        || previous.nodes != next.nodes
        || previous.web != next.web
        || previous.proxy != next.proxy
        || previous.cloudflare != next.cloudflare
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingStatus {
    /// A config is applied and waiting for confirmation.
    Armed,
    /// The ui confirmed it; nothing pending anymore.
    Confirmed,
    /// The deadline passed with no confirmation, previous config restored.
    RolledBack,
    /// An operator rolled back manually before the deadline.
    RolledBackManually,
}

/// One armed edit: the config to fall back to, and when to do it.
pub struct PendingEdit {
    previous: ClusterConfig,
    armed_at: Duration,
    deadline: Duration,
    status: PendingStatus,
}

impl PendingEdit {
    /// Arm a new edit. `now` and `after` are both durations since some fixed
    /// epoch the caller chooses; tests use a fake counter, the server uses
    /// `Instant::now()` translated to a duration since process start.
    pub fn arm(previous: ClusterConfig, now: Duration, after: Duration) -> Self {
        PendingEdit {
            previous,
            armed_at: now,
            deadline: now + after,
            status: PendingStatus::Armed,
        }
    }

    pub fn status(&self) -> PendingStatus {
        self.status
    }

    pub fn armed_at(&self) -> Duration {
        self.armed_at
    }

    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// Time left before an unconfirmed edit rolls back, zero once the
    /// deadline has passed.
    pub fn remaining(&self, now: Duration) -> Duration {
        self.deadline.saturating_sub(now)
    }

    /// The config to restore. Only meaningful once rolled back, but exposed
    /// unconditionally so the caller can always see what would come back.
    pub fn previous(&self) -> &ClusterConfig {
        &self.previous
    }

    /// Mark the edit confirmed. No-op once the edit already left the armed
    /// state, so a late confirm cannot resurrect a config that already
    /// rolled back.
    pub fn confirm(&mut self) -> bool {
        if self.status == PendingStatus::Armed {
            self.status = PendingStatus::Confirmed;
            true
        } else {
            false
        }
    }

    /// Roll back on operator request, before the deadline.
    pub fn rollback_manually(&mut self) -> bool {
        if self.status == PendingStatus::Armed {
            self.status = PendingStatus::RolledBackManually;
            true
        } else {
            false
        }
    }

    /// Called on the tick that notices the deadline passed. No-op if the
    /// edit was already confirmed or rolled back first.
    pub fn expire(&mut self, now: Duration) -> bool {
        if self.status == PendingStatus::Armed && now >= self.deadline {
            self.status = PendingStatus::RolledBack;
            true
        } else {
            false
        }
    }

    pub fn is_armed(&self) -> bool {
        self.status == PendingStatus::Armed
    }

    pub fn is_settled(&self) -> bool {
        !self.is_armed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config() -> ClusterConfig {
        ClusterConfig {
            cluster: cl_config::ClusterMeta { name: "c".into() },
            networks: BTreeMap::new(),
            drives: BTreeMap::new(),
            nodes: BTreeMap::new(),
            web: cl_config::WebConfig {
                address: "cloud".into(),
                network: "internal".into(),
                port: 8443,
            through_proxy: true,
            backend_port: 8444,
            },
            data_dir: "/var/lib/clouduler".into(),
            failover: Default::default(),
            secrets: Default::default(),
            auth: Default::default(),
            cloudflare: None,
            proxy: Default::default(),
            self_update: None,
            backup: None,
        }
    }

    #[test]
    fn confirming_before_the_deadline_settles_the_edit() {
        let mut edit = PendingEdit::arm(config(), Duration::from_secs(0), Duration::from_secs(60));
        assert!(edit.is_armed());
        assert!(edit.confirm());
        assert_eq!(edit.status(), PendingStatus::Confirmed);
        // A late deadline check must not undo a confirmation.
        assert!(!edit.expire(Duration::from_secs(120)));
        assert_eq!(edit.status(), PendingStatus::Confirmed);
    }

    #[test]
    fn expiring_past_the_deadline_rolls_back() {
        let mut edit = PendingEdit::arm(config(), Duration::from_secs(0), Duration::from_secs(60));
        assert!(!edit.expire(Duration::from_secs(59)));
        assert!(edit.is_armed());
        assert!(edit.expire(Duration::from_secs(60)));
        assert_eq!(edit.status(), PendingStatus::RolledBack);
    }

    #[test]
    fn manual_rollback_preempts_the_timer() {
        let mut edit = PendingEdit::arm(config(), Duration::from_secs(0), Duration::from_secs(60));
        assert!(edit.rollback_manually());
        assert_eq!(edit.status(), PendingStatus::RolledBackManually);
        // The timer firing later must not overwrite a settled state.
        assert!(!edit.expire(Duration::from_secs(60)));
        assert_eq!(edit.status(), PendingStatus::RolledBackManually);
    }

    #[test]
    fn double_confirm_only_the_first_counts() {
        let mut edit = PendingEdit::arm(config(), Duration::from_secs(0), Duration::from_secs(60));
        assert!(edit.confirm());
        assert!(!edit.confirm());
    }

    #[test]
    fn connection_sensitive_change_flags_network_and_web_edits() {
        let mut edited = config();
        edited.web.port = 9443;
        assert!(connection_sensitive_change(&config(), &edited));
    }

    #[test]
    fn connection_sensitive_change_ignores_everything_else() {
        let mut edited = config();
        edited.cluster.name = "renamed".into();
        edited.failover.node_timeout = Duration::from_secs(999);
        edited.auth.session_ttl = Duration::from_secs(1);
        assert!(!connection_sensitive_change(&config(), &edited));
    }

    #[test]
    fn remaining_time_never_goes_negative() {
        let edit = PendingEdit::arm(config(), Duration::from_secs(10), Duration::from_secs(60));
        assert_eq!(edit.remaining(Duration::from_secs(10)), Duration::from_secs(60));
        assert_eq!(edit.remaining(Duration::from_secs(70)), Duration::from_secs(0));
        assert_eq!(edit.remaining(Duration::from_secs(200)), Duration::from_secs(0));
    }
}
