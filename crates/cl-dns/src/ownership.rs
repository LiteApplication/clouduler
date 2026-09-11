//! Ownership tagging so clouduler never touches records a human created.
//!
//! Every record clouduler manages gets a companion TXT record at
//! `_clouduler.<fqdn>` whose content names the cluster and the record it
//! covers. Where the API supports it we also stamp a `comment` on the
//! record itself, which is a nicer signal in the dashboard but not load
//! bearing (the TXT record is the source of truth).

use crate::api::Record;

const TAG_PREFIX: &str = "_clouduler";

/// Name of the companion TXT record that tags ownership of `fqdn`.
pub fn tag_name(fqdn: &str) -> String {
    format!("{TAG_PREFIX}.{fqdn}")
}

/// Content of the ownership tag TXT record: cluster name and the key
/// (record type + fqdn) it covers, so a stale tag can be told apart from
/// a current one.
pub fn tag_content(cluster: &str, key: &str) -> String {
    format!("cluster={cluster};key={key}")
}

/// A parsed ownership tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub cluster: String,
    pub key: String,
}

/// Parses a tag TXT record's content back into cluster and key. Returns
/// `None` for anything that isn't in our format, e.g. a human's unrelated
/// TXT record that happens to live under the same name.
///
/// Strips one layer of surrounding double quotes first: TXT content is sent
/// quoted (see `tag_content`) and Cloudflare echoes it back the same way, but
/// this stays tolerant of an unquoted value too so a tag written before
/// quoting was added still parses.
pub fn parse_tag(content: &str) -> Option<Tag> {
    let content = content
        .strip_prefix('"')
        .and_then(|c| c.strip_suffix('"'))
        .unwrap_or(content);
    let mut cluster = None;
    let mut key = None;
    for field in content.split(';') {
        let (k, v) = field.split_once('=')?;
        match k {
            "cluster" => cluster = Some(v.to_string()),
            "key" => key = Some(v.to_string()),
            _ => {}
        }
    }
    Some(Tag {
        cluster: cluster?,
        key: key?,
    })
}

/// The key a tag covers for a given record: type and fqdn, since the same
/// name can carry multiple record types (A and AAAA on one host, say).
pub fn record_key(record: &Record) -> String {
    format!("{:?}:{}", record.kind, record.name)
}

/// Whether `tag_record` (a `_clouduler.<fqdn>` TXT record) marks `record` as
/// owned by `cluster`.
pub fn is_owned_by(record: &Record, tag_record: &Record, cluster: &str) -> bool {
    match parse_tag(&tag_record.content) {
        Some(tag) => tag.cluster == cluster && tag.key == record_key(record),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RecordType;

    fn rec(kind: RecordType, name: &str, content: &str) -> Record {
        Record {
            id: Some("id1".to_string()),
            zone_id: Some("zone1".to_string()),
            name: name.to_string(),
            kind,
            content: content.to_string(),
            ttl: 1,
            proxied: false,
            comment: None,
        }
    }

    #[test]
    fn tag_name_prefixes_fqdn() {
        assert_eq!(tag_name("svc.example.com"), "_clouduler.svc.example.com");
    }

    #[test]
    fn round_trips_through_content() {
        let key = record_key(&rec(RecordType::A, "svc.example.com", "1.2.3.4"));
        let content = tag_content("home", &key);
        let tag = parse_tag(&content).unwrap();
        assert_eq!(tag.cluster, "home");
        assert_eq!(tag.key, key);
    }

    #[test]
    fn parses_content_wrapped_in_quotes() {
        // What we actually send (see `reconcile::tag_input`) and what a
        // provider echoes back for a TXT record.
        let key = record_key(&rec(RecordType::A, "svc.example.com", "1.2.3.4"));
        let quoted = format!("\"{}\"", tag_content("home", &key));
        let tag = parse_tag(&quoted).expect("quoted content still parses");
        assert_eq!(tag.cluster, "home");
        assert_eq!(tag.key, key);
    }

    #[test]
    fn garbage_content_does_not_parse() {
        assert!(parse_tag("just some human text").is_none());
        assert!(parse_tag("cluster=home").is_none()); // missing key
        assert!(parse_tag("key=A:svc.example.com").is_none()); // missing cluster
    }

    #[test]
    fn is_owned_by_matches_cluster_and_key() {
        let record = rec(RecordType::A, "svc.example.com", "1.2.3.4");
        let key = record_key(&record);
        let tag_record = rec(
            RecordType::TXT,
            &tag_name("svc.example.com"),
            &tag_content("home", &key),
        );
        assert!(is_owned_by(&record, &tag_record, "home"));
        assert!(!is_owned_by(&record, &tag_record, "other-cluster"));
    }

    #[test]
    fn is_owned_by_rejects_mismatched_key() {
        let record = rec(RecordType::A, "svc.example.com", "1.2.3.4");
        let other_key = record_key(&rec(RecordType::AAAA, "svc.example.com", "::1"));
        let tag_record = rec(
            RecordType::TXT,
            &tag_name("svc.example.com"),
            &tag_content("home", &other_key),
        );
        assert!(!is_owned_by(&record, &tag_record, "home"));
    }

    #[test]
    fn is_owned_by_rejects_unrelated_txt() {
        let record = rec(RecordType::A, "svc.example.com", "1.2.3.4");
        let tag_record = rec(
            RecordType::TXT,
            &tag_name("svc.example.com"),
            "human wrote this",
        );
        assert!(!is_owned_by(&record, &tag_record, "home"));
    }
}
