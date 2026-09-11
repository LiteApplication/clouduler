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
}

#[derive(Serialize)]
struct CertificateSummary {
    fqdn: String,
    not_after: i64,
    /// True once the chain is parseable enough to say who issued it; kept
    /// simple since the full issuer name is not tracked in cluster state.
    has_chain: bool,
}

async fn list_certificates(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<CertificateSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_state
            .certificates
            .values()
            .map(|c| CertificateSummary {
                fqdn: c.fqdn.clone(),
                not_after: c.not_after,
                has_chain: !c.chain_pem.is_empty(),
            })
            .collect(),
    )
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
}

async fn list_dns(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<DnsRecordSummary>> {
    let cluster_state = state.cluster.state().await;
    Json(
        cluster_state
            .dns_records
            .values()
            .map(|r| DnsRecordSummary {
                fqdn: r.fqdn.clone(),
                record_type: r.record_type.clone(),
                content: r.content.clone(),
                zone: r.zone.clone(),
            })
            .collect(),
    )
}

/// The reconcile loop itself runs in the daemon (`clouduler::proxy`, every
/// 60s on the leader), not in this crate, and there is no channel from here
/// to nudge it early. This reports what the api can actually promise today
/// (a pass is already running periodically) rather than pretending to force
/// an immediate one.
async fn reconcile(_admin: RequireAdmin) -> StatusCode {
    StatusCode::ACCEPTED
}
