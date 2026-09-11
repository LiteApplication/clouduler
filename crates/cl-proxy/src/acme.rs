//! ACME certificate issuance and renewal over DNS-01, using `cl_dns` for the
//! Cloudflare side of the challenge.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cl_dns::Cloudflare;
use cl_raft::{CertificateState, ClusterNode, Request};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use tracing::{error, info, warn};

use crate::error::{ProxyError, Result};
use crate::http01::Http01Store;

/// Let's Encrypt's staging directory. Use for tests and the qemu harness so
/// issuance doesn't burn production rate limits. `AcmeConfig::default` points
/// at production.
pub const LETS_ENCRYPT_STAGING_URL: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";
const LETS_ENCRYPT_PRODUCTION_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// How long before expiry a certificate is renewed.
pub const RENEWAL_WINDOW: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long to wait for a DNS-01 TXT record to become visible to the CA
/// before giving up on an authorization.
pub const DEFAULT_PROPAGATION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct AcmeConfig {
    pub directory_url: String,
    pub contact_emails: Vec<String>,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            directory_url: LETS_ENCRYPT_PRODUCTION_URL.to_string(),
            contact_emails: Vec::new(),
        }
    }
}

fn acme_err(e: instant_acme::Error) -> ProxyError {
    ProxyError::Acme(e.to_string())
}

