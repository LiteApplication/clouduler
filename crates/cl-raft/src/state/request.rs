use super::*;

/// A command submitted through raft; every mutation of cluster state is one of these.
/// `Deserialize` is hand-written (see its impl) so an unknown variant from a newer node becomes `Unknown`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Request {
    /// Replace the cluster config file on every node. `expect_generation` is the
    /// generation the writer read (0 if none stored); a mismatch is refused rather than overwriting.
    PutClusterConfig {
        yaml: String,
        expect_generation: u64,
    },
    UpsertNode(NodeState),
    SetAvailability {
        node: String,
        availability: Availability,
    },
    /// Submitted by the leader once a node is silent past `failover.node_timeout`, or answers again.
    SetReachable {
        node: String,
        reachable: bool,
    },
    RemoveNode {
        node: String,
    },
    /// Tombstone an id without touching the roster, so `reap_removed_nodes` drops it
    /// from raft membership on its own timer. For an entry that changes identity
    /// but keeps its name (re-provisioned via `init --force`, or a node demoted by
    /// `force_quorum` returning under a new id); `RemoveNode` would delete the live entry.
    RetireNodeId {
        id: uuid::Uuid,
    },
    DeployStack {
        name: String,
        spec: String,
        /// Unix seconds of the deploy, stamped by the caller so `apply` needs no clock.
        #[serde(default)]
        at: i64,
    },
    RemoveStack {
        name: String,
    },
    /// Record which cluster objects a stack owns so removing it cleans them up; sent after they are written.
    SetStackObjects {
        name: String,
        configs: BTreeSet<String>,
        secrets: BTreeSet<String>,
        #[serde(default)]
        used_configs: BTreeSet<String>,
        #[serde(default)]
        used_secrets: BTreeSet<String>,
    },
    /// Place an instance; `drives` lets the state machine refuse a node lacking any.
    Assign {
        instance: InstanceId,
        node: String,
        drives: BTreeSet<String>,
    },
    Unassign {
        instance: InstanceId,
    },
    SetDesiredState {
        instance: InstanceId,
        desired: DesiredState,
    },
    SetImageDigest {
        instance: InstanceId,
        digest: String,
    },
    BeginMigration {
        instance: InstanceId,
        to: String,
        drives: BTreeSet<String>,
    },
    AdvanceMigration {
        instance: InstanceId,
        phase: MigrationPhase,
    },
    /// Forgets a failed move. Refused unless the record is `Failed`.
    ClearMigration {
        instance: InstanceId,
    },
    UpsertVolume {
        id: VolumeId,
        primary: String,
    },
    SetVolumeReplica {
        id: VolumeId,
        node: String,
        generation: u64,
    },
    TouchVolume {
        id: VolumeId,
        primary: String,
    },
    /// Records (or with `None` clears) that the source refused to replicate to avoid wiping a replica.
    SetVolumeSyncRefusal {
        id: VolumeId,
        refusal: Option<SyncRefusal>,
    },
    /// Deletes a volume, files included, on every node implementing its drive. Refused while frozen or mounted.
    RemoveVolume {
        id: VolumeId,
    },
    AckVolumeRemoved {
        id: VolumeId,
        node: String,
    },
    SetVolumeEmpty {
        id: VolumeId,
        empty: bool,
    },
    FreezeVolume {
        id: VolumeId,
        node: String,
        stopped: Vec<InstanceId>,
    },
    /// Resumes replication; the frozen node's copy becomes the new generation.
    UnfreezeVolume {
        id: VolumeId,
    },
    StoreCertificate(CertificateState),
    RemoveCertificate {
        fqdn: String,
    },
    /// A hostname's certificate is being ordered or failed; superseded by `StoreCertificate`.
    SetCertStatus {
        fqdn: String,
        status: ReconcileStatus,
    },
    UpsertDnsRecord(DnsRecordState),
    RemoveDnsRecord {
        key: String,
    },
    /// A hostname's dns record is being reconciled or failed; superseded by `UpsertDnsRecord`.
    SetDnsStatus {
        fqdn: String,
        status: ReconcileStatus,
    },
    /// Drops a hostname's cert/dns reconcile status, which only a successful
    /// store/upsert otherwise supersedes (one for a departed hostname would linger).
    ClearReconcileStatus {
        fqdn: String,
    },
    PutConfig {
        name: String,
        content: String,
        #[serde(default)]
        owner: Option<String>,
    },
    /// `PutConfig` applied only if the current revision is still `expect_revision`
    /// (0 = does not exist yet), so the loser of a read-modify-write races gets an error.
    PutConfigCas {
        name: String,
        content: String,
        #[serde(default)]
        owner: Option<String>,
        expect_revision: u64,
    },
    RemoveConfig {
        name: String,
    },
    /// Sealed by the caller: plaintext never enters the raft log.
    PutSecret {
        name: String,
        sealed: String,
        key_fingerprint: String,
        #[serde(default)]
        owner: Option<String>,
    },
    RemoveSecret {
        name: String,
    },
    /// Written by the node holding the plaintext key once `node` publishes a pubkey
    /// with no (or a stale, after rotation) wrapped copy.
    PutWrappedKey {
        node: String,
        entry: WrappedKeyEntry,
    },
    /// Overlay address book; allocation goes through raft so two nodes never hand out the same address.
    SetOverlayAllocations {
        allocations: String,
    },
    PutBackupDestination(BackupDestination),
    RemoveBackupDestination {
        id: String,
    },
    PutBackupTarget(DriveBackupTarget),
    RemoveBackupTarget {
        id: String,
    },
    /// Submitted by the leader when a target is due, claiming the tick so no other term double-fires.
    SetBackupTargetLastRun {
        id: String,
        at: i64,
    },
    /// Insert or update by id (`Running` -> `Succeeded`/`Failed`).
    RecordBackupRun(BackupRun),
    /// Submitted by an api handler; always inserted `Pending`, since only the claiming node may advance it.
    PutRestoreRequest(RestoreRequest),
    /// Advances a restore's status: a claim (`Pending` -> `Running`) or a terminal
    /// outcome. The claim is an optimistic lock: of two racing claimers only the
    /// first commits (see `backup_restore`).
    SetRestoreStatus {
        id: String,
        status: RestoreStatus,
        /// Unix seconds from the caller (keeps `apply` pure); used as `finished_at` when `status` is terminal.
        at: i64,
    },
    /// Self-reported each reconcile pass: state of every instance assigned to
    /// `node`, wholesale-replacing its prior `container_status` entries so moved or removed instances drop out.
    ReportContainerStatus {
        node: String,
        running: BTreeMap<InstanceId, bool>,
        /// Per instance, the secret revisions its container was created with; optional for older nodes' reports.
        #[serde(default)]
        secret_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
        /// Likewise for read-only configs; optional for older nodes' reports.
        #[serde(default)]
        config_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
    },
    /// Record the step an update or redeploy reached; `at` is caller-supplied unix
    /// seconds. Rejected for an instance with no assignment.
    SetInstanceUpdate {
        instance: InstanceId,
        node: String,
        phase: UpdatePhase,
        #[serde(default)]
        error: Option<String>,
        at: i64,
    },
    ClearInstanceUpdate {
        instance: InstanceId,
    },
    /// Record what `x-clouduler.auto_update` did about `digest`; the same digest keeps
    /// its `detected_at`, a new one starts afresh at `at` (unix seconds).
    SetAutoUpdate {
        instance: InstanceId,
        digest: String,
        outcome: AutoUpdateOutcome,
        #[serde(default)]
        error: Option<String>,
        at: i64,
    },
    /// Replace the registry check results wholesale (undeclared images drop out);
    /// `at` becomes `image_checks_at`. A failed entry keeps the previous digest.
    SetImageChecks {
        checks: BTreeMap<String, ImageCheck>,
        at: i64,
    },
    /// Drops every recorded failure (failed backup runs, restores, updates,
    /// migrations, cert/DNS errors, failed alert deliveries); running, pending and succeeded records stay.
    ClearErrors,
    /// Store a login session under `key` (a hash of the session id); `now` is caller
    /// supplied, and expired sessions are dropped at the same time.
    PutSession {
        key: String,
        session: SessionRecord,
        now: i64,
    },
    RemoveSession {
        key: String,
    },
    PruneSessions {
        now: i64,
    },
    /// Queue a node's signal for the alert engine; the inbox is bounded, oldest dropped first.
    PushAlertSignal(AlertSignal),
    TakeAlertSignals {
        ids: Vec<String>,
    },
    PutAlert(AlertRecord),
    RemoveAlert {
        id: String,
    },
    /// Delete resolved and one-shot records last touched before `before`, then the oldest until at most `max` remain.
    PruneAlerts {
        before: i64,
        max: usize,
    },
    SetDestinationHealth {
        name: String,
        health: DestinationHealth,
    },
    RemoveDestinationHealth {
        name: String,
    },
    /// Store a reverse proxy session under `key` (hash of the cookie); also sweeps expired ones, like `PutSession`.
    PutProxySession {
        key: String,
        session: ProxySessionRecord,
        now: i64,
    },
    RemoveProxySession {
        key: String,
    },
    PutProxyToken {
        key: String,
        token: ProxyTokenRecord,
        now: i64,
    },
    /// Atomically spend the token under `token_key`: it must exist, be unexpired and
    /// issued for `host`; then a session for `host` exists under `session_key`. On
    /// failure nothing changes. Of two racing redeems exactly one succeeds.
    RedeemProxyToken {
        token_key: String,
        host: String,
        session_key: String,
        now: i64,
    },
    /// Remember that `instance` was moved off `from` for crashing; `at` is caller
    /// supplied, and entries older than `CRASH_MOVE_RETENTION_SECS` are dropped.
    RecordCrashMove {
        instance: InstanceId,
        from: String,
        at: u64,
    },
    /// Written on leader election so the log always has a fresh entry to commit.
    NoOp,
    /// Ends emergency mode (`ClusterState::emergency_stacks`). Submitted by
    /// `watch_leadership` once another voter is reachable, not by an operator.
    ClearEmergency,
    /// Puts `stack` ahead of all others (`on`) or back at its own priority (`!on`).
    /// Turning on for an undeployed stack is refused.
    SetPrioritized {
        stack: String,
        on: bool,
    },
    /// Changes who holds a served drive, or a handover. See `served::ServedOp`.
    Served {
        drive: String,
        op: crate::served::ServedOp,
    },
    /// Replaces a stack's OpenTofu modules (service -> source).
    SetStackTerraform {
        stack: String,
        modules: BTreeMap<String, String>,
    },
    /// Sealed by the caller; compare-and-set on `expect_serial`.
    PutTerraformState {
        key: String,
        sealed: String,
        key_fingerprint: String,
        expect_serial: u64,
    },
    /// Starts (Running: the claim) or finishes a run; see `crate::terraform`.
    RecordTerraformRun {
        run: crate::terraform::TerraformRun,
    },
    /// The run's module outputs, written to cluster secrets and configs; see
    /// `ClusterState::apply_terraform_outputs`. `claims` take ownership of unchanged,
    /// already existing objects without rewriting them.
    PutTerraformOutputs {
        key: String,
        run_id: String,
        secrets: Vec<crate::terraform::TerraformSecretOutput>,
        configs: Vec<crate::terraform::TerraformConfigOutput>,
        #[serde(default)]
        claims: Vec<crate::terraform::OutputObject>,
    },
    /// A manual Plan/Apply, picked up by the leader's runner.
    RequestTerraformRun {
        key: String,
        mode: crate::terraform::TerraformMode,
    },
    /// Stand-in for a variant this (older) binary does not recognize, produced only
    /// by `Deserialize` for a genuinely unknown tag. `dispatch` treats it as a
    /// no-op without bumping `version`; the real effect lands once this node is
    /// upgraded and replays a snapshot or the entry. Holds the raw JSON and is
    /// `untagged` so re-serializing reproduces the payload verbatim (an old leader
    /// re-replicating it); the legacy bare string `"Unknown"` round trips too.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// Every variant name except `Unknown`: tells "never-heard-of tag" (falls back to
