//! Talking to a node that is already in the cluster, to join it.
//!
//! The joining machine is not in raft yet, so it cannot read the replicated
//! config. It fetches it over the http api instead, adds itself, and pushes it
//! back; the cluster replicates the result to everyone, this node included, the
//! moment its daemon comes up.

use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use cl_config::ClusterConfig;
use reqwest::StatusCode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// A brand new cluster serves a self signed certificate, so there is no chain
/// to check against. What can be checked is that the certificate is the one the
/// operator expected, by its fingerprint.
///
/// With a pin, a mismatch aborts the join. Without one, the certificate is
/// accepted and its fingerprint recorded so init can print it and the operator
/// can compare it with what the other node logged.
#[derive(Debug)]
struct PinnedVerifier {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

pub fn fingerprint(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Fingerprints compare equal regardless of separators or case, so a value
/// pasted from openssl, a browser or our own output all work.
fn same_fingerprint(a: &str, b: &str) -> bool {
    let normalize = |text: &str| {
        text.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect::<String>()
    };
    normalize(a) == normalize(b)
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = fingerprint(end_entity);
        if let Some(expected) = &self.expected {
            if !same_fingerprint(expected, &seen) {
                return Err(rustls::Error::General(format!(
                    "certificate fingerprint {seen} does not match the pinned {expected}"
                )));
            }
        }
        *self.seen.lock().expect("fingerprint lock") = Some(seen);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}

pub struct Peer {
    client: reqwest::Client,
    base: String,
    token: String,
    seen: Arc<Mutex<Option<String>>>,
}

/// Default port to reach the api on. The web port is in the config we have not
/// fetched yet, so a cluster on a different port has to be given `host:port`.
const DEFAULT_API_PORT: u16 = 8443;

impl Peer {
    pub fn connect(address: &str, token: String, pin: Option<String>) -> Result<Self> {
        let seen = Arc::new(Mutex::new(None));
        let verifier = Arc::new(PinnedVerifier {
            expected: pin,
            seen: seen.clone(),
        });

        // reqwest's rustls setup installs roots we deliberately do not want to
        // consult, so the whole client config is built here instead.
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("building the tls client")?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building the http client")?;

        Ok(Self {
            client,
            base: base_url(address),
            token,
            seen,
        })
    }

    /// Fingerprint of the certificate the peer presented, once a request has
    /// been made.
    pub fn peer_fingerprint(&self) -> Option<String> {
        self.seen.lock().expect("fingerprint lock").clone()
    }

    pub async fn fetch_config(&self) -> Result<ClusterConfig> {
        let response = self
            .client
            .get(format!("{}/api/config", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("asking {} for the cluster config", self.base))?;

        let response = check(response).await?;
        response
            .json()
            .await
            .context("the cluster config the peer returned does not parse")
    }

    /// Push the amended config and confirm it straight away.
    ///
    /// An unconfirmed edit rolls itself back, which for a join would mean the
    /// node quietly disappearing from the cluster a minute later. The window is
    /// left short and closed immediately rather than skipped, so a push that
    /// succeeds but whose confirm fails still ends in a known state.
    pub async fn push_config(&self, config: &ClusterConfig) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            config: &'a ClusterConfig,
            rollback_after: &'a str,
        }

        let response = self
            .client
            .put(format!("{}/api/config", self.base))
            .bearer_auth(&self.token)
            .json(&Body {
                config,
                rollback_after: "120s",
            })
            .send()
            .await
            .context("sending the amended cluster config")?;
        check(response).await?;

        let response = self
            .client
            .post(format!("{}/api/config/confirm", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("confirming the config edit")?;
        check(response).await?;
        Ok(())
    }
}

/// Turn a response into an error carrying what the api actually said, which is
/// the difference between "fix your token" and a shrug.
async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let leader = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.unwrap_or_default();

    match status {
        StatusCode::UNAUTHORIZED => {
            bail!("the cluster rejected the api key. Pass a valid one with --token or --token-file")
        }
        StatusCode::FORBIDDEN => bail!("that api key is not an admin key, so it cannot edit the config"),
        StatusCode::SERVICE_UNAVAILABLE => bail!(
            "the cluster has no leader right now, so it cannot accept the join. {}",
            match leader {
                Some(leader) => format!("Try again through {leader}"),
                None => "Try again once a manager quorum is up".to_string(),
            }
        ),
        _ => bail!("the cluster answered {status}: {}", body.trim()),
    }
}

/// Accept a bare host, a host:port, or a full url.
fn base_url(address: &str) -> String {
    let address = address.trim_end_matches('/');
    if address.starts_with("https://") || address.starts_with("http://") {
        return address.to_string();
    }
    if has_explicit_port(address) {
        return format!("https://{address}");
    }
    format!("https://{address}:{DEFAULT_API_PORT}")
}

/// A colon means a port, except inside a bare ipv6 literal, which has several.
fn has_explicit_port(address: &str) -> bool {
    match address.rfind(':') {
        Some(at) => address[at + 1..].chars().all(|c| c.is_ascii_digit()) && !address[at + 1..].is_empty(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_gets_https_and_the_default_port() {
        assert_eq!(base_url("10.10.0.1"), "https://10.10.0.1:8443");
        assert_eq!(base_url("n1.int.example.com"), "https://n1.int.example.com:8443");
    }

    #[test]
    fn an_explicit_port_or_url_is_left_alone() {
        assert_eq!(base_url("10.10.0.1:8443"), "https://10.10.0.1:8443");
        assert_eq!(base_url("https://n1:8443/"), "https://n1:8443");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_own_port() {
        assert_eq!(base_url("[fd00::1]"), "https://[fd00::1]:8443");
        assert_eq!(base_url("[fd00::1]:8443"), "https://[fd00::1]:8443");
    }

    #[test]
    fn a_fingerprint_is_the_hex_sha256_of_the_certificate() {
        // Known SHA-256 of the empty input.
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let printed = fingerprint(b"").replace(':', "");
        assert_eq!(printed, expected);
    }

    #[test]
    fn fingerprints_compare_past_separators_and_case() {
        assert!(same_fingerprint("AA:BB:CC", "aabbcc"));
        assert!(same_fingerprint("aa bb cc", "AA-BB-CC"));
        assert!(!same_fingerprint("aabbcc", "aabbcd"));
    }

    #[test]
    fn a_pinned_verifier_refuses_a_certificate_that_is_not_the_pinned_one() {
        let verifier = PinnedVerifier {
            expected: Some(fingerprint(b"the real one")),
            seen: Arc::new(Mutex::new(None)),
        };
        let name = ServerName::try_from("n1.example.com").unwrap();

        let good = verifier.verify_server_cert(
            &CertificateDer::from(b"the real one".to_vec()),
            &[],
            &name,
            &[],
            UnixTime::now(),
        );
        let bad = verifier.verify_server_cert(
            &CertificateDer::from(b"an impostor".to_vec()),
            &[],
            &name,
            &[],
            UnixTime::now(),
        );

        assert!(good.is_ok());
        assert!(bad.is_err());
    }

    #[test]
    fn without_a_pin_the_certificate_is_accepted_and_its_fingerprint_recorded() {
        let seen = Arc::new(Mutex::new(None));
        let verifier = PinnedVerifier {
            expected: None,
            seen: seen.clone(),
        };
        let name = ServerName::try_from("n1.example.com").unwrap();

        let result = verifier.verify_server_cert(
            &CertificateDer::from(b"whatever".to_vec()),
            &[],
            &name,
            &[],
            UnixTime::now(),
        );

        assert!(result.is_ok());
        assert_eq!(seen.lock().unwrap().as_deref(), Some(fingerprint(b"whatever").as_str()));
    }
}
