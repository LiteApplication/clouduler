//! `/api/alerts`: the feed, event catalog, settings and test button. The engine that fills the feed is `crate::alerts`.

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
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
}

#[derive(Serialize)]
struct DeliveryView {
    destination: String,
    at: i64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
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
                    method: l.method.clone(),
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
                    message_id: d.message_id.clone(),
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
            .read(|st| {
                let s = settings::load(st);
                build_list(st.alerts.values(), s.display_pending_warnings)
            })
            .await,
    )
}

fn build_list<'a>(
    records: impl Iterator<Item = &'a AlertRecord>,
    include_pending: bool,
) -> AlertList {
    let mut active: Vec<&AlertRecord> = Vec::new();
    let mut recent: Vec<&AlertRecord> = Vec::new();
    for r in records {
        if r.state == AlertState::Active || (include_pending && r.state == AlertState::Pending) {
            active.push(r);
        } else if r.state != AlertState::Pending {
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

pub(crate) fn mask_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(u) if u.host_str().is_some() => {
            format!("{}://{}/…", u.scheme(), u.host_str().unwrap_or_default())
        }
        _ => "…".to_string(),
    }
}

/// The masked form a client read from us, which must mean "keep", never be stored.
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

fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

/// A Matrix username and password, used once to obtain an access token and then dropped. Never stored,
/// logged or echoed: no Debug, no Serialize, and it is never put in an error message.
#[derive(Deserialize)]
struct LoginIn {
    username: String,
    password: String,
}

#[derive(Deserialize, Default)]
struct SecretsIn {
    /// Matrix only, instead of `auth`: log in with these and store only the resulting token.
    #[serde(default)]
    login: Option<LoginIn>,
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
                Ok(None)
            } else {
                Ok(Some(Some(s)))
            }
        }
    }
}

/// The client for talking to a homeserver from this node: the system's trust roots, not the peer client.
fn login_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("clouduler/{}", cl_config::version()))
        .build()
        .unwrap_or_default()
}

/// Log in and return the token. The error is safe to show: no password, token or URL.
async fn login_token(
    http: &reqwest::Client,
    homeserver: &str,
    login: &LoginIn,
) -> Result<String, ApiError> {
    if login.username.trim().is_empty() || login.password.is_empty() {
        return Err(ApiError::BadRequest(
            "enter both the username and the password, or an access token".into(),
        ));
    }
    if !settings::http_url_ok(homeserver) {
        return Err(ApiError::BadRequest(
            "the homeserver URL must be an http or https URL".into(),
        ));
    }
    cl_alert::deliver::matrix_login(http, homeserver, &login.username, &login.password)
        .await
        .map_err(|e| {
            ApiError::BadRequest(format!(
                "could not log in to the homeserver: {}",
                crate::alerts::send::scrub_urls(&e.message)
            ))
        })
}

/// Turn each Matrix `login` into a token in `auth_in`, so only the token is ever sealed. A destination
/// that gives both a token and a login, or a login for a non-Matrix kind, is refused before any request.
async fn apply_logins(
    http: &reqwest::Client,
    st: &cl_raft::ClusterState,
    key: &cl_secret::ClusterKey,
    logins: &BTreeMap<String, LoginIn>,
    url_in: &BTreeMap<String, Option<String>>,
    auth_in: &mut BTreeMap<String, Option<String>>,
) -> Result<(), ApiError> {
    for (name, l) in logins {
        let homeserver = match url_in.get(name) {
            Some(Some(u)) => u.clone(),
            Some(None) => return Err(ApiError::BadRequest(format!("secrets.{name}.url: a homeserver URL is required"))),
            None => crate::alerts::send::open(st, key, &url_secret(name))
                .ok()
                .flatten()
                .ok_or_else(|| ApiError::BadRequest(format!("secrets.{name}.url: a homeserver URL is required")))?,
        };
        let token = login_token(http, &homeserver, l)
            .await
            .map_err(|e| match e {
                ApiError::BadRequest(m) => ApiError::BadRequest(format!("secrets.{name}.login: {m}")),
                o => o,
            })?;
        auth_in.insert(name.clone(), Some(token));
    }
    Ok(())
}