/// `Unknown`) from "known tag, malformed body" (hard error). Extend with each
/// new variant, alongside `request_mirror` (checked by
/// `every_current_request_variant_round_trips_and_is_not_unknown`).
pub(super) const KNOWN_REQUEST_VARIANTS: &[&str] = &[
    "PutClusterConfig",
    "UpsertNode",
    "SetAvailability",
    "SetReachable",
    "RemoveNode",
    "RetireNodeId",
    "DeployStack",
    "RemoveStack",
    "SetStackObjects",
    "Assign",
    "Unassign",
    "SetDesiredState",
    "SetImageDigest",
    "BeginMigration",
    "AdvanceMigration",
    "ClearMigration",
    "UpsertVolume",
    "SetVolumeReplica",
    "TouchVolume",
    "SetVolumeSyncRefusal",
    "SetVolumeEmpty",
    "RemoveVolume",
    "AckVolumeRemoved",
    "FreezeVolume",
    "UnfreezeVolume",
    "StoreCertificate",
    "RemoveCertificate",
    "SetCertStatus",
    "UpsertDnsRecord",
    "RemoveDnsRecord",
    "SetDnsStatus",
    "ClearReconcileStatus",
    "PutConfig",
    "PutConfigCas",
    "RemoveConfig",
    "PutSecret",
    "RemoveSecret",
    "PutWrappedKey",
    "SetOverlayAllocations",
    "PutBackupDestination",
    "RemoveBackupDestination",
    "PutBackupTarget",
    "RemoveBackupTarget",
    "SetBackupTargetLastRun",
    "RecordBackupRun",
    "PutRestoreRequest",
    "SetRestoreStatus",
    "ReportContainerStatus",
    "SetInstanceUpdate",
    "ClearInstanceUpdate",
    "SetAutoUpdate",
    "SetImageChecks",
    "ClearErrors",
    "PutSession",
    "RemoveSession",
    "PruneSessions",
    "PushAlertSignal",
    "TakeAlertSignals",
    "PutAlert",
    "RemoveAlert",
    "PruneAlerts",
    "SetDestinationHealth",
    "RemoveDestinationHealth",
    "PutProxySession",
    "RemoveProxySession",
    "PutProxyToken",
    "RedeemProxyToken",
    "RecordCrashMove",
    "NoOp",
    "ClearEmergency",
    "SetPrioritized",
    "Served",
    "SetStackTerraform",
    "PutTerraformState",
    "RecordTerraformRun",
    "PutTerraformOutputs",
    "RequestTerraformRun",
];

