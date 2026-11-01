//! `/api/alerts`: the feed, the event catalog, the settings and the test
//! button. The engine that fills the feed is `crate::alerts`.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use cl_alert::catalog::{self, EVENTS};
use cl_alert::settings::{auth_secret, url_secret, SETTINGS_CONFIG, SMTP_PASSWORD_SECRET};
use cl_alert::{
    AlertSettings, Body, Destination, DestinationKind, EventOverride, Notification, Route,
    Severity, SmtpSettings,
};
use cl_config::AlertSeverity;
use cl_raft::alerts::{AlertRecord, AlertState, DestinationHealth};
use cl_raft::Request as ClusterRequest;
use cl_secret::Sealed;
use serde::{Deserialize, Deserializer, Serialize};

use crate::alerts::{route, send::Sender, settings, ui_url, unix_now};
use crate::auth::{Identity, RequireAdmin};
use crate::error::ApiError;
use crate::server::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/alerts", get(list))
        .route("/api/alerts/catalog", get(catalog_view))
        .route("/api/alerts/settings", get(get_settings).put(put_settings))
        .route("/api/alerts/test", post(test))
}

const RECENT_MAX: usize = 300;

#[derive(Serialize)]
struct LinkView {
    label: String,
    path: String,
}

#[derive(Serialize)]
struct DeliveryView {
    destination: String,
    at: i64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    attempts: u32,
}

#[derive(Serialize)]
struct AlertView {
    id: String,
    event: String,
    category: String,
    severity: AlertSeverity,
    state: AlertState,
    title: String,
    detail: String,
    subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    opened_at: i64,
    last_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_at: Option<i64>,
    count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    escalated_from: Option<AlertSeverity>,
    links: Vec<LinkView>,
    excerpt: Vec<String>,
    deliveries: Vec<DeliveryView>,
}

impl From<&AlertRecord> for AlertView {
    fn from(r: &AlertRecord) -> Self {
        AlertView {
            id: r.id.clone(),
            event: r.event.clone(),
            category: r.category.clone(),
            severity: r.severity,
            state: r.state,
            title: r.title.clone(),
            detail: r.detail.clone(),
            subject: r.subject.clone(),
            stack: r.stack.clone(),
            instance: r.instance.clone(),
            node: r.node.clone(),
            opened_at: r.opened_at,
            last_at: r.last_at,
            resolved_at: r.resolved_at,
            count: r.count,
            escalated_from: r.escalated_from,
            links: r
                .links
                .iter()
                .map(|l| LinkView {
                    label: l.label.clone(),
                    path: l.path.clone(),
                })
                .collect(),
            excerpt: r.excerpt.clone(),
            deliveries: r
                .deliveries
                .iter()
                .map(|d| DeliveryView {
                    destination: d.destination.clone(),
                    at: d.at,
                    ok: d.ok,
                    error: d.error.clone(),
                    attempts: d.attempts,
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Default)]
struct Counts {
    critical: usize,
    warning: usize,
    info: usize,
}

#[derive(Serialize)]
struct AlertList {
    active: Vec<AlertView>,
    recent: Vec<AlertView>,
    counts: Counts,
}

async fn list(_identity: Identity, State(state): State<AppState>) -> Json<AlertList> {
    Json(
        state
            .cluster
            .read(|st| build_list(st.alerts.values()))
            .await,
    )
}

fn build_list<'a>(records: impl Iterator<Item = &'a AlertRecord>) -> AlertList {
    let mut active: Vec<&AlertRecord> = Vec::new();
    let mut recent: Vec<&AlertRecord> = Vec::new();
    for r in records {
        if r.state.is_open() {
            active.push(r);
        } else {
            recent.push(r);
        }
    }
    active.sort_by(|a, b| (b.severity, b.last_at, &b.id).cmp(&(a.severity, a.last_at, &a.id)));
    recent.sort_by(|a, b| {
        let key = |r: &AlertRecord| r.resolved_at.unwrap_or(r.last_at);
        (key(b), &b.id).cmp(&(key(a), &a.id))
    });
    recent.truncate(RECENT_MAX);
    let mut counts = Counts::default();
    for r in &active {
        match r.severity {
            AlertSeverity::Critical => counts.critical += 1,
            AlertSeverity::Warning => counts.warning += 1,
            AlertSeverity::Info => counts.info += 1,
        }
    }
    AlertList {
        active: active.into_iter().map(AlertView::from).collect(),
        recent: recent.into_iter().map(AlertView::from).collect(),
        counts,
    }
}

#[derive(Serialize)]
struct CatalogEntry {
    #[serde(flatten)]
    def: &'static catalog::EventDef,
    r#override: Option<EventOverride>,
    effective_severity: Severity,
    routes: Vec<String>,
}

async fn catalog_view(
    _identity: Identity,
    State(state): State<AppState>,
) -> Json<Vec<CatalogEntry>> {
    let settings = state.cluster.read(settings::load).await;
    Json(catalog_entries(&settings))
}

fn catalog_entries(settings: &AlertSettings) -> Vec<CatalogEntry> {
    EVENTS
        .iter()
        .filter(|e| e.key != "test")
        .map(|def| {
            let o = settings.events.get(def.key).cloned();
            let effective = o.as_ref().and_then(|o| o.severity).unwrap_or(def.severity);
            let mut routes = Vec::new();
            match o.as_ref().and_then(|o| o.route.as_ref()) {
                Some(Route::Muted) => {}
                _ => {
                    routes.push("in-app".to_string());
                    let t =
                        route::targets(settings, def.key, def.category.as_str(), effective, None);
                    routes.extend(t.immediate);
                }
            }
            CatalogEntry {
                def,
                r#override: o,
                effective_severity: effective,
                routes,
            }
        })
        .collect()
}

#[derive(Serialize)]
struct SecretFlags {
    /// Scheme and host only, never the path or token. Null when unset.
    url: Option<String>,
    auth_set: bool,
}

/// `https://discord.com/api/webhooks/1/abc` -> `https://discord.com/…`.
/// Anything unparsable shows as a bare ellipsis.
pub(crate) fn mask_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(u) if u.host_str().is_some() => {
            format!("{}://{}/…", u.scheme(), u.host_str().unwrap_or_default())
        }
        _ => "…".to_string(),
    }
}

