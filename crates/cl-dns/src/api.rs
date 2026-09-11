//! Minimal Cloudflare API v4 client: auth, zone lookup, and DNS record CRUD.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{DnsError, Result};

const DEFAULT_BASE_URL: &str = "https://api.cloudflare.com/client/v4";
const MAX_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);

/// A DNS record type clouduler manages. Cloudflare supports many more (MX,
/// SRV, NS, CAA...), and a real zone has them: `Other` absorbs anything we
/// don't write ourselves, so listing every record in a zone that also has,
/// say, mail records doesn't fail to parse at all. `list_records`/`plan`
/// only ever act on the four variants we actually manage; an `Other` record
/// is never matched by anything we build and so is always left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecordType {
    A,
    AAAA,
    CNAME,
    TXT,
    #[serde(other)]
    Other,
}

/// A Cloudflare DNS record, as returned by or sent to the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub zone_id: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: RecordType,
    pub content: String,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    #[serde(default)]
    pub proxied: bool,
    /// Free text shown in the dashboard. We use it to tag ownership on
    /// record types (A/AAAA/CNAME) that can't carry a companion TXT well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

fn default_ttl() -> u32 {
    1 // Cloudflare's "automatic" ttl
}

/// Fields sent when creating or updating a record. Same shape as `Record`
/// minus the server assigned id/zone_id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordInput {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: RecordType,
    pub content: String,
    pub ttl: u32,
    pub proxied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Cloudflare wraps every response the same way. `result` is absent (null)
/// on a failed call, so it's optional.
#[derive(Debug, Deserialize)]
#[serde(bound = "T: Deserialize<'de>")]
struct ApiResponse<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    #[serde(default = "Option::default")]
    result: Option<T>,
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: u32,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ResultInfo {
    page: u32,
    total_pages: u32,
}

#[derive(Debug, Deserialize)]
struct Zone {
    id: String,
    name: String,
}

/// Cloudflare API v4 client, bearer token authenticated.
pub struct Cloudflare {
    http: reqwest::Client,
    base_url: String,
    token: String,
    zone_cache: Mutex<HashMap<String, String>>,
}