// Why this mirror (and the hand-written `Deserialize`) must exist: deriving `Deserialize`
// on `Request` directly with `#[serde(untagged)] Unknown` would also turn a *known* tag
// with a malformed body into `Unknown` (a silent no-op on this node while others apply it),
// instead of the hard error we require. Only a never-heard-of tag may fall back, so the tag
// check must run before variant parsing, which needs a separate derived impl (`serde(remote)`).
//
// Mirrors `Request` field-for-field so `#[derive(Deserialize)]` can generate the
// per-variant parsing under `serde(remote)`. Every new `Request` variant must be
// added here too: nothing forces it, and drift would make a known variant
// deserialize as `Unknown` (the `every_variant_name_round_trips` test is the tripwire).
mod request_mirror {
    use super::*;

    #[derive(Deserialize)]
    #[serde(remote = "Request")]
    pub enum RequestMirror {
        PutClusterConfig {
            yaml: String,
            expect_generation: u64,
        },
        UpsertNode(NodeState),
        SetAvailability {
            node: String,
            availability: Availability,
        },
        SetReachable {
            node: String,
            reachable: bool,
        },
        RemoveNode {
            node: String,
        },
        RetireNodeId {
            id: uuid::Uuid,
        },
        DeployStack {
            name: String,
            spec: String,
            #[serde(default)]
            at: i64,
        },
        RemoveStack {
            name: String,
        },
        SetStackObjects {
            name: String,
            configs: BTreeSet<String>,
            secrets: BTreeSet<String>,
            #[serde(default)]
            used_configs: BTreeSet<String>,
            #[serde(default)]
            used_secrets: BTreeSet<String>,
        },
        Assign {
            instance: InstanceId,
            node: String,
            drives: BTreeSet<String>,
        },
        Unassign {
            instance: InstanceId,
        },
        SetDesiredState {
            instance: InstanceId,
            desired: DesiredState,
        },
        SetImageDigest {
            instance: InstanceId,
            digest: String,
        },
        BeginMigration {
            instance: InstanceId,
            to: String,
            drives: BTreeSet<String>,
        },
        AdvanceMigration {
            instance: InstanceId,
            phase: MigrationPhase,
        },
        ClearMigration {
            instance: InstanceId,
        },
        UpsertVolume {
            id: VolumeId,
            primary: String,
        },
        SetVolumeReplica {
            id: VolumeId,
            node: String,
            generation: u64,
        },
        TouchVolume {
            id: VolumeId,
            primary: String,
        },
        SetVolumeSyncRefusal {
            id: VolumeId,
            refusal: Option<SyncRefusal>,
        },
        SetVolumeEmpty {
            id: VolumeId,
            empty: bool,
        },
        RemoveVolume {
            id: VolumeId,
        },
        AckVolumeRemoved {
            id: VolumeId,
            node: String,
        },
        FreezeVolume {
            id: VolumeId,
            node: String,
            stopped: Vec<InstanceId>,
        },
        UnfreezeVolume {
            id: VolumeId,
        },
        StoreCertificate(CertificateState),
        RemoveCertificate {
            fqdn: String,
        },
        SetCertStatus {
            fqdn: String,
            status: ReconcileStatus,
        },
        UpsertDnsRecord(DnsRecordState),
        RemoveDnsRecord {
            key: String,
        },
        SetDnsStatus {
            fqdn: String,
            status: ReconcileStatus,
        },
        ClearReconcileStatus {
            fqdn: String,
        },
        PutConfig {
            name: String,
            content: String,
            #[serde(default)]
            owner: Option<String>,
        },
        PutConfigCas {
            name: String,
            content: String,
            #[serde(default)]
            owner: Option<String>,
            expect_revision: u64,
        },
        RemoveConfig {
            name: String,
        },
        PutSecret {
            name: String,
            sealed: String,
            key_fingerprint: String,
            #[serde(default)]
            owner: Option<String>,
        },
        RemoveSecret {
            name: String,
        },
        PutWrappedKey {
            node: String,
            entry: WrappedKeyEntry,
        },
        SetOverlayAllocations {
            allocations: String,
        },
        PutBackupDestination(BackupDestination),
        RemoveBackupDestination {
            id: String,
        },
        PutBackupTarget(DriveBackupTarget),
        RemoveBackupTarget {
            id: String,
        },
        SetBackupTargetLastRun {
            id: String,
            at: i64,
        },
        RecordBackupRun(BackupRun),
        PutRestoreRequest(RestoreRequest),
        SetRestoreStatus {
            id: String,
            status: RestoreStatus,
            at: i64,
        },
        ReportContainerStatus {
            node: String,
            running: BTreeMap<InstanceId, bool>,
            #[serde(default)]
            secret_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
            #[serde(default)]
            config_revisions: BTreeMap<InstanceId, BTreeMap<String, u64>>,
        },
        SetInstanceUpdate {
            instance: InstanceId,
            node: String,
            phase: UpdatePhase,
            #[serde(default)]
            error: Option<String>,
            at: i64,
        },
        ClearInstanceUpdate {
            instance: InstanceId,
        },
        SetAutoUpdate {
            instance: InstanceId,
            digest: String,
            outcome: AutoUpdateOutcome,
            #[serde(default)]
            error: Option<String>,
            at: i64,
        },
        SetImageChecks {
            checks: BTreeMap<String, ImageCheck>,
            at: i64,
        },
        ClearErrors,
        PutSession {
            key: String,
            session: SessionRecord,
            now: i64,
        },
        RemoveSession {
            key: String,
        },
        PruneSessions {
            now: i64,
        },
        PushAlertSignal(AlertSignal),
        TakeAlertSignals {
            ids: Vec<String>,
        },
        PutAlert(AlertRecord),
        RemoveAlert {
            id: String,
        },
        PruneAlerts {
            before: i64,
            max: usize,
        },
        SetDestinationHealth {
            name: String,
            health: DestinationHealth,
        },
        RemoveDestinationHealth {
            name: String,
        },
        PutProxySession {
            key: String,
            session: ProxySessionRecord,
            now: i64,
        },
        RemoveProxySession {
            key: String,
        },
        PutProxyToken {
            key: String,
            token: ProxyTokenRecord,
            now: i64,
        },
        RedeemProxyToken {
            token_key: String,
            host: String,
            session_key: String,
            now: i64,
        },
        RecordCrashMove {
            instance: InstanceId,
            from: String,
            at: u64,
        },
        NoOp,
        ClearEmergency,
        SetPrioritized {
            stack: String,
            on: bool,
        },
        Served {
            drive: String,
            op: crate::served::ServedOp,
        },
        SetStackTerraform {
            stack: String,
            modules: BTreeMap<String, String>,
        },
        PutTerraformState {
            key: String,
            sealed: String,
            key_fingerprint: String,
            expect_serial: u64,
        },
        RecordTerraformRun {
            run: crate::terraform::TerraformRun,
        },
        PutTerraformOutputs {
            key: String,
            run_id: String,
            secrets: Vec<crate::terraform::TerraformSecretOutput>,
            configs: Vec<crate::terraform::TerraformConfigOutput>,
            #[serde(default)]
            claims: Vec<crate::terraform::OutputObject>,
        },
        RequestTerraformRun {
            key: String,
            mode: crate::terraform::TerraformMode,
        },
        // `Unknown` is absent: it is only ever the fallback when nothing matches.
    }
}