/// True when `candidate` is the masked form a client read from us (so it
/// must be treated as "keep", never stored).
fn is_masked_echo(candidate: &str) -> bool {
    candidate.ends_with('…')
}

#[derive(Serialize)]
struct HealthView {
    ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    consecutive_failures: u32,
}

#[derive(Serialize)]
struct SettingsView {
    settings: AlertSettings,
    secrets: BTreeMap<String, SecretFlags>,
    smtp_password: bool,
    health: BTreeMap<String, HealthView>,
}

async fn get_settings(_identity: Identity, State(state): State<AppState>) -> Json<SettingsView> {
    Json(settings_view(&state).await)
}

async fn settings_view(state: &AppState) -> SettingsView {
    let key = state.key.read().await.clone();
    state
        .cluster
        .read(|st| {
            let settings = settings::load(st);
            let secrets = settings
                .destinations
                .iter()
                .map(|d| {
                    (
                        d.name.clone(),
                        SecretFlags {
                            url: stored_url(st, key.as_deref(), &d.name),
                            auth_set: st.secrets.contains_key(&auth_secret(&d.name)),
                        },
                    )
                })
                .collect();
            let health = settings
                .destinations
                .iter()
                .map(|d| {
                    let h = st.alert_destinations.get(&d.name);
                    (
                        d.name.clone(),
                        HealthView {
                            ok: h.map(|h| h.ok),
                            at: h.map(|h| h.at),
                            error: h.and_then(|h| h.error.clone()),
                            consecutive_failures: h.map(|h| h.consecutive_failures).unwrap_or(0),
                        },
                    )
                })
                .collect();
            SettingsView {
                settings,
                secrets,
                smtp_password: st.secrets.contains_key(SMTP_PASSWORD_SECRET),
                health,
            }
        })
        .await
}

/// Masked form of the stored URL. A bare ellipsis when it cannot be opened,
/// so presence is still reported.
fn stored_url(
    st: &cl_raft::ClusterState,
    key: Option<&cl_secret::ClusterKey>,
    name: &str,
) -> Option<String> {
    st.secrets.get(&url_secret(name))?;
    let opened = key.and_then(|k| {
        crate::alerts::send::open(st, k, &url_secret(name))
            .ok()
            .flatten()
    });
    Some(opened.map(|u| mask_url(&u)).unwrap_or_else(|| "…".into()))
}

/// `absent` keeps, `null` deletes, a string sets.
fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Deserialize, Default)]
struct SecretsIn {
    #[serde(default, deserialize_with = "double_option")]
    url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    auth: Option<Option<String>>,
}

#[derive(Deserialize)]
struct PutSettings {
    settings: AlertSettings,
    #[serde(default)]
    secrets: BTreeMap<String, SecretsIn>,
    #[serde(default, deserialize_with = "double_option")]
    smtp_password: Option<Option<String>>,
}

