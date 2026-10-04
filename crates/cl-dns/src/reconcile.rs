//! Diffs desired state against live Cloudflare records and applies the
//! difference. Planning is a pure function so it can be tested without a
//! network; `apply` is the only part that talks to the API.

use std::collections::{HashMap, HashSet};

use crate::api::{Cloudflare, Record, RecordInput, RecordType};
use crate::desired::DesiredRecord;
use crate::error::{DnsError, Result};
use crate::ownership::{parse_tag, record_key, tag_content, tag_name};

/// Changes needed to bring a zone's live records in line with desired state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub create: Vec<RecordInput>,
    /// (record id, new field values)
    pub update: Vec<(String, RecordInput)>,
    /// record ids. Only ever clouduler-owned records land here.
    pub delete: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.create.is_empty() && self.update.is_empty() && self.delete.is_empty()
    }
}

/// Computes the plan to reconcile `live` records toward `desired`, scoped to
/// records tagged as owned by `cluster`. Never *updates or repoints* a live
/// record that isn't clouduler's, even if it shares a name with something
/// desired: content-level collisions are left for the create step to fail
/// loudly on rather than overwritten.
///
/// The one exception is Cloudflare's CNAME-exclusivity rule: a CNAME can't
/// coexist with *any* other record at the same name, in either direction.
/// A stale CNAME (or A/AAAA) left over from a previous scheme - or created
/// by hand - blocks every future create at that name with cloudflare error
/// 81053/81054 and never gets cleared, since it isn't tagged as ours. Those
/// specific conflicts are deleted regardless of ownership; see
/// `delete_conflicting_live_records`.
pub fn plan(cluster: &str, desired: &[DesiredRecord], live: &[Record]) -> Plan {
    // DNS names are case-insensitive and Cloudflare always returns them
    // lowercased, so compare everything lowercased: otherwise a mixed-case
    // desired host never matches its own live record and gets re-created
    // forever.
    let live: Vec<Record> = live
        .iter()
        .cloned()
        .map(|mut r| {
            r.name = r.name.to_ascii_lowercase();
            r
        })
        .collect();
    let live = live.as_slice();
    let desired = normalize_desired(desired);
    let owned_keys = owned_keys(cluster, live);

    let mut live_by_key: HashMap<String, Vec<&Record>> = HashMap::new();
    for r in live {
        if is_tag_record(r) {
            continue;
        }
        let key = record_key(r);
        if owned_keys.contains(&key) {
            live_by_key.entry(key).or_default().push(r);
        }
    }

    let mut desired_by_key: HashMap<String, Vec<&DesiredRecord>> = HashMap::new();
    for d in &desired {
        desired_by_key.entry(desired_key(d)).or_default().push(d);
    }

    let mut out = Plan::default();

    for (key, wants) in &desired_by_key {
        let mut haves = live_by_key.remove(key.as_str()).unwrap_or_default();
        let tag_exists = live.iter().any(|r| is_owning_tag(r, cluster, key));

        for want in wants {
            // Prefer a `have` that already matches: picking an arbitrary one
            // to update instead would try to repoint it to content another
            // live record already holds, which the provider rejects as a
            // duplicate and would abort the whole apply before the real
            // stale records ever get deleted below.
            let matching = haves.iter().position(|have| unchanged(have, want));
            match matching.map(|i| haves.remove(i)).or_else(|| haves.pop()) {
                Some(have) if unchanged(have, want) => {}
                Some(have) => {
                    if let Some(id) = &have.id {
                        out.update.push((id.clone(), to_input(cluster, want)));
                    }
                }
                None => {
                    // An identical record that isn't ours already exists: a
                    // human made it. Creating it would be swallowed as a
                    // duplicate, but the ownership tag would not, and a tag
                    // over a record we never created gets that record deleted
                    // the day it stops being desired. Leave it unclaimed.
                    if !tag_exists && has_identical_live(live, want) {
                        note_unclaimed(key);
                        continue;
                    }
                    out.create.push(to_input(cluster, want));
                    if !tag_exists {
                        out.create.push(tag_input(cluster, key, &want.fqdn));
                    }
                }
            }
        }

        // more live records under this key than desired: extras go.
        for extra in haves {
            if let Some(id) = &extra.id {
                out.delete.push(id.clone());
            }
        }
    }

    // keys with owned live records but nothing desired anymore: drop the
    // record and its ownership tag.
    for (key, haves) in live_by_key {
        for have in haves {
            if let Some(id) = &have.id {
                out.delete.push(id.clone());
            }
        }
        for tag in live.iter().filter(|r| is_owning_tag(r, cluster, &key)) {
            if let Some(id) = &tag.id {
                out.delete.push(id.clone());
            }
        }
    }

    delete_conflicting_live_records(&desired, live, &mut out);

    out
}