/// Loads a persisted account, or creates and persists a new one when none
/// exists yet at `credentials_path`.
pub async fn load_or_create_account(
    config: &AcmeConfig,
    credentials_path: &Path,
) -> Result<Account> {
    if let Ok(raw) = tokio::fs::read(credentials_path).await {
        let creds: AccountCredentials =
            serde_json::from_slice(&raw).map_err(|e| ProxyError::Acme(e.to_string()))?;
        let account = Account::builder()
            .map_err(acme_err)?
            .from_credentials(creds)
            .await
            .map_err(acme_err)?;
        return Ok(account);
    }

    let contact: Vec<String> = config
        .contact_emails
        .iter()
        .map(|e| format!("mailto:{e}"))
        .collect();
    let contact_refs: Vec<&str> = contact.iter().map(String::as_str).collect();

    let (account, credentials) = Account::builder()
        .map_err(acme_err)?
        .create(
            &NewAccount {
                contact: &contact_refs,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            config.directory_url.clone(),
            None,
        )
        .await
        .map_err(acme_err)?;

    let serialized =
        serde_json::to_vec_pretty(&credentials).map_err(|e| ProxyError::Acme(e.to_string()))?;
    if let Some(parent) = credentials_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(credentials_path, serialized).await?;

    Ok(account)
}

/// A freshly issued certificate.
pub struct IssuedCertificate {
    pub chain_pem: String,
    pub key_pem: String,
    /// Unix seconds, parsed from the leaf certificate.
    pub not_after: i64,
}

/// Orders a certificate covering `names` (which may include `*.domain`
/// wildcard entries) via DNS-01, driven through `cl_dns`. Every challenge
/// record created is cleared at the end regardless of whether issuance
/// succeeded, so a failed order never leaves stray TXT records behind.
pub async fn issue_certificate(
    account: &Account,
    cf: &Cloudflare,
    zone_id: &str,
    names: &[String],
    propagation_timeout: Duration,
) -> Result<IssuedCertificate> {
    let identifiers: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .map_err(acme_err)?;

    let mut created: Vec<cl_dns::Record> = Vec::new();
    let setup: Result<()> =
        complete_authorizations(&mut order, cf, zone_id, propagation_timeout, &mut created).await;

    for record in &created {
        if let Err(e) = cl_dns::clear_challenge(cf, zone_id, record).await {
            warn!(record = %record.name, error = %e, "failed to clear acme challenge record");
        }
    }
    setup?;

    let status = order
        .poll_ready(&RetryPolicy::default())
        .await
        .map_err(acme_err)?;
    if status != OrderStatus::Ready {
        return Err(ProxyError::Acme(format!("order not ready: {status:?}")));
    }

    let key_pem = order.finalize().await.map_err(acme_err)?;
    let chain_pem = order
        .poll_certificate(&RetryPolicy::default())
        .await
        .map_err(acme_err)?;
    let not_after = leaf_not_after(&chain_pem)?;

    Ok(IssuedCertificate {
        chain_pem,
        key_pem,
        not_after,
    })
}

async fn complete_authorizations(
    order: &mut instant_acme::Order,
    cf: &Cloudflare,
    zone_id: &str,
    propagation_timeout: Duration,
    created: &mut Vec<cl_dns::Record>,
) -> Result<()> {
    let mut authorizations = order.authorizations();
    while let Some(authz) = authorizations.next().await {
        let mut authz = authz.map_err(acme_err)?;
        if authz.status == AuthorizationStatus::Valid {
            continue;
        }

        let mut challenge = authz
            .challenge(ChallengeType::Dns01)
            .ok_or_else(|| ProxyError::Acme("ca did not offer a dns-01 challenge".to_string()))?;

        let domain = match challenge.identifier().identifier {
            Identifier::Dns(dns) => dns.clone(),
            other => {
                return Err(ProxyError::Acme(format!(
                    "unsupported identifier type: {other:?}"
                )))
            }
        };
        let key_auth = challenge.key_authorization();
        let value = key_auth.dns_value();

        let record = cl_dns::set_challenge(cf, zone_id, &domain, &value).await?;
        created.push(record);

        cl_dns::await_propagation(cf, zone_id, &domain, &value, propagation_timeout)
            .await
            .map_err(|_| ProxyError::ChallengeTimeout)?;

        challenge.set_ready().await.map_err(acme_err)?;
    }
    Ok(())
}

/// Orders a certificate covering a single `name` via HTTP-01, answered by
/// the plain-http listener out of `store`. Used only for a name the caller
/// has decided is not covered by any DNS provider zone (an explicit
/// trailing-dot stack hostname, in practice) - every other name goes through
/// `issue_certificate`'s DNS-01 instead. As with DNS-01, every token this
/// places in `store` is removed at the end regardless of outcome, so a
/// failed order never leaves a stray challenge response behind.
pub async fn issue_certificate_http01(
    account: &Account,
    store: &Http01Store,
    name: &str,
) -> Result<IssuedCertificate> {
    let identifiers = vec![Identifier::Dns(name.to_string())];
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .map_err(acme_err)?;

    let mut tokens: Vec<String> = Vec::new();
    let setup: Result<()> = complete_authorizations_http01(&mut order, store, &mut tokens).await;

    for token in &tokens {
        store.remove(token);
    }
    setup?;

    let status = order
        .poll_ready(&RetryPolicy::default())
        .await
        .map_err(acme_err)?;
    if status != OrderStatus::Ready {
        return Err(ProxyError::Acme(format!("order not ready: {status:?}")));
    }

    let key_pem = order.finalize().await.map_err(acme_err)?;
    let chain_pem = order
        .poll_certificate(&RetryPolicy::default())
        .await
        .map_err(acme_err)?;
    let not_after = leaf_not_after(&chain_pem)?;

    Ok(IssuedCertificate {
        chain_pem,
        key_pem,
        not_after,
    })
}

async fn complete_authorizations_http01(
    order: &mut instant_acme::Order,
    store: &Http01Store,
    tokens: &mut Vec<String>,
) -> Result<()> {
    let mut authorizations = order.authorizations();
    while let Some(authz) = authorizations.next().await {
        let mut authz = authz.map_err(acme_err)?;
        if authz.status == AuthorizationStatus::Valid {
            continue;
        }

        let mut challenge = authz
            .challenge(ChallengeType::Http01)
            .ok_or_else(|| ProxyError::Acme("ca did not offer an http-01 challenge".to_string()))?;

        let token = challenge.token.clone();
        let key_authorization = challenge.key_authorization().as_str().to_string();
        store.set(token.clone(), key_authorization);
        tokens.push(token);

        challenge.set_ready().await.map_err(acme_err)?;
    }
    Ok(())
}

fn leaf_not_after(chain_pem: &str) -> Result<i64> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(chain_pem.as_bytes())
        .map_err(|e| ProxyError::CertParse(e.to_string()))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| ProxyError::CertParse(e.to_string()))?;
    Ok(cert.validity().not_after.timestamp())
}

/// Whether a certificate expiring at `not_after` (unix seconds) needs
/// renewal, given `now` and the renewal window.
pub fn needs_renewal(not_after: i64, now: i64, window: Duration) -> bool {
    not_after - now <= window.as_secs() as i64
}

