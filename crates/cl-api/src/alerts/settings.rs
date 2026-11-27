use std::collections::BTreeSet;

use cl_alert::catalog;
use cl_alert::settings::SETTINGS_CONFIG;
use cl_alert::{AlertSettings, DestinationKind, Route};
use cl_raft::ClusterState;

pub fn load(state: &ClusterState) -> AlertSettings {
    let Some(entry) = state.configs.get(SETTINGS_CONFIG) else {
        return AlertSettings::default();
    };
    let Ok(raw) = entry.decode() else {
        return AlertSettings::default();
    };
    serde_json::from_slice(&raw).unwrap_or_default()
}

pub fn encode(settings: &AlertSettings) -> String {
    crate::encode_json_config(settings)
}

/// What only the catalog can tell: a `mute` or `severity` key that names no event.
pub fn stack_key_errors(stack: &cl_config::StackFile) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for service in stack.services.keys() {
        let a = stack.effective_alerts(service);
        let keys = a
            .mute
            .iter()
            .map(|k| ("mute", k))
            .chain(a.severity.keys().map(|k| ("severity", k)));
        for (field, key) in keys {
            let known = match key.strip_suffix(".*") {
                Some(prefix) => catalog::EVENTS.iter().any(|e| {
                    e.key != "test" && cl_config::mute_matches(key, e.key) && !prefix.is_empty()
                }),
                None => catalog::find(key).is_some_and(|e| e.key != "test"),
            };
            if !known && seen.insert((field, key.clone())) {
                out.push(format!(
                    "x-clouduler.alerts.{field}: {key} is not an alert event (see docs/alerts.md)"
                ));
            }
        }
    }
    out
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn discord_url_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    if !matches!(
        host,
        "discord.com"
            | "discordapp.com"
            | "ptb.discord.com"
            | "canary.discord.com"
            | "ptb.discordapp.com"
            | "canary.discordapp.com"
    ) {
        return false;
    }
    let mut parts = path.split('/');
    if parts.next() != Some("api") {
        return false;
    }
    let mut next = parts.next();
    if next.is_some_and(|p| {
        p.len() > 1 && p.starts_with('v') && p[1..].bytes().all(|b| b.is_ascii_digit())
    }) {
        next = parts.next();
    }
    if next != Some("webhooks") {
        return false;
    }
    let id = parts.next().unwrap_or("");
    let token = parts.next().unwrap_or("");
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && !token.is_empty()
}

