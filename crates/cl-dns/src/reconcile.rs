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
/// records tagged as owned by `cluster`. Never touches a live record that
/// isn't clouduler's, even if it collides with something desired: those are
/// left for the create step to fail loudly on rather than overwritten.
pub fn plan(cluster: &str, desired: &[DesiredRecord], live: &[Record]) -> Plan {
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
    for d in desired {
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

    out
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
        assert!(p.create.iter().any(|r| r.kind == RecordType::A && r.content == "1.2.3.4"));
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
        let tag = p.create.iter().find(|r| r.kind == RecordType::TXT).expect("tag created");
        assert!(tag.content.starts_with('"') && tag.content.ends_with('"'), "{tag:?}");
        assert_eq!(tag.content, format!("\"{}\"", tag_content(CLUSTER, "A:svc.example.com")));
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
        assert!(p.update.iter().all(|(_, input)| input.content == "10.0.0.1"), "{p:?}");
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
    fn does_not_touch_unowned_record_colliding_with_desired() {
        // A human already has an A record at this name. Reconciler must not
        // update or delete it; it can only try (and fail, elsewhere) to create.
        let want = vec![desired(RecordType::A, "human.example.com", "1.2.3.4")];
        let live_records = vec![live("r1", RecordType::A, "human.example.com", "9.9.9.9")];
        let p = plan(CLUSTER, &want, &live_records);
        assert!(p.update.is_empty());
        assert!(p.delete.is_empty());
        assert_eq!(p.create.len(), 2); // record + tag, will hit "already exists" at apply time
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
            create: vec![to_input(CLUSTER, &desired(RecordType::A, "human.example.com", "1.2.3.4"))],
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