fn clean(v: &Option<Option<String>>) -> Result<Option<Option<String>>, ApiError> {
    match v {
        None => Ok(None),
        Some(None) => Ok(Some(None)),
        Some(Some(s)) => {
            let s = s.trim().to_string();
            if s.is_empty() {
                Err(ApiError::BadRequest("a secret value is empty".into()))
            } else if is_masked_echo(&s) {
                // The masked form we returned, echoed back: keep.
                Ok(None)
            } else {
                Ok(Some(Some(s)))
            }
        }
    }
}

async fn put_settings(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PutSettings>,
) -> Result<Json<SettingsView>, ApiError> {
    let st = state.cluster.state().await;
    let new = body.settings;

    // What each destination's secrets will be once this is applied.
    let mut url_in: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut auth_in: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (name, s) in &body.secrets {
        if !new.destinations.iter().any(|d| &d.name == name) {
            return Err(ApiError::BadRequest(format!(
                "secrets.{name}: no destination with this name"
            )));
        }
        if let Some(v) = clean(&s.url)? {
            url_in.insert(name.clone(), v);
        }
        if let Some(v) = clean(&s.auth)? {
            auth_in.insert(name.clone(), v);
        }
    }
    let has_url = |name: &str| match url_in.get(name) {
        Some(v) => v.is_some(),
        None => st.secrets.contains_key(&url_secret(name)),
    };
    let new_url = |name: &str| url_in.get(name).cloned().flatten();
    if let Err((field, msg)) = settings::validate(&new, &has_url, &new_url) {
        return Err(ApiError::BadRequest(format!("{field}: {msg}")));
    }
    let smtp_password = clean(&body.smtp_password)?;

    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    let put_secret = |name: String, value: String| {
        let sealed = Sealed::seal(&key, &name, value.as_bytes()).map_err(ApiError::internal)?;
        Ok::<_, ApiError>(ClusterRequest::PutSecret {
            name,
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        })
    };
    let mut writes: Vec<ClusterRequest> = Vec::new();
    let mut removes: BTreeSet<String> = BTreeSet::new();
    for (name, v) in &url_in {
        match v {
            Some(v) => writes.push(put_secret(url_secret(name), v.clone())?),
            None => {
                removes.insert(url_secret(name));
            }
        }
    }
    for (name, v) in &auth_in {
        match v {
            Some(v) => writes.push(put_secret(auth_secret(name), v.clone())?),
            None => {
                removes.insert(auth_secret(name));
            }
        }
    }
    match &smtp_password {
        Some(Some(v)) => writes.push(put_secret(SMTP_PASSWORD_SECRET.into(), v.clone())?),
        Some(None) => {
            removes.insert(SMTP_PASSWORD_SECRET.into());
        }
        None => {}
    }
    // Secrets of destinations that are gone (or no longer need them).
    let old = settings::load(&st);
    for d in &old.destinations {
        let now = new.destinations.iter().find(|x| x.name == d.name);
        match now {
            None => {
                removes.insert(url_secret(&d.name));
                removes.insert(auth_secret(&d.name));
            }
            Some(n) => {
                if !matches!(n.kind, DestinationKind::Webhook) && !auth_in.contains_key(&n.name) {
                    removes.insert(auth_secret(&n.name));
                }
                if matches!(n.kind, DestinationKind::Email { .. }) {
                    removes.insert(url_secret(&n.name));
                }
            }
        }
    }
    if new.smtp.is_none() {
        removes.insert(SMTP_PASSWORD_SECRET.into());
    }

    let submit = |req: ClusterRequest| {
        let cluster = state.cluster.clone();
        async move {
            let resp = cluster.submit(req).await.map_err(ApiError::not_leader)?;
            match resp.error {
                Some(e) => Err(ApiError::BadRequest(e)),
                None => Ok(()),
            }
        }
    };
    for w in writes {
        submit(w).await?;
    }
    let expect_revision = st
        .configs
        .get(SETTINGS_CONFIG)
        .map(|c| c.revision)
        .unwrap_or(0);
    submit(ClusterRequest::PutConfigCas {
        name: SETTINGS_CONFIG.into(),
        content: settings::encode(&new),
        owner: None,
        expect_revision,
    })
    .await
    .map_err(|e| match e {
        ApiError::BadRequest(m) if m.contains("changed") => ApiError::Conflict(
            "the alert settings were changed by someone else, reload and try again".into(),
        ),
        other => other,
    })?;
    for name in removes {
        let being_set = url_in
            .iter()
            .any(|(d, v)| v.is_some() && url_secret(d) == name)
            || auth_in
                .iter()
                .any(|(d, v)| v.is_some() && auth_secret(d) == name)
            || (name == SMTP_PASSWORD_SECRET && matches!(smtp_password, Some(Some(_))));
        if st.secrets.contains_key(&name) && !being_set {
            submit(ClusterRequest::RemoveSecret { name }).await?;
        }
    }
    for name in st.alert_destinations.keys() {
        if !new.destinations.iter().any(|d| &d.name == name) {
            submit(ClusterRequest::RemoveDestinationHealth { name: name.clone() }).await?;
        }
    }
    Ok(Json(settings_view(&state).await))
}