/// Whether a live non-tag record already holds exactly `want`'s name, type
/// and content (names are expected lowercased already).
fn has_identical_live(live: &[Record], want: &DesiredRecord) -> bool {
    live.iter().any(|r| {
        !is_tag_record(r) && r.kind == want.kind && r.name == want.fqdn && r.content == want.content
    })
}

/// Logs, once per key for the life of the process, that an existing
/// unowned record is being used as-is instead of adopted.
fn note_unclaimed(key: &str) {
    static SEEN: std::sync::Mutex<Option<HashSet<String>>> = std::sync::Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen
        .get_or_insert_with(HashSet::new)
        .insert(key.to_string())
    {
        tracing::info!(
            %key,
            "an identical dns record already exists and isn't owned by clouduler; using it as-is without claiming ownership"
        );
    }
}

/// Collapses a desired set down to something DNS can actually represent
/// before anything gets diffed against live records: a CNAME can't share a
/// name with any other record, and can only ever point at one target.
/// `records_for` computes each node's contribution independently (a node
/// can publish an IP and a domain at once, and several nodes' answers get
/// merged for a routing-mesh hostname in `all-nodes` mode), so nothing
/// upstream guarantees the merged set is valid — this is the one place
/// that enforces it. An address record (A/AAAA) always wins over a CNAME
/// for the same name, since an address is usable on its own and a CNAME
/// isn't once it has to coexist with something; among competing CNAMEs for
/// the same name, the first one in `desired`'s order survives (`desired`'s
/// callers build that order deterministically - sorted node names - so this
/// picks the same target every pass instead of flapping).
fn normalize_desired(desired: &[DesiredRecord]) -> Vec<DesiredRecord> {
    let lowered: Vec<DesiredRecord> = desired
        .iter()
        .cloned()
        .map(|mut d| {
            d.fqdn = d.fqdn.to_ascii_lowercase();
            d
        })
        .collect();
    let desired = lowered.as_slice();
    let mut has_address: HashSet<&str> = HashSet::new();
    for d in desired {
        if d.kind != RecordType::CNAME {
            has_address.insert(d.fqdn.as_str());
        }
    }

    let mut cname_seen: HashSet<&str> = HashSet::new();
    let mut kept = Vec::with_capacity(desired.len());
    for d in desired {
        if d.kind == RecordType::CNAME {
            if has_address.contains(d.fqdn.as_str()) {
                continue;
            }
            if !cname_seen.insert(d.fqdn.as_str()) {
                continue;
            }
        }
        kept.push(d.clone());
    }
    kept
}

