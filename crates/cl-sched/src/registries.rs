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
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
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
        let creds = registries.for_image("nginx:1.25").expect("a docker hub login");
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
        assert!(registries.for_image("other.example.com/team/app:1").is_none());
    }

    /// The name is sanitised for a port, but what docker is told must still
    /// be the real host.
    #[test]
    fn a_port_survives_the_name_sanitising() {
        let key = ClusterKey::generate();
        assert_eq!(secret_name("registry.local:5000"), "_clouduler_registry_registry.local_5000");
        let state = state_with(&key, &login("registry.local:5000"));
        let registries = Registries::load(&state, &key);
        let creds = registries.for_image("registry.local:5000/app:1").expect("a login");
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
}