impl Cloudflare {
    /// Client pointed at the real Cloudflare API.
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url(token, DEFAULT_BASE_URL)
    }

    /// Client pointed at an arbitrary base url, for tests and the qemu harness.
    pub fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        Cloudflare {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            token: token.into(),
            zone_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Confirms the token is valid and has the expected permissions.
    pub async fn verify_token(&self) -> Result<()> {
        let url = format!("{}/user/tokens/verify", self.base_url);
        let _: serde_json::Value = self.request(reqwest::Method::GET, &url, None::<()>).await?;
        Ok(())
    }

    /// Resolves a zone name to its id, caching the result. Cloudflare has no
    /// "get zone by name" endpoint, so this lists zones filtered by name.
    pub async fn zone_id(&self, name: &str) -> Result<String> {
        if let Some(id) = self
            .zone_cache
            .lock()
            .expect("zone cache poisoned")
            .get(name)
        {
            return Ok(id.clone());
        }

        let url = format!("{}/zones?name={}", self.base_url, urlencode(name));
        let zones: Vec<Zone> = self.request(reqwest::Method::GET, &url, None::<()>).await?;
        let zone = zones
            .into_iter()
            .find(|z| z.name == name)
            .ok_or_else(|| DnsError::ZoneNotFound(name.to_string()))?;

        self.zone_cache
            .lock()
            .expect("zone cache poisoned")
            .insert(name.to_string(), zone.id.clone());
        Ok(zone.id)
    }

    /// Resolves a zone id back to its name. Used to find the zone apex to
    /// query authoritative nameservers directly for, when confirming a
    /// challenge record has actually propagated rather than just landed in
    /// Cloudflare's control plane.
    pub async fn zone_name(&self, zone_id: &str) -> Result<String> {
        let url = format!("{}/zones/{}", self.base_url, zone_id);
        let zone: Zone = self.request(reqwest::Method::GET, &url, None::<()>).await?;
        Ok(zone.name)
    }

    /// Lists records in a zone, optionally filtered by name and/or type.
    /// Follows pagination until every page has been fetched.
    pub async fn list_records(
        &self,
        zone_id: &str,
        name: Option<&str>,
        kind: Option<RecordType>,
    ) -> Result<Vec<Record>> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let mut url = format!(
                "{}/zones/{}/dns_records?page={}&per_page=100",
                self.base_url, zone_id, page
            );
            if let Some(name) = name {
                url.push_str(&format!("&name={}", urlencode(name)));
            }
            if let Some(kind) = kind {
                url.push_str(&format!("&type={}", record_type_str(kind)));
            }

            let (records, info) = self
                .request_paginated::<Vec<Record>>(reqwest::Method::GET, &url)
                .await?;
            all.extend(records);

            match info {
                Some(info) if info.page < info.total_pages => page += 1,
                _ => break,
            }
        }
        Ok(all)
    }

    pub async fn create_record(&self, zone_id: &str, record: &RecordInput) -> Result<Record> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, zone_id);
        self.request(reqwest::Method::POST, &url, Some(record))
            .await
    }

    pub async fn update_record(
        &self,
        zone_id: &str,
        record_id: &str,
        record: &RecordInput,
    ) -> Result<Record> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, zone_id, record_id
        );
        self.request(reqwest::Method::PUT, &url, Some(record)).await
    }

    pub async fn delete_record(&self, zone_id: &str, record_id: &str) -> Result<()> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, zone_id, record_id
        );
        let _: serde_json::Value = self
            .request(reqwest::Method::DELETE, &url, None::<()>)
            .await?;
        Ok(())
    }

    async fn request<B: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<B>,
    ) -> Result<T> {
        let (result, _) = self.request_with_body(method, url, body).await?;
        Ok(result)
    }

    async fn request_paginated<T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<(T, Option<ResultInfo>)> {
        self.request_with_body::<(), T>(method, url, None).await
    }

    /// Sends a request and unwraps Cloudflare's `ApiResponse` envelope in one
    /// place. Retries on 429 and 5xx with exponential backoff; anything else
    /// (auth, validation, 404) is not retried.
    async fn request_with_body<B: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<B>,
    ) -> Result<(T, Option<ResultInfo>)> {
        let mut attempt = 0u32;
        let mut backoff = INITIAL_BACKOFF;

        loop {
            attempt += 1;
            let mut req = self
                .http
                .request(method.clone(), url)
                .bearer_auth(&self.token);
            if let Some(body) = &body {
                req = req.json(body);
            }

            let resp = req.send().await.map_err(|source| DnsError::Transport {
                url: url.to_string(),
                source,
            })?;

            let status = resp.status();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < MAX_ATTEMPTS {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                continue;
            }

            let text = resp.text().await.map_err(|source| DnsError::Transport {
                url: url.to_string(),
                source,
            })?;

            let parsed: ApiResponse<T> =
                serde_json::from_str(&text).map_err(|_| DnsError::Api {
                    code: status.as_u16() as u32,
                    message: format!("unexpected response body: {text}"),
                })?;

            if !parsed.success {
                let err = parsed
                    .errors
                    .into_iter()
                    .next()
                    .map(|e| DnsError::Api {
                        code: e.code,
                        message: e.message,
                    })
                    .unwrap_or_else(|| DnsError::Api {
                        code: status.as_u16() as u32,
                        message: "cloudflare returned success=false with no error".to_string(),
                    });

                if status.as_u16() == 401 || status.as_u16() == 403 {
                    return Err(DnsError::Auth(err.to_string()));
                }
                return Err(err);
            }

            let result = parsed.result.ok_or_else(|| DnsError::Api {
                code: status.as_u16() as u32,
                message: "cloudflare returned success=true with no result".to_string(),
            })?;
            return Ok((result, parsed.result_info));
        }
    }
}

fn record_type_str(kind: RecordType) -> &'static str {
    match kind {
        RecordType::A => "A",
        RecordType::AAAA => "AAAA",
        RecordType::CNAME => "CNAME",
        RecordType::TXT => "TXT",
        // Never actually sent: nothing constructs a query or a record input
        // with this variant, since it exists only to absorb record types we
        // read but don't write. A real request for literally "OTHER" would
        // be a Cloudflare 400, which is a fine way to notice a bug here.
        RecordType::Other => "OTHER",
    }
}