// Hand-written like `BackupRunOutcome`'s impl, but for forward compatibility: a
// tag from a newer node must not fail. Delegates to `request_mirror::RequestMirror`
// (`serde(remote)`) and falls back to `Request::Unknown` when the tag is not in
// `KNOWN_REQUEST_VARIANTS` (a known tag with a bad body is an error). Also
// covers raft replication itself, since openraft's `Entry` embeds `Request`:
// this protects an old follower from a newer leader's new command.
impl<'de> Deserialize<'de> for Request {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        // The externally-tagged variant name: bare string or sole object key.
        let tag = match &value {
            serde_json::Value::String(s) => Some(s.as_str()),
            serde_json::Value::Object(map) if map.len() == 1 => {
                map.keys().next().map(|k| k.as_str())
            }
            _ => None,
        };
        match tag {
            // Only a never-heard-of tag is "from a newer node" (old logs wrote "Unknown").
            Some(tag) if tag == "Unknown" || !KNOWN_REQUEST_VARIANTS.contains(&tag) => {
                Ok(Request::Unknown(value))
            }
            // A known tag with an unparseable body is a bug or corruption, not version
            // skew: fail loudly rather than no-op here while other nodes apply it.
            _ => {
                request_mirror::RequestMirror::deserialize(&value).map_err(serde::de::Error::custom)
            }
        }
    }
}