#[derive(Deserialize)]
struct TestBody {
    /// A saved destination.
    #[serde(default)]
    name: Option<String>,
    /// An unsaved one, from the add sheet.
    #[serde(default)]
    destination: Option<Destination>,
    #[serde(default)]
    secrets: Option<SecretsIn>,
    #[serde(default)]
    smtp: Option<SmtpSettings>,
    #[serde(default, deserialize_with = "double_option")]
    smtp_password: Option<Option<String>>,
}

#[derive(Serialize)]
struct TestResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn failed(msg: impl Into<String>) -> Json<TestResult> {
    Json(TestResult {
        ok: false,
        error: Some(msg.into()),
    })
}

async fn test(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<TestBody>,
) -> Result<Json<TestResult>, ApiError> {
    let st = state.cluster.state().await;
    let saved = settings::load(&st);
    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;

    let (dest, smtp, mut secrets, saved_name) = match (&body.name, &body.destination) {
        (Some(name), None) => {
            let Some(dest) = saved.destinations.iter().find(|d| &d.name == name) else {
                return Err(ApiError::NotFound(format!("no destination named {name}")));
            };
            let secrets = match crate::alerts::send::unseal_for(&st, &key, dest, &saved) {
                Ok(s) => s,
                Err(e) => return Ok(failed(e)),
            };
            (
                dest.clone(),
                saved.smtp.clone(),
                secrets,
                Some(name.clone()),
            )
        }
        (None, Some(dest)) => {
            // Blank fields fall back to what is stored under the same name.
            let stored =
                crate::alerts::send::unseal_for(&st, &key, dest, &saved).unwrap_or_default();
            (
                dest.clone(),
                body.smtp.clone().or(saved.smtp.clone()),
                stored,
                None,
            )
        }
        _ => {
            return Err(ApiError::BadRequest(
                "give either name or destination".into(),
            ))
        }
    };
    if let Some(s) = &body.secrets {
        if let Some(Some(url)) = clean(&s.url)? {
            // A stored auth header only ever goes to the stored URL, so a
            // test cannot send it somewhere else.
            if secrets.url.as_deref() != Some(url.as_str()) {
                secrets.auth = None;
            }
            secrets.url = Some(url);
        }
        if let Some(Some(auth)) = clean(&s.auth)? {
            secrets.auth = Some(auth);
        }
    }
    // Same for the stored SMTP password: only to the saved server and user.
    let same_server = |a: &SmtpSettings, b: &SmtpSettings| {
        a.host == b.host && a.port == b.port && a.username == b.username
    };
    if let (Some(given), saved_smtp) = (&body.smtp, &saved.smtp) {
        if !saved_smtp.as_ref().is_some_and(|s| same_server(s, given)) {
            secrets.smtp_password = None;
        }
    }
    if let Some(Some(p)) = clean(&body.smtp_password)? {
        secrets.smtp_password = Some(p);
    }
    if let Some(url) = &secrets.url {
        let ok = match dest.kind {
            DestinationKind::Discord(_) => settings::discord_url_ok(url),
            DestinationKind::Webhook => settings::http_url_ok(url),
            DestinationKind::Email { .. } => true,
        };
        if !ok {
            return Ok(failed("the URL is not valid for this kind of destination"));
        }
    } else if !matches!(dest.kind, DestinationKind::Email { .. }) {
        return Ok(failed("the destination has no URL"));
    }
    if matches!(dest.kind, DestinationKind::Email { .. }) && smtp.is_none() {
        return Ok(failed("set up the email server first"));
    }

    let ui = ui_url(&state, &saved).await;
    let n = Notification {
        cluster: state.cluster_config.read().await.cluster.name.clone(),
        ui_url: ui,
        body: Body::Test,
    };
    let sender = Sender::new(state.cluster.clone(), state.key.clone());
    let result = sender.send_once(&dest, &secrets, smtp.as_ref(), &n).await;
    drop(secrets);

    let (ok, error) = match result {
        Ok(()) => (true, None),
        Err(e) => (false, Some(e.message)),
    };
    if let Some(name) = saved_name {
        let prev = st
            .alert_destinations
            .get(&name)
            .map(|h| h.consecutive_failures)
            .unwrap_or(0);
        let _ = state
            .cluster
            .submit(ClusterRequest::SetDestinationHealth {
                name,
                health: DestinationHealth {
                    ok,
                    at: unix_now(),
                    error: error.clone(),
                    consecutive_failures: if ok { 0 } else { prev + 1 },
                },
            })
            .await;
    }
    Ok(Json(TestResult { ok, error }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_alert::DestinationKind;

    #[test]
    fn masking_keeps_only_scheme_and_host() {
        assert_eq!(
            mask_url("https://discord.com/api/webhooks/123/tok?x=1"),
            "https://discord.com/…"
        );
        assert_eq!(
            mask_url("http://user:pw@host.example:8080/p/q"),
            "http://host.example/…"
        );
        assert_eq!(mask_url("garbage"), "…");
        for m in [
            mask_url("https://discord.com/api/webhooks/1/SECRET"),
            mask_url("x"),
        ] {
            assert!(!m.contains("SECRET") && !m.contains("api"));
        }
    }

    #[test]
    fn masked_echo_is_keep() {
        let masked = mask_url("https://discord.com/api/webhooks/1/abc");
        assert_eq!(clean(&Some(Some(masked))).unwrap(), None);
        assert_eq!(
            clean(&Some(Some("https://x.example/h".into()))).unwrap(),
            Some(Some("https://x.example/h".into()))
        );
        assert_eq!(clean(&Some(None)).unwrap(), Some(None));
    }

    #[test]
    fn secrets_distinguish_absent_null_and_value() {
        let s: SecretsIn = serde_json::from_str(r#"{"url": null, "auth": "x"}"#).unwrap();
        assert_eq!(s.url, Some(None));
        assert_eq!(s.auth, Some(Some("x".into())));
        let s: SecretsIn = serde_json::from_str("{}").unwrap();
        assert_eq!(s.url, None);
    }

    #[test]
    fn catalog_routes_follow_the_settings() {
        let mut s = AlertSettings::default();
        s.destinations.push(Destination {
            name: "main".into(),
            enabled: true,
            kind: DestinationKind::Webhook,
            severities: [Severity::Critical].into(),
            digest: false,
            categories: vec![],
            stacks: vec![],
        });
        let find = |s: &AlertSettings, k: &str| {
            catalog_entries(s)
                .into_iter()
                .find(|e| e.def.key == k)
                .unwrap()
        };
        assert_eq!(
            find(&s, "instance.crash_loop").routes,
            vec!["in-app", "main"]
        );
        assert_eq!(find(&s, "instance.unhealthy").routes, vec!["in-app"]);
        s.events.insert(
            "instance.unhealthy".into(),
            EventOverride {
                severity: Some(Severity::Critical),
                route: None,
            },
        );
        let e = find(&s, "instance.unhealthy");
        assert_eq!(e.effective_severity, Severity::Critical);
        assert_eq!(e.routes, vec!["in-app", "main"]);
        assert!(catalog_entries(&s).iter().all(|e| e.def.key != "test"));
    }

    #[test]
    fn feed_orders_worst_first_and_counts_open_alerts() {
        use cl_raft::alerts::AlertState::*;
        let rec = |id: &str, sev: AlertSeverity, state, last: i64| AlertRecord {
            id: id.into(),
            event: "node.disk_low".into(),
            dedupe: id.into(),
            category: "nodes".into(),
            severity: sev,
            state,
            title: id.into(),
            detail: String::new(),
            subject: String::new(),
            stack: None,
            instance: None,
            node: None,
            opened_at: last,
            last_at: last,
            resolved_at: None,
            count: 1,
            escalated_from: None,
            sent_severity: None,
            resolved_sent: false,
            digested: false,
            links: vec![],
            excerpt: vec![],
            deliveries: vec![],
        };
        let all = [
            rec("w-old", AlertSeverity::Warning, Active, 1),
            rec("c", AlertSeverity::Critical, Active, 0),
            rec("w-new", AlertSeverity::Warning, Pending, 5),
            rec("done-old", AlertSeverity::Warning, Resolved, 2),
            rec("done-new", AlertSeverity::Info, Event, 9),
        ];
        let l = build_list(all.iter());
        let ids: Vec<_> = l.active.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "w-new", "w-old"]);
        assert_eq!(
            (l.counts.critical, l.counts.warning, l.counts.info),
            (1, 2, 0)
        );
        assert_eq!(l.recent[0].id, "done-new");
    }
}
