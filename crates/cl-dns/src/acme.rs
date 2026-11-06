//! DNS-01 hooks for ACME certificate issuance. Purely Cloudflare record
//! plumbing: the ACME protocol itself lives elsewhere.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use hickory_resolver::config::{NameServerConfig, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType as ResolveRecordType};
use hickory_resolver::Resolver;

use crate::api::{Cloudflare, Record, RecordInput, RecordType};
use crate::error::{DnsError, Result};

fn challenge_name(domain: &str) -> String {
    format!("_acme-challenge.{domain}")
}

/// Creates the `_acme-challenge.<domain>` TXT record with the given key
/// authorization digest. Cloudflare allows multiple TXT records at the same
/// name, so this doesn't need to check for an existing one first; callers
/// doing renewal should `clear_challenge` the old one when done.
pub async fn set_challenge(
    cf: &Cloudflare,
    zone_id: &str,
    domain: &str,
    token_value: &str,
) -> Result<Record> {
    let input = RecordInput {
        name: challenge_name(domain),
        kind: RecordType::TXT,
        content: token_value.to_string(),
        ttl: 60, // short lived, this record is torn down right after issuance
        proxied: false,
        comment: None,
    };
    cf.create_record(zone_id, &input).await
}

/// Removes a challenge record once issuance has completed.
pub async fn clear_challenge(cf: &Cloudflare, zone_id: &str, record: &Record) -> Result<()> {
    let id = record
        .id
        .as_deref()
        .ok_or_else(|| DnsError::RecordNotFound(record.name.clone()))?;
    cf.delete_record(zone_id, id).await
}

/// Confirms `_acme-challenge.<domain>` is actually resolvable with
/// `expected_value` before letting the caller tell the CA to validate it.
///
/// Checking Cloudflare's API (as this used to do exclusively) only proves the
/// record exists in their control plane; the CA queries DNS, and control
/// plane writes lag visibility on Cloudflare's authoritative edge network by
/// anywhere from nothing to tens of seconds. Asking Cloudflare first is still
/// worthwhile — it fails fast when the record was never created at all — but
/// only a real DNS answer from the zone's own authoritative nameservers, not
/// a recursive resolver that might be holding an earlier negative answer in
/// cache, tells us what the CA is actually going to see.
pub async fn await_propagation(
    cf: &Cloudflare,
    zone_id: &str,
    domain: &str,
    expected_value: &str,
    timeout: Duration,
) -> Result<()> {
    let name = challenge_name(domain);
    let deadline = Instant::now() + timeout;
    let poll_interval = Duration::from_secs(2);

    loop {
        let records = cf
            .list_records(zone_id, Some(&name), Some(RecordType::TXT))
            .await?;
        if records.iter().any(|r| r.content == expected_value) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(DnsError::PropagationTimeout(name));
        }
        tokio::time::sleep(poll_interval).await;
    }

    let (zone, name_servers) = cf.zone_info(zone_id).await?;
    await_authoritative(&zone, &name_servers, &name, expected_value, deadline).await
}

/// Polls the zone's own authoritative nameservers directly for `name` until
/// a TXT record matching `expected_value` shows up, or `deadline` passes.
async fn await_authoritative(
    zone: &str,
    name_servers: &[String],
    name: &str,
    expected_value: &str,
    deadline: Instant,
) -> Result<()> {
    let servers = authoritative_servers(zone, name_servers).await?;
    let resolver = direct_resolver(&servers)?;
    let poll_interval = Duration::from_secs(2);

    loop {
        if let Ok(lookup) = resolver
            .lookup(format!("{name}."), ResolveRecordType::TXT)
            .await
        {
            let matched = lookup.answers().iter().any(|record| match &record.data {
                RData::TXT(txt) => {
                    txt.txt_data
                        .iter()
                        .map(|chunk| String::from_utf8_lossy(chunk))
                        .collect::<String>()
                        == expected_value
                }
                _ => false,
            });
            if matched {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(DnsError::PropagationTimeout(name.to_string()));
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Public recursive resolvers used to find the zone's nameservers. Never the
/// host's own resolver: a node behind split-horizon DNS (Tailscale MagicDNS
/// forwarding the zone to the cluster's own DNS server, say) answers for the
/// zone itself, with no NS records, and the real authoritative servers cannot
/// be found through it.
const PUBLIC_RESOLVERS: [IpAddr; 3] = [
    IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1)),
    IpAddr::V4(std::net::Ipv4Addr::new(9, 9, 9, 9)),
    IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8)),
];

/// Where the nameservers of a zone come from. A trait so the selection logic
/// can be tested without a network.
trait NameserverSource {
    /// NS hostnames for `zone` according to DNS; empty when none are returned.
    async fn ns_hostnames(&self, zone: &str) -> Result<Vec<String>>;
    /// Addresses of one nameserver hostname.
    async fn addresses(&self, host: &str) -> Vec<IpAddr>;
}

struct PublicDns(Resolver<TokioRuntimeProvider>);

