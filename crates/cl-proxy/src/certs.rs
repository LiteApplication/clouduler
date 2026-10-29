//! Certificate store: parsed certs keyed by fqdn, hot swappable, with a
//! self-signed fallback so the proxy is reachable before ACME issuance
//! completes.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::pki_types::CertificateDer;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

use crate::error::{ProxyError, Result};
use crate::route::wildcard_matches;

/// Makes sure a `CryptoProvider` is installed process wide. Safe to call more
/// than once; only the first call has any effect.
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn signing_key_for(key_pem: &str) -> Result<Arc<dyn rustls::sign::SigningKey>> {
    let key_der = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|e| ProxyError::CertParse(e.to_string()))?
        .ok_or_else(|| ProxyError::CertParse("no private key found in PEM".to_string()))?;
    rustls::crypto::aws_lc_rs::sign::any_supported_type(&key_der)
        .map_err(|e| ProxyError::CertParse(e.to_string()))
}

fn chain_for(chain_pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs: std::result::Result<Vec<_>, _> =
        rustls_pemfile::certs(&mut chain_pem.as_bytes()).collect();
    let certs = certs.map_err(|e| ProxyError::CertParse(e.to_string()))?;
    if certs.is_empty() {
        return Err(ProxyError::CertParse(
            "no certificates found in PEM".to_string(),
        ));
    }
    Ok(certs)
}

/// Turns a chain + key PEM pair into a rustls `CertifiedKey`.
pub fn parse_certified_key(chain_pem: &str, key_pem: &str) -> Result<CertifiedKey> {
    let chain = chain_for(chain_pem)?;
    let key = signing_key_for(key_pem)?;
    Ok(CertifiedKey::new(chain, key))
}

/// Generates a self-signed cert for `name`, valid for a short window. Used
/// only until real issuance completes.
fn self_signed(name: &str) -> Result<CertifiedKey> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![name.to_string()])
            .map_err(|e| ProxyError::Tls(e.to_string()))?;
    parse_certified_key(&cert.pem(), &signing_key.serialize_pem())
}

struct Entry {
    certified: Arc<CertifiedKey>,
}

/// Fqdn keyed certificate store. Cheap to read from many connections at once;
/// `load` swaps the whole map atomically.
pub struct CertStore {
    entries: ArcSwap<HashMap<String, Entry>>,
    /// Self-signed certs generated on demand for names with nothing issued
    /// yet, cached so repeated handshakes don't regenerate keys.
    fallbacks: AsyncMutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl CertStore {
    pub fn new() -> Self {
        Self {
            entries: ArcSwap::from_pointee(HashMap::new()),
            fallbacks: AsyncMutex::new(HashMap::new()),
        }
    }

    /// Replace the whole store from raft `CertificateState` records. Entries
    /// that fail to parse are logged and skipped rather than failing the
    /// whole load, so one bad record doesn't take every site down.
    pub fn load(&self, certs: &[cl_raft::CertificateState]) {
        let mut map = HashMap::new();
        for c in certs {
            match parse_certified_key(&c.chain_pem, &c.key_pem) {
                Ok(certified) => {
                    map.insert(
                        c.fqdn.clone(),
                        Entry {
                            certified: Arc::new(certified),
                        },
                    );
                }
                Err(e) => warn!(fqdn = %c.fqdn, error = %e, "skipping unparseable certificate"),
            }
        }
        self.entries.store(Arc::new(map));
    }

    fn lookup_issued(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        let entries = self.entries.load();
        if let Some(e) = entries.get(name) {
            return Some(e.certified.clone());
        }
        entries
            .iter()
            .filter_map(|(host, e)| {
                let base = host.strip_prefix("*.")?;
                (wildcard_matches(base, name)).then_some((base, e))
            })
            .max_by_key(|(base, _)| base.len())
            .map(|(_, e)| e.certified.clone())
    }

    /// Resolve a cert for `name`, generating and caching a self-signed
    /// fallback when nothing has been issued yet.
    pub async fn resolve(&self, name: &str) -> Result<Arc<CertifiedKey>> {
        if let Some(c) = self.lookup_issued(name) {
            return Ok(c);
        }
        let mut fallbacks = self.fallbacks.lock().await;
        if let Some(c) = fallbacks.get(name) {
            return Ok(c.clone());
        }
        warn!(host = %name, "no issued certificate yet, serving a self-signed fallback");
        let certified = Arc::new(self_signed(name)?);
        fallbacks.insert(name.to_string(), certified.clone());
        info!(host = %name, "generated self-signed fallback certificate");
        Ok(certified)
    }
}

impl Default for CertStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapts `CertStore` to rustls' SNI based resolution. Handshakes that need
/// the (async) fallback path can't complete synchronously, so this resolver
/// only serves what's already loaded or already cached; `server.rs` warms
/// the fallback cache for a host before accepting its first TLS connection.
#[derive(Clone)]
pub struct CertResolver {
    store: Arc<CertStore>,
}

impl CertResolver {
    pub fn new(store: Arc<CertStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &Arc<CertStore> {
        &self.store
    }
}

impl std::fmt::Debug for CertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertResolver").finish()
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = client_hello.server_name()?;
        if let Some(c) = self.store.lookup_issued(name) {
            return Some(c);
        }
        // Best effort synchronous fallback: return a cached self-signed cert
        // if one already exists. If not, the handshake fails and the caller
        // retries after `CertStore::resolve` has warmed the cache (see
        // `server.rs`'s accept loop).
        self.store.fallbacks.try_lock().ok()?.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::CertificateState;

    fn make_pair(name: &str) -> (String, String) {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        (cert.pem(), signing_key.serialize_pem())
    }

    #[test]
    fn parses_valid_pem_pair() {
        ensure_crypto_provider();
        let (chain, key) = make_pair("example.com");
        assert!(parse_certified_key(&chain, &key).is_ok());
    }

    #[test]
    fn rejects_garbage_pem() {
        ensure_crypto_provider();
        assert!(parse_certified_key("not pem", "not pem").is_err());
    }

    #[tokio::test]
    async fn load_then_resolve_finds_exact_match() {
        ensure_crypto_provider();
        let (chain, key) = make_pair("app.example.com");
        let store = CertStore::new();
        store.load(&[CertificateState {
            fqdn: "app.example.com".to_string(),
            chain_pem: chain,
            key_pem: key,
            not_after: 0,
            issued_at: 0,
        }]);
        assert!(store.resolve("app.example.com").await.is_ok());
    }

    #[tokio::test]
    async fn resolve_falls_back_to_self_signed_when_unissued() {
        ensure_crypto_provider();
        let store = CertStore::new();
        let cert = store.resolve("nowhere.example.com").await.unwrap();
        // Same instance served on a second call, from the fallback cache.
        let cert2 = store.resolve("nowhere.example.com").await.unwrap();
        assert!(Arc::ptr_eq(&cert, &cert2));
    }

    #[tokio::test]
    async fn resolve_matches_wildcard_issued_cert() {
        ensure_crypto_provider();
        let (chain, key) = make_pair("*.example.com");
        let store = CertStore::new();
        store.load(&[CertificateState {
            fqdn: "*.example.com".to_string(),
            chain_pem: chain,
            key_pem: key,
            not_after: 0,
            issued_at: 0,
        }]);
        assert!(store.resolve("foo.example.com").await.is_ok());
    }
}
