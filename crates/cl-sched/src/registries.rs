//! Registry logins: where they live, and how a node gets at them.
//!
//! A login is an ordinary cluster secret under a reserved name, so it is
//! sealed with the cluster key, replicated through raft, and hidden from the
//! generic secret routes exactly like the Cloudflare token or the self-update
//! deploy key. Nothing here ever writes a password to disk or a log line: a
//! login is opened in memory at the moment a pull needs it and dropped again.
//!
//! Both the scheduler and the api read them through this module. The
//! scheduler needs them to pull a private image at all; the api needs them to
//! ask a registry what the current digest of a tag is, which for a private
//! repository is just as gated as the pull.

use std::collections::BTreeMap;

use cl_docker::RegistryCredentials;
use cl_raft::state::ClusterState;
use cl_secret::{ClusterKey, Sealed};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Prefix marking a secret as a registry login rather than a user's own.
///
/// Leading underscore for the same reason the other reserved objects use one:
/// `validate_object_name` forbids a leading dot, so an underscore is the
/// available way to keep clouduler's own names out of the namespace an
/// operator is likely to reach for.
pub const SECRET_PREFIX: &str = "_clouduler_registry_";

/// True for a secret this module owns, which the generic secret routes hide.
pub fn is_registry_secret(name: &str) -> bool {
    name.starts_with(SECRET_PREFIX)
}

/// The secret name a login for `server` is stored under.
///
/// A registry host can contain a colon (a port), which object names may not,
/// so it is folded to an underscore. That makes `registry.local:5000` and
/// `registry.local_5000` share a name, which would matter if both were ever
/// real hosts at once. It is not a correctness problem for lookups: the true
/// server is carried inside the sealed payload and `load` keys on that, never
/// on the name. Only storing both at once would have one overwrite the other.
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

/// What is actually sealed: the server this login is for, and the login.
///
/// The server is stored rather than derived from the secret name so a name
/// that had to be sanitised still round-trips to the exact host docker has to
/// be told about.
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

/// Reduce whatever the operator typed to the host docker itself keys
/// credentials on.
///
/// Docker Hub has four spellings in common use and only one of them is what a
/// credential store holds, so they are all folded to `docker.io` — otherwise
/// a login saved as `hub.docker.com` would simply never be found when pulling
/// `nginx`, with nothing to indicate why.
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

/// Ask the registry whether it accepts this login.
///
/// Follows the ordinary registry v2 handshake: `GET /v2/` either succeeds
/// outright, or answers 401 naming the scheme to use. For `Bearer` — Docker
/// Hub, Forgejo, Gitea, GHCR, GitLab — that means fetching a token from the
/// realm it names, with the login as basic auth; a wrong password is a 401
/// there. For `Basic`, the same request is simply retried with the header.
///
/// Shared by the api's `/api/registries` routes and the `clouduler registry`
/// cli, so a login typed at either one is checked the same way. Returns a
/// plain message on failure rather than an api-specific error type, since
/// the cli has no `ApiError` to build.
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
        // An open registry answers this anonymously, so there is nothing here
        // to check the password against. Storing it is still the right move:
        // the host is real and reachable, and a registry that is public for
        // reads commonly still wants a login for private repositories.
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

/// Pull one quoted parameter out of a `WWW-Authenticate` challenge.
///
/// Deliberately simple rather than a full RFC 7235 parser: every registry in
/// practice emits `Bearer realm="...",service="..."` with quoted values, and
/// a challenge this cannot read falls back to basic auth rather than failing.
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
    /// Open every stored login with the cluster key.
    ///
    /// A login that will not open is skipped with a warning rather than
    /// failing the lot: one bad entry — sealed under a key this node no
    /// longer has, say — must not stop every other registry from working.
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
                // `name` is safe to log; it is derived from the host, and the
                // value it guards never reached this branch.
                Err(reason) => warn!(secret = %name, %reason, "could not open a registry login"),
            }
        }
        Self(logins)
    }

    /// The login to use when pulling or inspecting `image`, if there is one.
    ///
    /// Matched on the registry the reference names, so credentials for one
    /// registry are never handed to another: docker sends whatever it is
    /// given to the host in the reference, so a mismatched login is a
    /// password disclosed to a third party, not a failed pull.
    pub fn for_image(&self, image: &str) -> Option<&RegistryCredentials> {
        self.0.get(&cl_docker::ImageRef::parse(image).registry())
    }

    /// The login stored for `server`, if any, keyed the same way `login`
    /// stores it. Used by the cli's `registry test` and `registry rm`, which
    /// take whatever spelling the operator typed rather than an image
    /// reference.
    pub fn get(&self, server: &str) -> Option<&RegistryCredentials> {
        self.0.get(&normalize_server(server))
    }

    /// Servers with a login stored, for the ui's account list.
    pub fn servers(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// `(server, username)` for each stored login.
    ///
    /// There is deliberately no accessor that hands back a password: the only
    /// way one leaves this type is into docker, through `for_image`. That
    /// keeps "which registries are configured" — a question the ui asks — and
    /// "what is the password" strictly separate, so no listing endpoint can
    /// disclose one by mistake.
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

    /// Handing one registry's password to another is a disclosure, not a
    /// missing feature, so a reference to a host with no login gets none.
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