fn urlencode(s: &str) -> String {
    // Query values here are hostnames and zone names: safe characters plus
    // dots and dashes. No need to pull in a url encoding crate for this.
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ok_envelope(result: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "success": true, "errors": [], "messages": [], "result": result })
    }

    fn err_envelope(code: u32, message: &str) -> serde_json::Value {
        serde_json::json!({
            "success": false,
            "errors": [{ "code": code, "message": message }],
            "messages": [],
            "result": null
        })
    }

    #[tokio::test]
    async fn verify_token_sends_bearer_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .and(header("Authorization", "Bearer secret-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(ok_envelope(serde_json::json!({ "status": "active" }))),
            )
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("secret-token", server.uri());
        cf.verify_token().await.unwrap();
    }

    #[tokio::test]
    async fn zone_id_resolves_and_caches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(
                serde_json::json!([
                    { "id": "zone-abc", "name": "example.com" }
                ]),
            )))
            .expect(1) // second zone_id call must hit the cache, not the server
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        assert_eq!(cf.zone_id("example.com").await.unwrap(), "zone-abc");
        assert_eq!(cf.zone_id("example.com").await.unwrap(), "zone-abc");
    }

    #[tokio::test]
    async fn zone_id_missing_zone_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(ok_envelope(serde_json::json!([]))),
            )
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        let err = cf.zone_id("nope.com").await.unwrap_err();
        assert!(matches!(err, DnsError::ZoneNotFound(z) if z == "nope.com"));
    }

    #[tokio::test]
    async fn list_records_follows_pagination() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones/zone1/dns_records"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "errors": [], "messages": [],
                "result": [{ "id": "r1", "zone_id": "zone1", "name": "a.example.com", "type": "A", "content": "1.1.1.1", "ttl": 1, "proxied": false }],
                "result_info": { "page": 1, "total_pages": 2 }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/zones/zone1/dns_records"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "errors": [], "messages": [],
                "result": [{ "id": "r2", "zone_id": "zone1", "name": "b.example.com", "type": "A", "content": "2.2.2.2", "ttl": 1, "proxied": false }],
                "result_info": { "page": 2, "total_pages": 2 }
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        let records = cf.list_records("zone1", None, None).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].id.as_deref(), Some("r1"));
        assert_eq!(records[1].id.as_deref(), Some("r2"));
    }

    /// A real zone has record types clouduler doesn't manage (MX, SRV, NS...).
    /// Listing it unfiltered must not fail to parse just because one of them
    /// showed up; it should come back as `RecordType::Other` and be ignored
    /// by anything that only looks for the four types clouduler writes.
    #[tokio::test]
    async fn unmanaged_record_types_do_not_break_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones/zone1/dns_records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true, "errors": [], "messages": [],
                "result": [
                    { "id": "r1", "zone_id": "zone1", "name": "example.com", "type": "MX", "content": "mail.example.com", "priority": 10, "ttl": 300, "proxied": false },
                    { "id": "r2", "zone_id": "zone1", "name": "a.example.com", "type": "A", "content": "1.1.1.1", "ttl": 1, "proxied": false }
                ],
                "result_info": { "page": 1, "total_pages": 1 }
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        let records = cf
            .list_records("zone1", None, None)
            .await
            .expect("parses despite the MX record");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, RecordType::Other);
        assert_eq!(records[1].kind, RecordType::A);
    }

    #[tokio::test]
    async fn create_record_returns_parsed_record() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/zones/zone1/dns_records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_envelope(
                serde_json::json!({
                    "id": "new1", "zone_id": "zone1", "name": "svc.example.com",
                    "type": "A", "content": "1.2.3.4", "ttl": 1, "proxied": false
                }),
            )))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        let input = RecordInput {
            name: "svc.example.com".to_string(),
            kind: RecordType::A,
            content: "1.2.3.4".to_string(),
            ttl: 1,
            proxied: false,
            comment: None,
        };
        let record = cf.create_record("zone1", &input).await.unwrap();
        assert_eq!(record.id.as_deref(), Some("new1"));
        assert_eq!(record.content, "1.2.3.4");
    }

    #[tokio::test]
    async fn api_error_carries_code_and_is_recognised_as_benign_duplicate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/zones/zone1/dns_records"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(err_envelope(81057, "Record already exists.")),
            )
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        let input = RecordInput {
            name: "svc.example.com".to_string(),
            kind: RecordType::A,
            content: "1.2.3.4".to_string(),
            ttl: 1,
            proxied: false,
            comment: None,
        };
        let err = cf.create_record("zone1", &input).await.unwrap_err();
        assert!(matches!(&err, DnsError::Api { code: 81057, .. }));
        assert!(err.is_benign_duplicate());
    }

    #[tokio::test]
    async fn auth_error_maps_to_auth_variant() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(err_envelope(9109, "Invalid access token")),
            )
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("bad-token", server.uri());
        let err = cf.verify_token().await.unwrap_err();
        assert!(matches!(err, DnsError::Auth(_)));
    }

    #[tokio::test]
    async fn retries_on_500_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user/tokens/verify"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(ok_envelope(serde_json::json!({ "status": "active" }))),
            )
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("tok", server.uri());
        cf.verify_token().await.unwrap();
    }

    #[test]
    fn api_response_deserialises_error_payload() {
        let raw = serde_json::to_string(&err_envelope(81058, "Duplicate record")).unwrap();
        let parsed: ApiResponse<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        assert!(!parsed.success);
        assert_eq!(parsed.errors[0].code, 81058);
        assert_eq!(parsed.errors[0].message, "Duplicate record");
        assert!(parsed.result.is_none());
    }

    #[test]
    fn api_response_deserialises_success_payload() {
        let raw = serde_json::to_string(&ok_envelope(serde_json::json!({ "id": "z1" }))).unwrap();
        let parsed: ApiResponse<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        assert!(parsed.success);
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.result.unwrap()["id"], "z1");
    }
}