/// Deletes any live record - owned or not - that Cloudflare's CNAME-exclusivity
/// rule would refuse to let a desired record coexist with: a CNAME can't
/// share a name with anything else, so a stale CNAME blocking a new A/AAAA
/// (or a stale A/AAAA blocking a new CNAME) is queued for deletion even
/// without an ownership tag. Records of the same kind as something desired
/// are left alone here - they're handled (or deliberately left for `create`
/// to fail on) by the ownership-scoped logic above.
fn delete_conflicting_live_records(desired: &[DesiredRecord], live: &[Record], out: &mut Plan) {
    let mut desired_kinds_by_name: HashMap<&str, HashSet<RecordType>> = HashMap::new();
    for d in desired {
        desired_kinds_by_name
            .entry(d.fqdn.as_str())
            .or_default()
            .insert(d.kind);
    }

    let mut already_queued: HashSet<String> = out.delete.iter().cloned().collect();
    for r in live {
        if is_tag_record(r) {
            continue;
        }
        let Some(kinds) = desired_kinds_by_name.get(r.name.as_str()) else {
            continue;
        };
        if kinds.contains(&r.kind) {
            continue;
        }
        // Only A/AAAA/CNAME can actually conflict; MX, TXT, CAA and the like
        // are a human's, never ours.
        if !matches!(r.kind, RecordType::A | RecordType::AAAA | RecordType::CNAME) {
            continue;
        }
        if r.kind != RecordType::CNAME && !kinds.contains(&RecordType::CNAME) {
            continue;
        }
        if let Some(id) = &r.id {
            if already_queued.insert(id.clone()) {
                out.delete.push(id.clone());
            }
        }
    }
}

/// Applies a plan against a zone. Cloudflare's "record already exists"
/// errors on create are swallowed: they mean a previous run (or a race with
/// another node) already got us to the desired state.
///
/// Deletes run first, before creates and updates: a stale owned record can
/// share a name with a newly desired record of a different type (e.g. a
/// hostname that used to resolve via a CNAME now wants a plain A record),
/// and Cloudflare refuses to create the new record while the old one still
/// occupies the name. Running deletes first clears the way instead of
/// failing on that collision every pass.
///
/// A failing record does not stop the rest of the plan from applying: one
/// hostname's bad state must not block every other hostname in the same
/// zone from converging. All failures are collected and returned together
/// so the caller can attribute each one to its own record.
pub async fn apply(cf: &Cloudflare, zone_id: &str, plan: &Plan) -> Result<()> {
    let mut errors = Vec::new();

    for id in &plan.delete {
        if let Err(e) = cf.delete_record(zone_id, id).await {
            errors.push(e);
        }
    }
    for input in &plan.create {
        match cf.create_record(zone_id, input).await {
            Ok(_) => {}
            Err(e) if e.is_benign_duplicate() => {
                tracing::debug!(name = %input.name, "record already exists, skipping");
            }
            Err(e) => errors.push(record_apply_error(input, e)),
        }
    }
    for (id, input) in &plan.update {
        match cf.update_record(zone_id, id, input).await {
            Ok(_) => {}
            Err(e) if e.is_benign_duplicate() => {
                tracing::debug!(name = %input.name, "record already exists, skipping");
            }
            Err(e) => errors.push(record_apply_error(input, e)),
        }
    }

    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.remove(0)),
        _ => Err(DnsError::Batch(errors)),
    }
}

fn record_apply_error(input: &RecordInput, source: DnsError) -> DnsError {
    DnsError::RecordApply {
        name: input.name.clone(),
        kind: input.kind,
        content: input.content.clone(),
        source: Box::new(source),
    }
}

fn desired_key(d: &DesiredRecord) -> String {
    format!("{:?}:{}", d.kind, d.fqdn)
}

fn is_tag_record(r: &Record) -> bool {
    r.kind == RecordType::TXT && r.name.starts_with("_clouduler.")
}

fn is_owning_tag(r: &Record, cluster: &str, key: &str) -> bool {
    is_tag_record(r)
        && parse_tag(&r.content)
            .map(|t| t.cluster == cluster && t.key == key)
            .unwrap_or(false)
}

/// Keys (type:name) of live records tagged as owned by `cluster`.
fn owned_keys(cluster: &str, live: &[Record]) -> HashSet<String> {
    live.iter()
        .filter(|r| is_tag_record(r))
        .filter_map(|r| parse_tag(&r.content))
        .filter(|t| t.cluster == cluster)
        .map(|t| t.key)
        .collect()
}