pub fn http_url_ok(url: &str) -> bool {
    url::Url::parse(url)
        .map(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
        .unwrap_or(false)
}

/// A room ID (`!opaque:server`, or just `!opaque` on room versions whose ID is
/// a hash with no server part) or an alias (`#name:server`).
pub fn matrix_room_ok(room: &str) -> bool {
    if room.len() > 255 || room.contains(char::is_whitespace) {
        return false;
    }
    if let Some(id) = room.strip_prefix('!') {
        return match id.split_once(':') {
            Some((local, server)) => !local.is_empty() && !server.is_empty(),
            None => !id.is_empty(),
        };
    }
    match room.strip_prefix('#').and_then(|a| a.split_once(':')) {
        Some((local, server)) => !local.is_empty() && !server.is_empty(),
        None => false,
    }
}

pub fn mention_ok(m: &str) -> bool {
    if m == "@here" || m == "@everyone" {
        return true;
    }
    let Some(inner) = m.strip_prefix("<@").and_then(|s| s.strip_suffix('>')) else {
        return false;
    };
    let digits = inner
        .strip_prefix('&')
        .or_else(|| inner.strip_prefix('!'))
        .unwrap_or(inner);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

pub fn email_ok(value: &str, allow_name: bool) -> bool {
    let value = value.trim();
    let addr = if allow_name {
        match (value.rfind('<'), value.ends_with('>')) {
            (Some(i), true) => &value[i + 1..value.len() - 1],
            _ => value,
        }
    } else {
        value
    };
    let Some((local, domain)) = addr.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !addr.contains(char::is_whitespace)
        && !addr.contains(['<', '>', ',', ';', '"'])
        && domain.matches('@').count() == 0
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

fn hhmm_ok(s: &str) -> bool {
    let Some((h, m)) = s.split_once(':') else {
        return false;
    };
    h.len() == 2
        && m.len() == 2
        && h.parse::<u8>().is_ok_and(|h| h < 24)
        && m.parse::<u8>().is_ok_and(|m| m < 60)
}

/// `has_url(name)` / `has_auth(name)` say whether a destination will have a URL / auth secret.
pub fn validate(
    settings: &AlertSettings,
    has_url: &dyn Fn(&str) -> bool,
    has_auth: &dyn Fn(&str) -> bool,
    url_to_check: &dyn Fn(&str) -> Option<String>,
) -> Result<(), (String, String)> {
    let err = |field: String, msg: &str| Err((field, msg.to_string()));
    let mut seen = BTreeSet::new();
    for (i, d) in settings.destinations.iter().enumerate() {
        let at = format!("destinations[{i}]");
        if !name_ok(&d.name) {
            return err(
                format!("{at}.name"),
                "use 1 to 40 lowercase letters, digits or dashes",
            );
        }
        if !seen.insert(d.name.as_str()) {
            return err(format!("{at}.name"), "another destination has this name");
        }
        match &d.kind {
            DestinationKind::Webhook | DestinationKind::Discord(_) => {
                if !has_url(&d.name) {
                    return err(format!("{at}.url"), "a URL is required");
                }
                if let Some(url) = url_to_check(&d.name) {
                    let discord = matches!(d.kind, DestinationKind::Discord(_));
                    if discord && !discord_url_ok(&url) {
                        return err(
                            format!("{at}.url"),
                            "not a Discord webhook URL (https://discord.com/api/webhooks/...)",
                        );
                    }
                    if !discord && !http_url_ok(&url) {
                        return err(format!("{at}.url"), "must be an http or https URL");
                    }
                }
                if let DestinationKind::Discord(o) = &d.kind {
                    if let Some(m) = &o.mention {
                        if !mention_ok(m) {
                            return err(
                                format!("{at}.mention"),
                                "use <@&roleId>, <@userId>, @here or @everyone",
                            );
                        }
                    }
                    if o.username
                        .as_deref()
                        .is_some_and(|u| u.chars().count() > 80)
                    {
                        return err(format!("{at}.username"), "at most 80 characters");
                    }
                    if let Some(u) = o
                        .avatar_url
                        .as_deref()
                        .map(str::trim)
                        .filter(|u| !u.is_empty())
                    {
                        if !http_url_ok(u) || u.len() > 2048 {
                            return err(
                                format!("{at}.avatar_url"),
                                "must be an http or https URL of at most 2048 characters",
                            );
                        }
                    }
                }
            }
            DestinationKind::Matrix(o) => {
                if !has_url(&d.name) {
                    return err(format!("{at}.url"), "a homeserver URL is required");
                }
                if !has_auth(&d.name) {
                    return err(format!("{at}.auth"), "an access token is required");
                }
                if let Some(url) = url_to_check(&d.name) {
                    if !http_url_ok(&url) {
                        return err(
                            format!("{at}.url"),
                            "must be an http or https URL (https://matrix.example.org)",
                        );
                    }
                }
                if !matrix_room_ok(o.room.trim()) {
                    return err(
                        format!("{at}.room"),
                        "use a room ID (!abc:example.org, or !abc on newer room versions) or an alias (#ops:example.org)",
                    );
                }
            }
            DestinationKind::Email { to } => {
                if settings.smtp.is_none() {
                    return err(
                        format!("{at}.to"),
                        "set up the email server before adding an email destination",
                    );
                }
                if to.is_empty() {
                    return err(format!("{at}.to"), "add at least one recipient");
                }
                for (j, a) in to.iter().enumerate() {
                    if !email_ok(a, false) {
                        return err(format!("{at}.to[{j}]"), "not an email address");
                    }
                }
            }
        }
        for (j, c) in d.categories.iter().enumerate() {
            if !catalog::EVENTS.iter().any(|e| e.category.as_str() == c) {
                return err(format!("{at}.categories[{j}]"), "unknown category");
            }
        }
    }
    if let Some(smtp) = &settings.smtp {
        if smtp.host.trim().is_empty() {
            return err("smtp.host".into(), "required");
        }
        if smtp.port == 0 {
            return err("smtp.port".into(), "required");
        }
        if !email_ok(&smtp.from, true) {
            return err("smtp.from".into(), "not an email address");
        }
    }
    if !hhmm_ok(&settings.digest_at) {
        return err("digest_at".into(), "use HH:MM, for example 09:00");
    }
    if let Some(tz) = &settings.timezone {
        if jiff::tz::TimeZone::get(tz).is_err() {
            return err("timezone".into(), "not an IANA time zone like Europe/Paris");
        }
    }
    if let Some(url) = &settings.public_url {
        if !http_url_ok(url) {
            return err("public_url".into(), "must be an http or https URL");
        }
    }
    for (key, o) in &settings.events {
        if catalog::find(key).is_none() || key == "test" {
            return err(format!("events.{key}"), "unknown event");
        }
        if let Some(Route::Destinations(names)) = &o.route {
            for n in names {
                if !settings.destinations.iter().any(|d| &d.name == n) {
                    return err(
                        format!("events.{key}.route"),
                        &format!("no destination named {n}"),
                    );
                }
            }
        }
    }
    let t = &settings.thresholds;
    if t.disk_warning_percent == 0
        || t.disk_critical_percent > 100
        || t.disk_warning_percent > t.disk_critical_percent
    {
        return err(
            "thresholds.disk_warning_percent".into(),
            "warning must be between 1 and the critical threshold (at most 100)",
        );
    }
    if t.memory_percent == 0 || t.memory_percent > 100 {
        return err("thresholds.memory_percent".into(), "use 1 to 100");
    }
    if t.cert_critical_days > t.cert_warning_days {
        return err(
            "thresholds.cert_critical_days".into(),
            "must not exceed the warning days",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_rooms() {
        assert!(matrix_room_ok("!abc:example.org"));
        assert!(matrix_room_ok("#ops:example.org"));
        assert!(matrix_room_ok("!Zq9x-hash_without_server"));
        assert!(!matrix_room_ok("!"));
        assert!(!matrix_room_ok("!abc:"));
        assert!(!matrix_room_ok("ops:example.org"));
        assert!(!matrix_room_ok("#ops"));
        assert!(!matrix_room_ok("#:example.org"));
        assert!(!matrix_room_ok("#o ps:example.org"));
    }

    #[test]
    fn matrix_needs_url_token_and_room() {
        let s: AlertSettings = serde_json::from_value(serde_json::json!({
            "destinations": [{"name": "mx", "kind": "matrix", "room": "!r:example.org"}]
        }))
        .unwrap();
        let yes = |_: &str| true;
        let no = |_: &str| false;
        let url = |_: &str| Some("https://matrix.example.org".to_string());
        assert!(validate(&s, &yes, &yes, &url).is_ok());
        assert_eq!(
            validate(&s, &no, &yes, &url).unwrap_err().0,
            "destinations[0].url"
        );
        assert_eq!(
            validate(&s, &yes, &no, &url).unwrap_err().0,
            "destinations[0].auth"
        );
        let bad = |_: &str| Some("ftp://x".to_string());
        assert_eq!(
            validate(&s, &yes, &yes, &bad).unwrap_err().0,
            "destinations[0].url"
        );
        let mut s2 = s.clone();
        s2.destinations[0].kind = DestinationKind::Matrix(cl_alert::MatrixOptions {
            room: "nope".into(),
            style: Default::default(),
        });
        assert_eq!(
            validate(&s2, &yes, &yes, &url).unwrap_err().0,
            "destinations[0].room"
        );
    }

    #[test]
    fn discord_urls() {
        assert!(discord_url_ok(
            "https://discord.com/api/webhooks/123/abc_D-e"
        ));
        assert!(discord_url_ok(
            "https://ptb.discord.com/api/v10/webhooks/1/x"
        ));
        assert!(discord_url_ok("https://discordapp.com/api/webhooks/1/x"));
        assert!(!discord_url_ok("http://discord.com/api/webhooks/1/x"));
        assert!(!discord_url_ok("https://evil.com/api/webhooks/1/x"));
        assert!(!discord_url_ok("https://discord.com/api/webhooks/1"));
        assert!(!discord_url_ok("https://discord.com/api/webhooks/x/y"));
    }

    #[test]
    fn mentions_and_emails() {
        for m in ["@here", "@everyone", "<@123>", "<@&123>", "<@!123>"] {
            assert!(mention_ok(m), "{m}");
        }
        for m in ["here", "<@>", "<@&abc>", "<@123", "@role"] {
            assert!(!mention_ok(m), "{m}");
        }
        assert!(email_ok("a@b.co", false));
        assert!(email_ok("Clouduler <alerts@example.com>", true));
        assert!(!email_ok("Clouduler <alerts@example.com>", false));
        assert!(!email_ok("a b@c.d", false));
        assert!(!email_ok("nope", false));
    }

    #[test]
    fn round_trip_and_defaults() {
        let s = AlertSettings::default();
        let mut st = ClusterState::default();
        assert_eq!(load(&st), s);
        st.apply(&cl_raft::Request::PutConfig {
            name: SETTINGS_CONFIG.into(),
            content: encode(&s),
            owner: None,
        });
        assert_eq!(load(&st), s);
    }
}
