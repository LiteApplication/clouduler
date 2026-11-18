//! Registry logins: where they live and how a node gets at them.
//!
//! A login is an ordinary sealed cluster secret under a reserved name (hidden from the
//! generic secret routes). It is opened in memory when a pull or a digest check needs it
//! and never written to disk or logged. Both the scheduler and the api read through here.

use std::collections::BTreeMap;

use cl_docker::RegistryCredentials;
use cl_raft::state::ClusterState;
use cl_secret::{ClusterKey, Sealed};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Prefix marking a secret as a registry login. Underscore because
/// `validate_object_name` forbids a leading dot.
pub const SECRET_PREFIX: &str = "_clouduler_registry_";

/// True for a secret this module owns, which the generic secret routes hide.
pub fn is_registry_secret(name: &str) -> bool {
    name.starts_with(SECRET_PREFIX)
}

/// The secret name a login for `server` is stored under. A port colon is folded to an
/// underscore, so `host:5000` and `host_5000` would collide; lookups are unaffected since
/// `load` keys on the server stored in the sealed payload.
pub fn secret_name(server: &str) -> String {
    let safe: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{SECRET_PREFIX}{safe}")
}

/// What is sealed: the real server (the name may have been sanitised) and the login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredLogin {
    pub server: String,
    pub username: String,
    pub password: String,
}

impl StoredLogin {
    /// Seal this login under the name `secret_name(server)` returns.
    pub fn seal(&self, key: &ClusterKey) -> Result<(String, Sealed), cl_secret::SecretError> {
        let name = secret_name(&self.server);
        let json = serde_json::to_vec(self).map_err(|e| {
            cl_secret::SecretError::Malformed(format!("encoding a registry login: {e}"))
        })?;
        let sealed = Sealed::seal(key, &name, &json)?;
        Ok((name, sealed))
    }
}

/// Reduce what the operator typed to the host docker keys credentials on. Docker Hub's
/// spellings all fold to `docker.io`, otherwise a login would silently never match.
pub fn normalize_server(server: &str) -> String {
    let trimmed = server
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_ascii_lowercase();
    match trimmed.as_str() {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" | "hub.docker.com" => {
            cl_docker::DOCKER_HUB.to_string()
        }
        other => other.to_string(),
    }
}

/// Where a registry's v2 api lives. Docker Hub is the one registry whose
/// credential-store name is not the host its api answers on.
fn api_base(server: &str) -> String {
    if server == cl_docker::DOCKER_HUB {
        "https://registry-1.docker.io".to_string()
    } else {
        format!("https://{server}")
    }
}

/// Ask the registry whether it accepts this login (v2 handshake: `GET /v2/`, then for a
/// `Bearer` challenge fetch a token from the realm with basic auth; for `Basic` retry with
/// the header). Shared by the api and the cli; fails with a plain message.
pub async fn verify(
    http: &reqwest::Client,
    server: &str,
    username: &str,
    password: &str,
) -> Result<(), String> {
    let base = api_base(server);
    let probe = http
        .get(format!("{base}/v2/"))
        .send()
        .await
        .map_err(|e| format!("cannot reach {server}: {e}"))?;

    if probe.status().is_success() {
        // An open registry cannot check the password; storing it is still right since
        // public-read registries often want a login for private repositories.
        return Ok(());
    }
    if probe.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Err(format!(
            "{server} answered {} to a registry api request, which does not look like a container registry",
            probe.status()
        ));
    }

    let challenge = probe
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let response = if let Some(realm) = bearer_realm(&challenge) {
        let mut url = reqwest::Url::parse(&realm)
            .map_err(|e| format!("{server} named an unusable token url: {e}"))?;
        if let Some(service) = challenge_param(&challenge, "service") {
            url.query_pairs_mut().append_pair("service", &service);
        }
        http.get(url)
            .basic_auth(username, Some(password))
            .send()
            .await
            .map_err(|e| format!("cannot reach {server}'s token endpoint: {e}"))?
    } else {
        http.get(format!("{base}/v2/"))
            .basic_auth(username, Some(password))
            .send()
            .await
            .map_err(|e| format!("cannot reach {server}: {e}"))?
    };

    match response.status() {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            Err(format!("{server} rejected these credentials"))
        }
        other => Err(format!(
            "{server} answered {other} while checking these credentials"
        )),
    }
}

/// The `realm` of a `Bearer` challenge, or `None` for any other scheme.
fn bearer_realm(challenge: &str) -> Option<String> {
    if !challenge
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("bearer")
    {
        return None;
    }
    challenge_param(challenge, "realm")
}

