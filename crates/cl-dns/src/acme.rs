//! DNS-01 hooks for ACME certificate issuance. Purely Cloudflare record
//! plumbing: the ACME protocol itself lives elsewhere.

use std::time::{Duration, Instant};

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

/// Polls Cloudflare's own view of `_acme-challenge.<domain>` until a TXT
/// record with `expected_value` shows up, or `timeout` elapses.
///
/// This checks Cloudflare's API, not public DNS: it tells us the record is
/// live on their side, which is what our own resolvers will eventually see.
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
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(DnsError::PropagationTimeout(name));
        }
        tokio::time::sleep(poll_interval).await;
    }
}