impl PublicDns {
    fn new() -> Result<Self> {
        let mut config = ResolverConfig::from_parts(None, vec![], vec![]);
        for ip in PUBLIC_RESOLVERS {
            config.add_name_server(NameServerConfig::udp_and_tcp(ip));
        }
        Resolver::builder_with_config(config, TokioRuntimeProvider::default())
            .build()
            .map(Self)
            .map_err(|e| DnsError::Resolve(e.to_string()))
    }
}

impl NameserverSource for PublicDns {
    async fn ns_hostnames(&self, zone: &str) -> Result<Vec<String>> {
        let lookup = self
            .0
            .lookup(format!("{zone}."), ResolveRecordType::NS)
            .await
            .map_err(|e| DnsError::Resolve(format!("no nameservers found for {zone}: {e}")))?;
        Ok(lookup
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::NS(ns) => Some(ns.0.to_string()),
                _ => None,
            })
            .collect())
    }

    async fn addresses(&self, host: &str) -> Vec<IpAddr> {
        match self.0.lookup_ip(host.to_string()).await {
            Ok(addrs) => addrs.iter().collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// The IP addresses of `zone`'s authoritative nameservers. The hostnames come
/// from the Cloudflare API (`api_names`, the zone's own `name_servers`) and
/// only when that is empty from an NS lookup against public resolvers; their
/// addresses are resolved the same way. The system resolver is never used.
async fn authoritative_servers(zone: &str, api_names: &[String]) -> Result<Vec<IpAddr>> {
    resolve_servers(zone, api_names, &PublicDns::new()?).await
}

async fn resolve_servers(
    zone: &str,
    api_names: &[String],
    source: &impl NameserverSource,
) -> Result<Vec<IpAddr>> {
    let names = if api_names.is_empty() {
        source.ns_hostnames(zone).await?
    } else {
        api_names.to_vec()
    };
    if names.is_empty() {
        return Err(DnsError::Resolve(format!(
            "no nameservers found for {zone}"
        )));
    }
    let mut ips = Vec::new();
    for name in &names {
        ips.extend(source.addresses(name).await);
    }
    if ips.is_empty() {
        return Err(DnsError::Resolve(format!(
            "could not resolve any nameserver address for {zone} ({})",
            names.join(", ")
        )));
    }
    Ok(ips)
}

/// A resolver that queries `servers` directly rather than the system's
/// recursive resolver, with no local cache: every call must see whatever
/// those nameservers answer right now, not a cached earlier answer.
fn direct_resolver(servers: &[IpAddr]) -> Result<hickory_resolver::Resolver<TokioRuntimeProvider>> {
    let mut config = ResolverConfig::from_parts(None, vec![], vec![]);
    for ip in servers {
        config.add_name_server(NameServerConfig::udp_and_tcp(*ip));
    }
    let mut builder = Resolver::builder_with_config(config, TokioRuntimeProvider::default());
    builder.options_mut().cache_size = 0;
    builder
        .build()
        .map_err(|e| DnsError::Resolve(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that answers from fixed tables, to prove where the
    /// nameservers come from without any network.
    struct Fake {
        ns: Vec<String>,
        ips: Vec<(&'static str, IpAddr)>,
    }

    impl NameserverSource for Fake {
        async fn ns_hostnames(&self, _zone: &str) -> Result<Vec<String>> {
            Ok(self.ns.clone())
        }
        async fn addresses(&self, host: &str) -> Vec<IpAddr> {
            self.ips
                .iter()
                .filter(|(h, _)| *h == host)
                .map(|(_, ip)| *ip)
                .collect()
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[tokio::test]
    async fn a_resolver_with_no_ns_records_and_no_api_names_is_a_clear_error() {
        let fake = Fake {
            ns: vec![],
            ips: vec![],
        };
        let err = resolve_servers("liteapp.fr", &[], &fake).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("no nameservers found for liteapp.fr"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_zones_api_nameservers_are_used_even_when_dns_has_no_ns_records() {
        let fake = Fake {
            ns: vec![],
            ips: vec![("apollo.ns.cloudflare.com", ip("108.162.1.1"))],
        };
        let api = vec!["apollo.ns.cloudflare.com".to_string()];
        let got = resolve_servers("liteapp.fr", &api, &fake).await.unwrap();
        assert_eq!(got, vec![ip("108.162.1.1")]);
    }

    #[tokio::test]
    async fn dns_ns_records_are_the_fallback_when_the_api_lists_none() {
        let fake = Fake {
            ns: vec!["dara.ns.cloudflare.com".to_string()],
            ips: vec![("dara.ns.cloudflare.com", ip("108.162.2.2"))],
        };
        let got = resolve_servers("liteapp.fr", &[], &fake).await.unwrap();
        assert_eq!(got, vec![ip("108.162.2.2")]);
    }

    #[tokio::test]
    async fn nameservers_that_resolve_to_nothing_name_themselves_in_the_error() {
        let fake = Fake {
            ns: vec![],
            ips: vec![],
        };
        let api = vec!["apollo.ns.cloudflare.com".to_string()];
        let err = resolve_servers("liteapp.fr", &api, &fake)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("apollo.ns.cloudflare.com"),
            "{err}"
        );
    }
}