/// Pull one quoted parameter out of a `WWW-Authenticate` challenge. Not a full RFC 7235
/// parser; an unreadable challenge falls back to basic auth.
fn challenge_param(challenge: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = challenge.find(&needle)? + needle.len();
    let rest = &challenge[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Every registry login the cluster holds, keyed by server.
#[derive(Debug, Clone, Default)]
pub struct Registries(BTreeMap<String, RegistryCredentials>);

impl Registries {
    /// Open every stored login with the cluster key. A login that will not open is skipped
    /// with a warning so one bad entry does not break the other registries.
    pub fn load(state: &ClusterState, key: &ClusterKey) -> Self {
        let mut logins = BTreeMap::new();
        for (name, entry) in &state.secrets {
            if !is_registry_secret(name) {
                continue;
            }
            let opened = Sealed::from_blob(entry.sealed.clone())
                .open(key, name)
                .map_err(|e| e.to_string())
                .and_then(|plain| {
                    serde_json::from_slice::<StoredLogin>(&plain).map_err(|e| e.to_string())
                });
            match opened {
                Ok(login) => {
                    logins.insert(
                        login.server.clone(),
                        RegistryCredentials {
                            username: login.username,
                            password: login.password,
                            server: login.server,
                        },
                    );
                }
                // `name` is derived from the host, so it is safe to log.
                Err(reason) => warn!(secret = %name, %reason, "could not open a registry login"),
            }
        }
        Self(logins)
    }

    /// The login to use when pulling or inspecting `image`. Matched on the reference's
    /// registry: docker sends credentials to whatever host the reference names, so a
    /// mismatch would disclose a password to a third party.
    pub fn for_image(&self, image: &str) -> Option<&RegistryCredentials> {
        self.0.get(&cl_docker::ImageRef::parse(image).registry())
    }

    /// The login stored for `server`, keyed as `login` stores it (for the cli's
    /// `registry test`/`rm`).
    pub fn get(&self, server: &str) -> Option<&RegistryCredentials> {
        self.0.get(&normalize_server(server))
    }

    /// Servers with a login stored, for the ui's account list.
    pub fn servers(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// `(server, username)` for each stored login. No accessor returns a password: it only
    /// leaves this type into docker via `for_image`, so no listing can leak one.
    pub fn accounts(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(server, creds)| (server.as_str(), creds.username.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::Request;

    fn state_with(key: &ClusterKey, login: &StoredLogin) -> ClusterState {
        let mut state = ClusterState::default();
        let (name, sealed) = login.seal(key).expect("seal");
        state.apply(&Request::PutSecret {
            name,
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        });
        state
    }

    fn login(server: &str) -> StoredLogin {
        StoredLogin {
            server: server.to_string(),
            username: "alice".into(),
            password: "hunter2".into(),
        }
    }

    #[test]
    fn a_stored_login_round_trips_through_raft() {
        let key = ClusterKey::generate();
        let state = state_with(&key, &login("docker.io"));
        let registries = Registries::load(&state, &key);
        let creds = registries
            .for_image("nginx:1.25")
            .expect("a docker hub login");
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "hunter2");
        assert_eq!(creds.server, "docker.io");
    }

    /// A host with no login gets none: handing another registry's password out is a disclosure.
    #[test]
    fn a_login_is_never_offered_to_a_different_registry() {
        let key = ClusterKey::generate();
        let state = state_with(&key, &login("git.example.com"));
        let registries = Registries::load(&state, &key);
        assert!(registries.for_image("git.example.com/team/app:1").is_some());
        assert!(registries.for_image("nginx:1.25").is_none());
        assert!(registries
            .for_image("other.example.com/team/app:1")
            .is_none());
    }

    /// The name is sanitised for a port, but what docker is told must still
    /// be the real host.
    #[test]
    fn a_port_survives_the_name_sanitising() {
        let key = ClusterKey::generate();
        assert_eq!(
            secret_name("registry.local:5000"),
            "_clouduler_registry_registry.local_5000"
        );
        let state = state_with(&key, &login("registry.local:5000"));
        let registries = Registries::load(&state, &key);
        let creds = registries
            .for_image("registry.local:5000/app:1")
            .expect("a login");
        assert_eq!(creds.server, "registry.local:5000");
    }

    #[test]
    fn a_login_that_will_not_open_does_not_take_the_others_with_it() {
        let key = ClusterKey::generate();
        let mut state = state_with(&key, &login("docker.io"));
        state.apply(&Request::PutSecret {
            name: secret_name("broken.example.com"),
            sealed: "not base64 at all!!".into(),
            key_fingerprint: key.fingerprint(),
            owner: None,
        });
        let registries = Registries::load(&state, &key);
        assert!(registries.for_image("nginx").is_some());
        assert_eq!(registries.servers().count(), 1);
    }

    #[test]
    fn every_spelling_of_docker_hub_folds_to_one() {
        for spelling in [
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "hub.docker.com",
            "https://hub.docker.com/",
            "  HUB.DOCKER.COM  ",
        ] {
            assert_eq!(normalize_server(spelling), "docker.io", "{spelling}");
        }
    }

    #[test]
    fn a_private_registry_keeps_its_host_and_port() {
        assert_eq!(
            normalize_server("https://git.example.com/"),
            "git.example.com"
        );
        assert_eq!(
            normalize_server("registry.local:5000"),
            "registry.local:5000"
        );
    }

    #[test]
    fn docker_hub_credentials_are_checked_against_the_registry_not_the_website() {
        assert_eq!(api_base("docker.io"), "https://registry-1.docker.io");
        assert_eq!(api_base("git.example.com"), "https://git.example.com");
    }

    #[test]
    fn a_bearer_challenge_yields_its_realm_and_service() {
        let challenge =
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#;
        assert_eq!(
            bearer_realm(challenge),
            Some("https://auth.docker.io/token".to_string())
        );
        assert_eq!(
            challenge_param(challenge, "service"),
            Some("registry.docker.io".to_string())
        );
    }

    #[test]
    fn a_basic_challenge_is_not_mistaken_for_a_bearer_one() {
        assert_eq!(bearer_realm(r#"Basic realm="registry""#), None);
    }
}
