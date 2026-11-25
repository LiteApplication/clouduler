//! Command application: the thin router over the per-domain handlers.

use super::*;

/// Result of applying a command; failures are values, since a rejected command is still a committed entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub version: u64,
    pub error: Option<String>,
}

impl Response {
    fn ok(version: u64) -> Self {
        Self {
            version,
            error: None,
        }
    }

    fn err(version: u64, msg: impl Into<String>) -> Self {
        Self {
            version,
            error: Some(msg.into()),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

impl ClusterState {
    /// Apply one command. Must stay deterministic: no clocks, randomness or IO.
    pub fn apply(&mut self, req: &Request) -> Response {
        // An `Unknown` command must not bump `version` (it drives the SSE stream, lag
        // readings and snapshot ids); it still succeeds so the entry counts as applied.
        if matches!(req, Request::Unknown(_)) {
            let _ = self.dispatch(req);
            return Response::ok(self.version);
        }
        let result = self.dispatch(req);
        match result {
            Ok(()) => {
                self.version += 1;
                Response::ok(self.version)
            }
            // A rejected command leaves state untouched, so the version stays put.
            Err(msg) => Response::err(self.version, msg),
        }
    }

    pub(super) fn dispatch(&mut self, req: &Request) -> Result<(), String> {
        match req {
            Request::NoOp => Ok(()),

            Request::Unknown(_) => {
                tracing::warn!(
                    "received a Request variant this node's binary doesn't recognize (from an upgraded peer) - ignoring until this node is updated"
                );
                Ok(())
            }

            Request::PutClusterConfig { .. } | Request::SetOverlayAllocations { .. } => {
                self.apply_config(req)
            }

            Request::PutConfig { .. }
            | Request::PutConfigCas { .. }
            | Request::RemoveConfig { .. }
            | Request::PutSecret { .. }
            | Request::RemoveSecret { .. }
            | Request::PutWrappedKey { .. } => self.apply_secret(req),

            Request::ClearEmergency
            | Request::SetPrioritized { .. }
            | Request::DeployStack { .. }
            | Request::RemoveStack { .. }
            | Request::SetStackObjects { .. }
            | Request::Assign { .. }
            | Request::Unassign { .. }
            | Request::SetDesiredState { .. }
            | Request::SetImageDigest { .. }
            | Request::ReportContainerStatus { .. }
            | Request::RecordCrashMove { .. } => self.apply_stack(req),

            Request::UpsertNode { .. }
            | Request::SetAvailability { .. }
            | Request::SetReachable { .. }
            | Request::RemoveNode { .. }
            | Request::RetireNodeId { .. } => self.apply_node(req),

            Request::BeginMigration { .. }
            | Request::AdvanceMigration { .. }
            | Request::ClearMigration { .. }
            | Request::SkipMigrationVerify { .. } => self.apply_migration(req),

            Request::UpsertVolume { .. }
            | Request::SetVolumeReplica { .. }
            | Request::TouchVolume { .. }
            | Request::SetVolumeSyncRefusal { .. }
            | Request::SetVolumeEmpty { .. }
            | Request::RemoveVolume { .. }
            | Request::AckVolumeRemoved { .. }
            | Request::FreezeVolume { .. }
            | Request::UnfreezeVolume { .. }
            | Request::Served { .. } => self.apply_volume(req),

            Request::PutBackupDestination { .. }
            | Request::RemoveBackupDestination { .. }
            | Request::PutBackupTarget { .. }
            | Request::RemoveBackupTarget { .. }
            | Request::SetBackupTargetLastRun { .. }
            | Request::RecordBackupRun { .. }
            | Request::PutRestoreRequest { .. }
            | Request::SetRestoreStatus { .. } => self.apply_backup(req),

            Request::SetInstanceUpdate { .. }
            | Request::ClearInstanceUpdate { .. }
            | Request::SetImageChecks { .. }
            | Request::SetAutoUpdate { .. }
            | Request::ClearErrors => self.apply_update(req),

            Request::PutSession { .. }
            | Request::RemoveSession { .. }
            | Request::PruneSessions { .. }
            | Request::PutProxySession { .. }
            | Request::RemoveProxySession { .. }
            | Request::PutProxyToken { .. }
            | Request::RedeemProxyToken { .. } => self.apply_session(req),

            Request::PushAlertSignal { .. }
            | Request::TakeAlertSignals { .. }
            | Request::PutAlert { .. }
            | Request::RemoveAlert { .. }
            | Request::PruneAlerts { .. }
            | Request::SetDestinationHealth { .. }
            | Request::RemoveDestinationHealth { .. } => self.apply_alert(req),

            Request::StoreCertificate { .. }
            | Request::RemoveCertificate { .. }
            | Request::SetCertStatus { .. }
            | Request::UpsertDnsRecord { .. }
            | Request::RemoveDnsRecord { .. }
            | Request::SetDnsStatus { .. }
            | Request::ClearReconcileStatus { .. } => self.apply_reconcile(req),

            Request::SetStackTerraform { .. }
            | Request::PutTerraformState { .. }
            | Request::RecordTerraformRun { .. }
            | Request::PutTerraformOutputs { .. }
            | Request::RequestTerraformRun { .. } => self.apply_terraform(req),
        }
    }
}