fn unchanged(have: &Record, want: &DesiredRecord) -> bool {
    have.content == want.content && have.ttl == want.ttl && have.proxied == want.proxied
}

fn to_input(cluster: &str, want: &DesiredRecord) -> RecordInput {
    RecordInput {
        name: want.fqdn.clone(),
        kind: want.kind,
        content: want.content.clone(),
        ttl: want.ttl,
        proxied: want.proxied,
        comment: Some(format!("managed by clouduler ({cluster})")),
    }
}

fn tag_input(cluster: &str, key: &str, fqdn: &str) -> RecordInput {
    RecordInput {
        name: tag_name(fqdn),
        kind: RecordType::TXT,
        // TXT content is sent as a quoted <character-string>, same as any
        // other TXT record; Cloudflare stores and returns it that way too.
        content: format!("\"{}\"", tag_content(cluster, key)),
        ttl: 1,
        proxied: false,
        comment: Some(format!("clouduler ownership tag ({cluster})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CLUSTER: &str = "home";

    fn desired(kind: RecordType, fqdn: &str, content: &str) -> DesiredRecord {
        DesiredRecord {
            fqdn: fqdn.to_string(),
            kind,
            content: content.to_string(),
            ttl: 1,
            proxied: false,
        }
    }

    fn live(id: &str, kind: RecordType, name: &str, content: &str) -> Record {
        Record {
            id: Some(id.to_string()),
            zone_id: Some("zone1".to_string()),
            name: name.to_string(),
            kind,
            content: content.to_string(),
            ttl: 1,
            proxied: false,
            comment: None,
        }
    }

    fn tag(id: &str, fqdn: &str, key: &str) -> Record {
        Record {
            id: Some(id.to_string()),
            zone_id: Some("zone1".to_string()),
            name: tag_name(fqdn),
            kind: RecordType::TXT,
            content: tag_content(CLUSTER, key),
            ttl: 1,
            proxied: false,
            comment: None,
        }
    }

    #[test]
    fn creates_missing_record_and_its_tag() {
        let want = vec![desired(RecordType::A, "svc.example.com", "1.2.3.4")];
        let p = plan(CLUSTER, &want, &[]);
        assert_eq!(p.create.len(), 2);
        assert!(p
            .create
            .iter()
            .any(|r| r.kind == RecordType::A && r.content == "1.2.3.4"));
        assert!(p.create.iter().any(|r| r.kind == RecordType::TXT));
        assert!(p.update.is_empty());
        assert!(p.delete.is_empty());
    }

    /// TXT content is a DNS <character-string>; sent unquoted, Cloudflare
    /// (and other providers) can split it on whitespace or otherwise mangle
    /// it. The ownership tag must go out quoted like any other TXT value.
    #[test]
    fn the_ownership_tag_is_sent_quoted() {
        let want = vec![desired(RecordType::A, "svc.example.com", "1.2.3.4")];
        let p = plan(CLUSTER, &want, &[]);
        let tag = p
            .create
            .iter()
            .find(|r| r.kind == RecordType::TXT)
            .expect("tag created");
        assert!(
            tag.content.starts_with('"') && tag.content.ends_with('"'),
            "{tag:?}"
        );
        assert_eq!(
            tag.content,
            format!("\"{}\"", tag_content(CLUSTER, "A:svc.example.com"))
        );
    }

    #[test]
    fn no_op_when_record_and_tag_already_match() {
        let want = vec![desired(RecordType::A, "svc.example.com", "1.2.3.4")];
        let key = "A:svc.example.com";
        let live_records = vec![
            live("r1", RecordType::A, "svc.example.com", "1.2.3.4"),
            tag("t1", "svc.example.com", key),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.is_empty(), "expected no-op, got {p:?}");
    }

    #[test]
    fn updates_when_content_changed() {
        let want = vec![desired(RecordType::A, "svc.example.com", "5.6.7.8")];
        let key = "A:svc.example.com";
        let live_records = vec![
            live("r1", RecordType::A, "svc.example.com", "1.2.3.4"),
            tag("t1", "svc.example.com", key),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert_eq!(p.update.len(), 1);
        assert_eq!(p.update[0].0, "r1");
        assert_eq!(p.update[0].1.content, "5.6.7.8");
        assert!(p.create.is_empty());
        assert!(p.delete.is_empty());
    }

    #[test]
    fn deletes_owned_record_and_tag_no_longer_desired() {
        let key = "A:old.example.com";
        let live_records = vec![
            live("r1", RecordType::A, "old.example.com", "1.2.3.4"),
            tag("t1", "old.example.com", key),
        ];
        let p = plan(CLUSTER, &[], &live_records);
        assert_eq!(p.delete.len(), 2);
        assert!(p.delete.contains(&"r1".to_string()));
        assert!(p.delete.contains(&"t1".to_string()));
    }

    /// Two nodes used to both get an A record for the dashboard host; now
    /// only the leader should. A previously-published follower record must
    /// be cleaned up, not left to collide with the leader's.
    #[test]
    fn deletes_a_stale_owned_record_when_fewer_are_desired_for_the_same_name() {
        let key = "A:cloud.example.com";
        let want = vec![desired(RecordType::A, "cloud.example.com", "10.0.0.1")];
        let live_records = vec![
            live("r1", RecordType::A, "cloud.example.com", "10.0.0.1"),
            live("r2", RecordType::A, "cloud.example.com", "10.0.0.2"),
            tag("t1", "cloud.example.com", key),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.create.is_empty(), "{p:?}");
        // One of the two live records is repointed to the desired content,
        // the other - now surplus - is deleted; either way exactly one A
        // record survives at 10.0.0.1 and none stay behind at 10.0.0.2.
        assert_eq!(p.delete.len(), 1, "{p:?}");
        assert!(
            p.update
                .iter()
                .all(|(_, input)| input.content == "10.0.0.1"),
            "{p:?}"
        );
    }

    /// A previous leader failover left one record already pointing at the
    /// new leader's address, plus stale records for two other nodes. The
    /// planner must keep the already-correct record as-is and only touch
    /// the stale ones, rather than picking an arbitrary `have` to "become"
    /// the desired content - that would try to repoint a stale record to
    /// content another live record already holds, which real providers
    /// reject as a duplicate.
    #[test]
    fn keeps_the_already_matching_record_instead_of_updating_an_arbitrary_one() {
        let key = "A:cloud.example.com";
        let want = vec![desired(RecordType::A, "cloud.example.com", "10.0.0.3")];
        let live_records = vec![
            live("r1", RecordType::A, "cloud.example.com", "10.0.0.1"),
            live("r2", RecordType::A, "cloud.example.com", "10.0.0.2"),
            live("r3", RecordType::A, "cloud.example.com", "10.0.0.3"),
            tag("t1", "cloud.example.com", key),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.update.is_empty(), "{p:?}");
        assert_eq!(p.delete.len(), 2, "{p:?}");
        assert!(p.delete.contains(&"r1".to_string()));
        assert!(p.delete.contains(&"r2".to_string()));
    }

    #[test]
    fn refuses_to_delete_unowned_record() {
        // A record exists but has no ownership tag: a human made it.
        let live_records = vec![live("r1", RecordType::A, "human.example.com", "9.9.9.9")];
        let p = plan(CLUSTER, &[], &live_records);
        assert!(p.delete.is_empty());
    }

    #[test]
    fn refuses_to_delete_record_owned_by_different_cluster() {
        let key = "A:svc.example.com";
        let live_records = vec![
            live("r1", RecordType::A, "svc.example.com", "1.2.3.4"),
            Record {
                id: Some("t1".to_string()),
                zone_id: Some("zone1".to_string()),
                name: tag_name("svc.example.com"),
                kind: RecordType::TXT,
                content: tag_content("other-cluster", key),
                ttl: 1,
                proxied: false,
                comment: None,
            },
        ];
        let p = plan(CLUSTER, &[], &live_records);
        assert!(p.delete.is_empty());
    }

    #[test]
    fn does_not_touch_unowned_same_type_record_colliding_with_desired() {
        // A human already has an A record at this name. Cloudflare allows
        // multiple A records at the same name, so this isn't a real
        // conflict; the reconciler must not update or delete it, only try
        // (and fail, elsewhere) to create the desired one alongside it.
        let want = vec![desired(RecordType::A, "human.example.com", "1.2.3.4")];
        let live_records = vec![live("r1", RecordType::A, "human.example.com", "9.9.9.9")];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.update.is_empty());
        assert!(p.delete.is_empty());
        assert_eq!(p.create.len(), 2); // record + tag, will hit "already exists" at apply time
    }

    /// Cloudflare forbids a CNAME from coexisting with any other record at
    /// the same name. A CNAME left over from a previous scheme (or made by
    /// hand) is invisible to the ownership-scoped matching above, so without
    /// this it would block the new A record forever with error 81053/81054.
    #[test]
    fn deletes_unowned_cname_blocking_a_new_a_record() {
        let want = vec![desired(RecordType::A, "auth.example.com", "1.2.3.4")];
        let live_records = vec![live(
            "r1",
            RecordType::CNAME,
            "auth.example.com",
            "osny.example.com",
        )];
        let p = plan(CLUSTER, &want, &live_records);
        assert_eq!(p.delete, vec!["r1".to_string()]);
        assert_eq!(p.create.len(), 2); // record + tag
    }

    /// Same rule, opposite direction: an unowned A record blocking a new
    /// CNAME must also be cleared.
    #[test]
    fn deletes_unowned_a_record_blocking_a_new_cname() {
        let want = vec![desired(
            RecordType::CNAME,
            "auth.example.com",
            "osny.example.com",
        )];
        let live_records = vec![live("r1", RecordType::A, "auth.example.com", "1.2.3.4")];
        let p = plan(CLUSTER, &want, &live_records);
        assert_eq!(p.delete, vec!["r1".to_string()]);
    }

    /// `records_for` can hand back both an A and a CNAME for the same node
    /// (one that publishes an IP and a domain at once) - not something
    /// Cloudflare can represent. The address record must win, and the
    /// invalid CNAME must never even reach `create`.
    #[test]
    fn an_address_record_beats_a_cname_desired_for_the_same_name() {
        let want = vec![
            desired(RecordType::A, "auth.example.com", "1.2.3.4"),
            desired(RecordType::CNAME, "auth.example.com", "soa.example.com"),
        ];
        let p = plan(CLUSTER, &want, &[]);
        assert_eq!(p.create.len(), 2, "{p:?}"); // the A record + its tag
        assert!(
            p.create.iter().all(|c| c.kind != RecordType::CNAME),
            "{p:?}"
        );
    }

    /// `all-nodes` DNS publish mode merges every node's answer for a shared
    /// hostname; if two nodes each publish only a domain (no IP), that's two
    /// different CNAME targets for one name, which DNS cannot represent.
    /// Only the first one (deterministic on `desired`'s order) may survive.
    #[test]
    fn only_the_first_of_several_conflicting_cnames_survives() {
        let want = vec![
            desired(
                RecordType::CNAME,
                "headscale.example.com",
                "soa.example.com",
            ),
            desired(
                RecordType::CNAME,
                "headscale.example.com",
                "osny.example.com",
            ),
        ];
        let p = plan(CLUSTER, &want, &[]);
        let cnames: Vec<_> = p
            .create
            .iter()
            .filter(|c| c.kind == RecordType::CNAME)
            .collect();
        assert_eq!(cnames.len(), 1, "{p:?}");
        assert_eq!(cnames[0].content, "soa.example.com");
    }

    /// A conflicting record must not be queued twice when it's already
    /// slated for deletion by the ownership-scoped "no longer desired"
    /// cleanup (e.g. an owned CNAME being replaced by a desired A record).
    #[test]
    fn does_not_double_queue_an_owned_conflicting_record_for_deletion() {
        let key = "CNAME:auth.example.com";
        let want = vec![desired(RecordType::A, "auth.example.com", "1.2.3.4")];
        let live_records = vec![
            live(
                "r1",
                RecordType::CNAME,
                "auth.example.com",
                "osny.example.com",
            ),
            tag("t1", "auth.example.com", key),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert_eq!(
            p.delete.iter().filter(|id| id.as_str() == "r1").count(),
            1,
            "{p:?}"
        );
    }

    /// A non-benign create failure (e.g. a name already claimed by an
    /// unowned record) must name the offending record in its message - the
    /// bare Cloudflare error alone doesn't say which record in the batch it
    /// was about.
    #[tokio::test]
    async fn apply_error_names_the_record_that_collided() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/zones/zone1/dns_records"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "success": false,
                "errors": [{ "code": 81053, "message": "An A, AAAA, or CNAME record with that host already exists." }],
                "messages": [],
                "result": null
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        let plan = Plan {
            create: vec![to_input(
                CLUSTER,
                &desired(RecordType::A, "human.example.com", "1.2.3.4"),
            )],
            update: vec![],
            delete: vec![],
        };
        let err = apply(&cf, "zone1", &plan).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("human.example.com"), "{msg}");
        assert!(msg.contains("1.2.3.4"), "{msg}");
        assert!(msg.contains("81053"), "{msg}");
    }

    #[test]
    fn leaves_human_mx_txt_and_caa_alone_next_to_a_desired_cname() {
        let want = vec![desired(
            RecordType::CNAME,
            "auth.example.com",
            "osny.example.com",
        )];
        let live_records = vec![
            live("r1", RecordType::TXT, "auth.example.com", "v=spf1 -all"),
            live("r2", RecordType::Other, "auth.example.com", "0 issue x"),
            live("r3", RecordType::A, "auth.example.com", "1.2.3.4"),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert_eq!(p.delete, vec!["r3".to_string()], "{p:?}");
    }

    #[test]
    fn does_not_adopt_an_identical_human_record() {
        let want = vec![desired(RecordType::A, "human.example.com", "9.9.9.9")];
        let live_records = vec![live("r1", RecordType::A, "human.example.com", "9.9.9.9")];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.is_empty(), "no tag, no create: {p:?}");
        // and once it's no longer desired, nothing is ever deleted
        let p = plan(CLUSTER, &[], &live_records);
        assert!(p.delete.is_empty());
    }

    #[test]
    fn mixed_case_hosts_match_their_lowercase_live_records() {
        let want = vec![desired(RecordType::A, "Svc.Example.com", "1.2.3.4")];
        let live_records = vec![
            live("r1", RecordType::A, "svc.example.com", "1.2.3.4"),
            tag("t1", "svc.example.com", "A:svc.example.com"),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn replan_after_apply_is_idempotent() {
        // Simulate: first plan wants a create, then imagine it was applied
        // (live now reflects desired + tag); replanning must be empty.
        let want = vec![
            desired(RecordType::A, "svc.example.com", "1.2.3.4"),
            desired(RecordType::AAAA, "svc.example.com", "2001:db8::1"),
        ];
        let live_records = vec![
            live("r1", RecordType::A, "svc.example.com", "1.2.3.4"),
            tag("t1", "svc.example.com", "A:svc.example.com"),
            live("r2", RecordType::AAAA, "svc.example.com", "2001:db8::1"),
            tag("t2", "svc.example.com", "AAAA:svc.example.com"),
        ];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.is_empty(), "expected no-op, got {p:?}");
    }
}
