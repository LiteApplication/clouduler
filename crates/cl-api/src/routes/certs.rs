//! `/api/certificates`, `/api/dns`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/certificates", get(list_certificates))
        .route("/api/certificates/{fqdn}/renew", post(renew))
        .route("/api/tls-fingerprint", get(tls_fingerprint))
        .route("/api/dns", get(list_dns))
        .route("/api/dns/reconcile", post(reconcile))
        .route("/api/dns/internal-reconcile", post(internal_reconcile))
}

#[derive(Serialize)]
struct CertificateSummary {
    fqdn: String,
    /// Unix seconds. 0 for a hostname with no certificate yet.
    not_after: i64,
    /// Unix seconds the certificate was issued at; 0 when unknown.
    issued_at: i64,
    /// True once the chain is parseable enough to say who issued it; kept
    /// simple since the full issuer name is not tracked in cluster state.
    has_chain: bool,
    /// `ready`: a certificate is held and nothing is wrong. `pending`: no
    /// certificate yet. `error`: issuance or renewal is failing (`error`
    /// says why; a held certificate keeps serving meanwhile).
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Unix seconds of the failure above.
    #[serde(skip_serializing_if = "Option::is_none")]
    error_at: Option<i64>,
}

/// Every certificate held, plus every hostname still waiting on one or whose
/// issuance failed: the union of `certificates` and `cert_status`.
async fn list_certificates(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<CertificateSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(certificate_summaries(&cluster_state))
}

fn certificate_summaries(cluster_state: &cl_raft::ClusterState) -> Vec<CertificateSummary> {
    let status_of = |fqdn: &str| match cluster_state.cert_status.get(fqdn) {
        Some(cl_raft::ReconcileStatus::Error { message, at }) => {
            ("error", Some(message.clone()), Some(*at))
        }
        Some(cl_raft::ReconcileStatus::Pending) => ("pending", None, None),
        None => ("ready", None, None),
    };
    let mut rows: Vec<CertificateSummary> = cluster_state
        .certificates
        .values()
        .map(|c| {
            // A held certificate is never "pending": that status is only
            // meaningful while there is nothing to serve.
            let (status, error, error_at) = match status_of(&c.fqdn) {
                ("pending", ..) => ("ready", None, None),
                other => other,
            };
            CertificateSummary {
                fqdn: c.fqdn.clone(),
                not_after: c.not_after,
                issued_at: c.issued_at,
                has_chain: !c.chain_pem.is_empty(),
                status,
                error,
                error_at,
            }
        })
        .collect();
    rows.extend(
        cluster_state
            .cert_status
            .keys()
            .filter(|fqdn| !cluster_state.certificates.contains_key(*fqdn))
            .map(|fqdn| {
                let (status, error, error_at) = status_of(fqdn);
                CertificateSummary {
                    fqdn: fqdn.clone(),
                    not_after: 0,
                    issued_at: 0,
                    has_chain: false,
                    status,
                    error,
                    error_at,
                }
            }),
    );
    rows
}

#[derive(Serialize)]
struct TlsFingerprint {
    fingerprint: String,
}

/// The fingerprint of the certificate this node is actually serving over
/// TLS, for the "Add a node" flow to hand a joining node its
/// `--tls-fingerprint` pin without the operator having to compare it by
/// hand.
async fn tls_fingerprint(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<TlsFingerprint> {
    Json(TlsFingerprint {
        fingerprint: state.tls_fingerprint.clone(),
    })
}

/// Renewal itself is `cl-dns`'s job (it holds the ACME account and ownership
/// records); this crate has no channel to nudge that loop early. Removing the
/// stored certificate is the best available trigger: the next reconcile pass
/// sees it missing and issues a fresh one.
async fn renew(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(fqdn): Path<String>,
) -> Result<StatusCode, ApiError> {
    let response = state
        .cluster
        .submit(cl_raft::Request::RemoveCertificate { fqdn })
        .await
        .map_err(ApiError::not_leader)?;
    if let Some(msg) = response.error {
        return Err(ApiError::Conflict(msg));
    }
    Ok(StatusCode::OK)
}

#[derive(Serialize)]
struct DnsRecordSummary {
    fqdn: String,
    record_type: String,
    content: String,
    zone: String,
    /// `ready` for a live record, `pending` for a hostname still waiting for
    /// one, `error` when publishing it failed (`error` says why).
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Live records, plus a row for each hostname still waiting on its record or
/// failing to get one (from `dns_status`). Those rows have no type, content
/// or zone yet.
async fn list_dns(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<DnsRecordSummary>> {
    let cluster_state = state.cluster.state().await;
    let mut rows: Vec<DnsRecordSummary> = cluster_state
        .dns_records
        .values()
        .map(|r| DnsRecordSummary {
            fqdn: r.fqdn.clone(),
            record_type: r.record_type.clone(),
            content: r.content.clone(),
            zone: r.zone.clone(),
            status: "ready",
            error: None,
        })
        .collect();
    rows.extend(cluster_state.dns_status.iter().map(|(fqdn, status)| {
        let (status, error) = match status {
            cl_raft::ReconcileStatus::Pending => ("pending", None),
            cl_raft::ReconcileStatus::Error { message, .. } => ("error", Some(message.clone())),
        };
        DnsRecordSummary {
            fqdn: fqdn.clone(),
            record_type: String::new(),
            content: String::new(),
            zone: String::new(),
            status,
            error,
        }
    }));
    Json(rows)
}

/// Runs the dns reconcile and certificate passes now instead of at their
/// next tick. Both loops are leader-only, so a non-leader forwards the nudge
/// to the leader. The passes run in the background: 202 means "asked", the
/// outcome shows up as record and certificate status.
async fn reconcile(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    if state.cluster.is_leader() {
        nudge(&state);
        return Ok(StatusCode::ACCEPTED);
    }
    let leader = super::git_backup::leader_node_name(&state)
        .await
        .ok_or_else(|| ApiError::Unavailable("no cluster leader known right now".into()))?;
    crate::internal_peer::post_empty(&state, &leader, "/api/dns/internal-reconcile").await?;
    Ok(StatusCode::ACCEPTED)
}

/// Node-to-node only: what `reconcile` forwards to the leader.
async fn internal_reconcile(
    _auth: crate::auth::internal::InternalAuth,
    State(state): State<AppState>,
) -> StatusCode {
    nudge(&state);
    StatusCode::ACCEPTED
}

fn nudge(state: &AppState) {
    state.nudges.dns.notify_one();
    state.nudges.acme.notify_one();
}
