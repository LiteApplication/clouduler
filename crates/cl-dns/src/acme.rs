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
        let records = cf.list_records(zone_id, Some(&name), Some(RecordType::TXT)).await?;
        if records.iter().any(|r| r.content == expected_value) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(DnsError::PropagationTimeout(name));
        }
        tokio::time::sleep(poll_interval).await;
    }

    let zone = cf.zone_name(zone_id).await?;
    await_authoritative(&zone, &name, expected_value, deadline).await
}

/// Polls the zone's own authoritative nameservers directly for `name` until
/// a TXT record matching `expected_value` shows up, or `deadline` passes.
async fn await_authoritative(zone: &str, name: &str, expected_value: &str, deadline: Instant) -> Result<()> {
    let servers = authoritative_servers(zone).await?;
    let resolver = direct_resolver(&servers)?;
    let poll_interval = Duration::from_secs(2);

    loop {
        if let Ok(lookup) = resolver.lookup(format!("{name}."), ResolveRecordType::TXT).await {
            let matched = lookup.answers().iter().any(|record| match &record.data {
                RData::TXT(txt) => txt
                    .txt_data
                    .iter()
                    .map(|chunk| String::from_utf8_lossy(chunk))
                    .collect::<String>()
                    == expected_value,
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

/// The IP addresses of `zone`'s authoritative nameservers, resolved through
/// the system's normal recursive resolver. NS/glue records are set once at
/// registration and don't have the propagation lag this module exists to
/// work around, so a plain recursive lookup is fine here.
async fn authoritative_servers(zone: &str) -> Result<Vec<IpAddr>> {
    let system = hickory_resolver::TokioResolver::builder_tokio()
        .map_err(|e| DnsError::Resolve(e.to_string()))?
        .build()
        .map_err(|e| DnsError::Resolve(e.to_string()))?;

    let ns_lookup = system
        .lookup(format!("{zone}."), ResolveRecordType::NS)
        .await
        .map_err(|e| DnsError::Resolve(format!("no nameservers found for {zone}: {e}")))?;

    let mut ips = Vec::new();
    for record in ns_lookup.answers() {
        let RData::NS(ns) = &record.data else { continue };
        if let Ok(addrs) = system.lookup_ip(ns.0.to_string()).await {
            ips.extend(addrs.iter());
        }
    }
    if ips.is_empty() {
        return Err(DnsError::Resolve(format!("could not resolve any nameserver address for {zone}")));
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
    builder.build().map_err(|e| DnsError::Resolve(e.to_string()))
}