/// Looks up a zone id for a certificate's fqdn. Kept abstract because
/// zone-to-domain mapping lives in the cluster config, which this crate
/// doesn't depend on.
pub trait ZoneResolver: Send + Sync {
    fn zone_id_for(&self, fqdn: &str) -> Option<String>;
}

impl<F> ZoneResolver for F
where
    F: Fn(&str) -> Option<String> + Send + Sync,
{
    fn zone_id_for(&self, fqdn: &str) -> Option<String> {
        self(fqdn)
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One pass over the leader's certificate store, renewing anything inside
/// the renewal window. Runs only when `node` currently leads; a follower
/// call is a no-op so callers can invoke this on a plain timer everywhere.
///
/// A name `zones` can't resolve falls back to HTTP-01 (through `http01_store`)
/// when `allow_http01` says so - the same opt-in a fresh issuance checks, so
/// a name issued that way keeps renewing that way rather than erroring once
/// `zones` still can't cover it.
pub async fn renewal_tick(
    node: &ClusterNode,
    account: &Account,
    cf: &Cloudflare,
    zones: &dyn ZoneResolver,
    allow_http01: &(dyn Fn(&str) -> bool + Send + Sync),
    http01_store: &Http01Store,
    propagation_timeout: Duration,
) {
    if !node.is_leader() {
        return;
    }
    let state = node.state().await;
    let now = now_unix();
    for cert in state.certificates.values() {
        if !needs_renewal(cert.not_after, now, RENEWAL_WINDOW) {
            continue;
        }
        info!(fqdn = %cert.fqdn, "renewing certificate");
        let names = vec![cert.fqdn.clone()];
        let issued = match zones.zone_id_for(&cert.fqdn) {
            Some(zone_id) => {
                issue_certificate(account, cf, &zone_id, &names, propagation_timeout).await
            }
            None if allow_http01(&cert.fqdn) => {
                issue_certificate_http01(account, http01_store, &cert.fqdn).await
            }
            None => {
                warn!(fqdn = %cert.fqdn, "no zone configured for certificate, skipping renewal");
                continue;
            }
        };
        match issued {
            Ok(issued) => {
                let req = Request::StoreCertificate(CertificateState {
                    fqdn: cert.fqdn.clone(),
                    chain_pem: issued.chain_pem,
                    key_pem: issued.key_pem,
                    not_after: issued.not_after,
                });
                if let Err(e) = node.submit(req).await {
                    error!(fqdn = %cert.fqdn, error = %e, "failed to store renewed certificate");
                }
            }
            Err(e) => error!(fqdn = %cert.fqdn, error = %e, "certificate renewal failed"),
        }
    }
}

/// Runs `renewal_tick` on `check_interval` forever. Meant to be spawned as a
/// background task for the life of the process.
#[allow(clippy::too_many_arguments)]
pub async fn run_renewal_loop(
    node: Arc<ClusterNode>,
    account: Account,
    cf: Cloudflare,
    zones: Arc<dyn ZoneResolver>,
    allow_http01: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    http01_store: Arc<Http01Store>,
    propagation_timeout: Duration,
    check_interval: Duration,
) -> ! {
    let mut ticker = tokio::time::interval(check_interval);
    loop {
        ticker.tick().await;
        renewal_tick(
            &node,
            &account,
            &cf,
            zones.as_ref(),
            allow_http01.as_ref(),
            &http01_store,
            propagation_timeout,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_directory_is_production() {
        assert_eq!(
            AcmeConfig::default().directory_url,
            LETS_ENCRYPT_PRODUCTION_URL
        );
    }

    #[test]
    fn staging_constant_is_distinct_from_default() {
        assert_ne!(
            LETS_ENCRYPT_STAGING_URL,
            AcmeConfig::default().directory_url
        );
    }

    #[test]
    fn needs_renewal_within_window() {
        let now = 1_000_000;
        let window = Duration::from_secs(30 * 24 * 60 * 60);
        assert!(needs_renewal(now + 10, now, window));
        assert!(!needs_renewal(
            now + window.as_secs() as i64 + 10,
            now,
            window
        ));
    }

    #[test]
    fn leaf_not_after_parses_generated_cert() {
        let rcgen::CertifiedKey { cert, .. } =
            rcgen::generate_simple_self_signed(vec!["example.com".to_string()]).unwrap();
        let not_after = leaf_not_after(&cert.pem()).unwrap();
        assert!(not_after > 0);
    }
}