async fn put_settings(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PutSettings>,
) -> Result<Json<SettingsView>, ApiError> {
    let st = state.cluster.snapshot().await;
    let new = body.settings;

    let mut url_in: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut auth_in: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut login_in: BTreeMap<String, LoginIn> = BTreeMap::new();
    for (name, s) in body.secrets {
        let Some(dest) = new.destinations.iter().find(|d| d.name == name) else {
            return Err(ApiError::BadRequest(format!(
                "secrets.{name}: no destination with this name"
            )));
        };
        if let Some(l) = s.login {
            if !matches!(dest.kind, DestinationKind::Matrix(_)) {
                return Err(ApiError::BadRequest(format!(
                    "secrets.{name}.login: only a Matrix destination can log in"
                )));
            }
            if s.auth.is_some() {
                return Err(ApiError::BadRequest(format!(
                    "secrets.{name}: give an access token or a username and password, not both"
                )));
            }
            login_in.insert(name.clone(), l);
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
    let has_auth = |name: &str| match auth_in.get(name) {
        Some(v) => v.is_some(),
        None => login_in.contains_key(name) || st.secrets.contains_key(&auth_secret(name)),
    };
    let new_url = |name: &str| url_in.get(name).cloned().flatten();
    if let Err((field, msg)) = settings::validate(&new, &has_url, &has_auth, &new_url) {
        return Err(ApiError::BadRequest(format!("{field}: {msg}")));
    }
    let smtp_password = clean(&body.smtp_password)?;

    let key = state
        .key
        .read()
        .await
        .clone()
        .ok_or_else(|| ApiError::Conflict("this node holds no cluster key".into()))?;
    apply_logins(
        &login_client(),
        &st,
        &key,
        &login_in,
        &url_in,
        &mut auth_in,
    )
    .await?;
    drop(login_in);
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
    let old = settings::load(&st);
    for d in &old.destinations {
        let now = new.destinations.iter().find(|x| x.name == d.name);
        match now {
            None => {
                removes.insert(url_secret(&d.name));
                removes.insert(auth_secret(&d.name));
            }
            Some(n) => {
                if !matches!(
                    n.kind,
                    DestinationKind::Webhook | DestinationKind::Matrix(_)
                ) && !auth_in.contains_key(&n.name)
                {
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
    #[serde(default)]
    name: Option<String>,
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
    let st = state.cluster.snapshot().await;
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
            // A stored auth header only goes to the stored URL, so a test cannot send it elsewhere.
            if secrets.url.as_deref() != Some(url.as_str()) {
                secrets.auth = None;
            }
            secrets.url = Some(url);
        }
        if let Some(Some(auth)) = clean(&s.auth)? {
            secrets.auth = Some(auth);
        }
    }
    // Likewise the stored SMTP password: only to the saved server and user.
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
            DestinationKind::Webhook | DestinationKind::Matrix(_) => settings::http_url_ok(url),
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
    // A username and password log in for this test only: the token is not saved, and the device is logged out after.
    let mut test_session: Option<(reqwest::Client, String, String)> = None;
    if let Some(l) = body.secrets.as_ref().and_then(|s| s.login.as_ref()) {
        if !matches!(dest.kind, DestinationKind::Matrix(_)) {
            return Err(ApiError::BadRequest(
                "only a Matrix destination can log in".into(),
            ));
        }
        if body.secrets.as_ref().is_some_and(|s| s.auth.is_some()) {
            return Err(ApiError::BadRequest(
                "give an access token or a username and password, not both".into(),
            ));
        }
        let homeserver = secrets.url.clone().unwrap_or_default();
        let http = login_client();
        match login_token(&http, &homeserver, l).await {
            Ok(token) => {
                secrets.auth = Some(token.clone());
                test_session = Some((http, homeserver, token));
            }
            Err(ApiError::BadRequest(m)) => return Ok(failed(m)),
            Err(e) => return Err(e),
        }
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
    if let Some((http, homeserver, token)) = test_session {
        cl_alert::deliver::matrix_logout(&http, &homeserver, &token).await;
    }

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

    /// One canned reply per connection; returns the raw requests.
    async fn fake_homeserver(
        replies: Vec<(&'static str, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut n = 0;
                loop {
                    let r = s.read(&mut buf[n..]).await.unwrap();
                    n += r;
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if n >= i + 4 + len || r == 0 {
                            break;
                        }
                    } else if r == 0 {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        (url, h)
    }

    fn login(user: &str, pw: &str) -> BTreeMap<String, LoginIn> {
        BTreeMap::from([(
            "mx".to_string(),
            LoginIn {
                username: user.into(),
                password: pw.into(),
            },
        )])
    }

    fn test_key() -> cl_secret::ClusterKey {
        cl_secret::ClusterKey::generate()
    }

    #[test]
    fn a_login_parses_and_is_distinct_from_a_token() {
        let s: SecretsIn = serde_json::from_str(
            r#"{"url":"https://m.example.org","login":{"username":"@bot:example.org","password":"pw"}}"#,
        )
        .unwrap();
        assert_eq!(s.login.as_ref().unwrap().username, "@bot:example.org");
        assert_eq!(s.auth, None);
        let s: SecretsIn = serde_json::from_str("{}").unwrap();
        assert!(s.login.is_none());
    }

    #[tokio::test]
    async fn login_becomes_only_a_token_in_the_secret_edits() {
        let (url, h) = fake_homeserver(vec![(
            "200 OK",
            r#"{"access_token":"syt_the_token","device_id":"D"}"#.into(),
        )])
        .await;
        let url_in = BTreeMap::from([("mx".to_string(), Some(url))]);
        let mut auth_in = BTreeMap::new();
        apply_logins(
            &reqwest::Client::new(),
            &cl_raft::ClusterState::default(),
            &test_key(),
            &login("@bot:example.org", "hunter2-pw"),
            &url_in,
            &mut auth_in,
        )
        .await
        .unwrap();
        assert_eq!(auth_in["mx"].as_deref(), Some("syt_the_token"));
        // Nothing that would be written carries the username or password.
        let written = format!("{auth_in:?}{url_in:?}");
        assert!(!written.contains("hunter2-pw") && !written.contains("bot:example.org"));
        let req = h.await.unwrap().remove(0);
        assert!(req.starts_with("POST /_matrix/client/v3/login "));
        assert!(req.contains("\"m.login.password\"") && req.contains("hunter2-pw"));
    }

    #[tokio::test]
    async fn login_failures_are_validation_errors_without_secrets() {
        let (url, _h) = fake_homeserver(vec![(
            "403 Forbidden",
            r#"{"errcode":"M_FORBIDDEN","error":"bad hunter2-pw for bot"}"#.into(),
        )])
        .await;
        let url_in = BTreeMap::from([("mx".to_string(), Some(url.clone()))]);
        let mut auth_in = BTreeMap::new();
        let e = apply_logins(
            &reqwest::Client::new(),
            &cl_raft::ClusterState::default(),
            &test_key(),
            &login("bot", "hunter2-pw"),
            &url_in,
            &mut auth_in,
        )
        .await
        .unwrap_err();
        let ApiError::BadRequest(m) = e else {
            panic!("expected a 400")
        };
        assert!(m.contains("the homeserver rejected the username or password"), "{m}");
        assert!(!m.contains("hunter2-pw") && !m.contains(&url) && !m.contains("syt_"));
        assert!(auth_in.is_empty());

        // Unreachable homeserver: still a 400, still nothing leaked.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let url_in = BTreeMap::from([("mx".to_string(), Some(dead.clone()))]);
        let e = apply_logins(
            &reqwest::Client::new(),
            &cl_raft::ClusterState::default(),
            &test_key(),
            &login("bot", "hunter2-pw"),
            &url_in,
            &mut auth_in,
        )
        .await
        .unwrap_err();
        let ApiError::BadRequest(m) = e else {
            panic!("expected a 400")
        };
        assert!(m.contains("could not log in") && !m.contains("hunter2-pw") && !m.contains(&dead));
        assert!(auth_in.is_empty());

        // No homeserver URL given or saved.
        let e = apply_logins(
            &reqwest::Client::new(),
            &cl_raft::ClusterState::default(),
            &test_key(),
            &login("bot", "pw"),
            &BTreeMap::new(),
            &mut auth_in,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ApiError::BadRequest(m) if m.contains("homeserver URL is required")));
        // A blank password is refused before any request.
        let url_in = BTreeMap::from([("mx".to_string(), Some("http://127.0.0.1:9".to_string()))]);
        let e = apply_logins(
            &reqwest::Client::new(),
            &cl_raft::ClusterState::default(),
            &test_key(),
            &login("bot", ""),
            &url_in,
            &mut auth_in,
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ApiError::BadRequest(m) if m.contains("both the username and the password")));
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
        let l = build_list(all.iter(), false);
        let ids: Vec<_> = l.active.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "w-old"]);
        assert_eq!(
            (l.counts.critical, l.counts.warning, l.counts.info),
            (1, 1, 0)
        );
        assert_eq!(l.recent[0].id, "done-new");

        let l_pending = build_list(all.iter(), true);
        let ids_pending: Vec<_> = l_pending.active.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids_pending, vec!["c", "w-new", "w-old"]);
        assert_eq!(
            (
                l_pending.counts.critical,
                l_pending.counts.warning,
                l_pending.counts.info
            ),
            (1, 2, 0)
        );
    }
}
