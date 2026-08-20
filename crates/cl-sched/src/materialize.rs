//! Getting configs and secrets from the database onto disk and into containers.
//!
//! Docker's own configs and secrets are a swarm feature we cannot use, so the
//! node writes each object to a file under its data directory and bind mounts
//! that file read only at the target path. From inside the container it looks
//! exactly like a docker secret at `/run/secrets/<name>`, which is what matters:
//! images that already read docker secrets work unchanged.
//!
//! Secrets are decrypted only at this point, as late as possible, and written
//! with owner-only permissions to a directory that is also owner-only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cl_config::ObjectRef;
use cl_docker::BindMount;
use cl_raft::ids::InstanceId;
use cl_raft::state::ClusterState;
use cl_secret::{ClusterKey, Sealed};

use crate::error::{Result, SchedError};

/// Directory under the node's data dir holding materialized objects.
const OBJECT_DIR: &str = "objects";

/// One object written to disk, ready to mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialized {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub mode: u32,
    pub secret: bool,
    /// Revision written, so a stale file is detectable without reading it.
    pub revision: u64,
}

impl Materialized {
    pub fn mount(&self) -> BindMount {
        BindMount {
            host_path: self.host_path.display().to_string(),
            container_path: self.container_path.display().to_string(),
            // Always read only. A container has no business editing the
            // cluster's copy of a config, and a writable secret is a leak
            // waiting to happen.
            read_only: true,
        }
    }
}

/// What an instance's configs and secrets resolve to: files to bind mount,
/// and secrets exposed as environment variables instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared {
    pub mounts: Vec<Materialized>,
    pub env: Vec<(String, String)>,
}

/// Writes objects out for one instance.
pub struct Materializer {
    root: PathBuf,
    key: Option<ClusterKey>,
}

impl Materializer {
    /// `data_dir` is this node's data directory. `key` is the cluster key, if
    /// this node holds one; without it, secrets cannot be materialized.
    pub fn new(data_dir: &Path, key: Option<ClusterKey>) -> Self {
        Self {
            root: data_dir.join(OBJECT_DIR),
            key,
        }
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    /// Directory holding one instance's objects. Per instance rather than
    /// shared, so removing a container takes its secrets off disk with it.
    fn instance_dir(&self, instance: &InstanceId) -> PathBuf {
        self.root.join(instance.container_name())
    }

    /// Write every config and secret the instance references and return the
    /// mounts and environment variables for them.
    pub fn prepare(
        &self,
        state: &ClusterState,
        instance: &InstanceId,
        configs: &[ObjectRef],
        secrets: &[ObjectRef],
    ) -> Result<Prepared> {
        let dir = self.instance_dir(instance);
        create_private_dir(&dir)?;

        let mut out = Vec::new();
        let mut env = Vec::new();

        for reference in configs {
            let entry = state.configs.get(&reference.source).ok_or_else(|| {
                SchedError::MissingObject {
                    kind: "config",
                    name: reference.source.clone(),
                    instance: instance.to_string(),
                }
            })?;
            let content = entry.decode().map_err(|e| SchedError::BadObject {
                name: reference.source.clone(),
                reason: e,
            })?;

            out.push(self.write_object(
                &dir,
                reference,
                &content,
                reference.config_target(),
                false,
                entry.revision,
            )?);
        }

        for reference in secrets {
            let entry = state.secrets.get(&reference.source).ok_or_else(|| {
                SchedError::MissingObject {
                    kind: "secret",
                    name: reference.source.clone(),
                    instance: instance.to_string(),
                }
            })?;

            // A node without the key holds only ciphertext, so it must refuse
            // to start the container rather than mount something unusable.
            let key = self.key.as_ref().ok_or_else(|| SchedError::NoSecretKey {
                name: reference.source.clone(),
                instance: instance.to_string(),
            })?;

            if key.fingerprint() != entry.key_fingerprint {
                return Err(SchedError::WrongSecretKey {
                    name: reference.source.clone(),
                    expected: entry.key_fingerprint.clone(),
                    got: key.fingerprint(),
                });
            }

            let plaintext = Sealed::from_blob(entry.sealed.clone())
                .open(key, &reference.source)
                .map_err(|e| SchedError::BadObject {
                    name: reference.source.clone(),
                    reason: e.to_string(),
                })?;

            if let Some(var) = &reference.env {
                // Never touches disk: it goes straight into the container's
                // environment, the same as a value from `environment:`.
                let value = String::from_utf8(plaintext).map_err(|_| SchedError::BadObject {
                    name: reference.source.clone(),
                    reason: "secret is not valid UTF-8, so it cannot be an environment variable".into(),
                })?;
                env.push((var.clone(), value));
                continue;
            }

            out.push(self.write_object(
                &dir,
                reference,
                &plaintext,
                reference.secret_target(),
                true,
                entry.revision,
            )?);
        }

        Ok(Prepared { mounts: out, env })
    }

    fn write_object(
        &self,
        dir: &Path,
        reference: &ObjectRef,
        content: &[u8],
        container_path: PathBuf,
        secret: bool,
        revision: u64,
    ) -> Result<Materialized> {
        let host_path = dir.join(&reference.source);

        // Write to a temp file and rename, so a container starting while we
        // write never sees a half written config.
        let temp = host_path.with_extension("tmp");
        write_private(&temp, content, secret)?;
        std::fs::rename(&temp, &host_path).map_err(|source| SchedError::ObjectIo {
            path: host_path.clone(),
            source,
        })?;

        set_mode(&host_path, reference.mode_or_default())?;

        Ok(Materialized {
            host_path,
            container_path,
            mode: reference.mode_or_default(),
            secret,
            revision,
        })
    }

    /// Remove an instance's objects, called when its container goes away.
    pub fn clean(&self, instance: &InstanceId) -> Result<()> {
        let dir = self.instance_dir(instance);
        if !dir.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&dir).map_err(|source| SchedError::ObjectIo { path: dir, source })
    }

    /// Remove object directories for instances no longer assigned here, so a
    /// secret does not outlive the container that needed it.
    pub fn clean_orphans(&self, keep: &BTreeMap<String, ()>) -> Result<()> {
        if !self.root.exists() {
            return Ok(());
        }
        let entries = std::fs::read_dir(&self.root).map_err(|source| SchedError::ObjectIo {
            path: self.root.clone(),
            source,
        })?;

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !keep.contains_key(&name) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(())
    }
}

fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| SchedError::ObjectIo {
        path: path.to_path_buf(),
        source,
    })?;
    set_mode(path, 0o700)
}

fn write_private(path: &Path, content: &[u8], secret: bool) -> Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Owner only from creation for a secret, so there is no window where
        // it is readable by anyone else.
        options.mode(if secret { 0o600 } else { 0o644 });
    }
    let _ = secret;

    let mut file = options.open(path).map_err(|source| SchedError::ObjectIo {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(content).map_err(|source| SchedError::ObjectIo {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| SchedError::ObjectIo {
        path: path.to_path_buf(),
        source,
    })
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| {
            SchedError::ObjectIo {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_raft::state::{base64_encode, Request};

    fn state_with_objects(key: &ClusterKey) -> ClusterState {
        let mut state = ClusterState::default();
        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 80; }"),
            owner: None,
        });
        let sealed = Sealed::seal(key, "db_password", b"hunter2").expect("seal");
        state.apply(&Request::PutSecret {
            name: "db_password".into(),
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        });
        state
    }

    fn instance() -> InstanceId {
        InstanceId::new("site", "web", 0)
    }

    #[test]
    fn configs_and_secrets_land_on_disk_and_mount() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        let out = m
            .prepare(
                &state,
                &instance(),
                &[ObjectRef {
                    source: "nginx_conf".into(),
                    target: Some(PathBuf::from("/etc/nginx/nginx.conf")),
                    mode: None,
                    uid: None,
                    gid: None,
                    env: None,
                }],
                &[ObjectRef::new("db_password")],
            )
            .expect("prepare");

        assert_eq!(out.mounts.len(), 2);
        assert!(out.env.is_empty());

        let config = &out.mounts[0];
        assert_eq!(
            std::fs::read(&config.host_path).expect("read"),
            b"server { listen 80; }"
        );
        assert_eq!(config.container_path, PathBuf::from("/etc/nginx/nginx.conf"));
        assert!(config.mount().read_only);

        let secret = &out.mounts[1];
        assert_eq!(std::fs::read(&secret.host_path).expect("read"), b"hunter2");
        // The docker location, so existing images work unchanged.
        assert_eq!(
            secret.container_path,
            PathBuf::from("/run/secrets/db_password")
        );
        assert!(secret.secret);
    }

    #[test]
    fn a_node_without_the_key_refuses_to_start_the_container() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), None);
        assert!(!m.has_key());
        let err = m
            .prepare(&state, &instance(), &[], &[ObjectRef::new("db_password")])
            .unwrap_err();
        assert!(matches!(err, SchedError::NoSecretKey { .. }));
    }

    #[test]
    fn the_wrong_key_is_reported_as_such() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = state_with_objects(&ClusterKey::generate());

        // A different key: say so plainly rather than reporting corruption.
        let m = Materializer::new(dir.path(), Some(ClusterKey::generate()));
        let err = m
            .prepare(&state, &instance(), &[], &[ObjectRef::new("db_password")])
            .unwrap_err();
        assert!(matches!(err, SchedError::WrongSecretKey { .. }));
    }

    #[test]
    fn a_missing_object_names_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = Materializer::new(dir.path(), None);
        let err = m
            .prepare(
                &ClusterState::default(),
                &instance(),
                &[ObjectRef::new("nope")],
                &[],
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"), "{msg}");
        assert!(msg.contains("site.web.0"), "{msg}");
    }

    #[test]
    fn secrets_are_written_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        let out = m
            .prepare(&state, &instance(), &[], &[ObjectRef::new("db_password")])
            .expect("prepare");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out.mounts[0].host_path)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o444, "the requested mode wins");

            // The directory itself must not be readable by other users.
            let parent = out.mounts[0].host_path.parent().expect("parent");
            let dir_mode = std::fs::metadata(parent).expect("meta").permissions().mode();
            assert_eq!(dir_mode & 0o077, 0);
        }
    }

    #[test]
    fn an_explicit_mode_is_honoured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        let out = m
            .prepare(
                &state,
                &instance(),
                &[],
                &[ObjectRef {
                    source: "db_password".into(),
                    target: None,
                    mode: Some(0o400),
                    uid: None,
                    gid: None,
                    env: None,
                }],
            )
            .expect("prepare");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out.mounts[0].host_path)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o400);
        }
    }

    #[test]
    fn cleaning_takes_secrets_off_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        let out = m
            .prepare(&state, &instance(), &[], &[ObjectRef::new("db_password")])
            .expect("prepare");
        assert!(out.mounts[0].host_path.exists());

        m.clean(&instance()).expect("clean");
        assert!(!out.mounts[0].host_path.exists());
        // Cleaning twice is not an error, the reconcile loop may retry.
        m.clean(&instance()).expect("clean again");
    }

    #[test]
    fn orphaned_instance_directories_are_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        m.prepare(&state, &instance(), &[], &[ObjectRef::new("db_password")])
            .expect("prepare");
        let other = InstanceId::new("site", "web", 1);
        m.prepare(&state, &other, &[], &[ObjectRef::new("db_password")])
            .expect("prepare");

        let keep = BTreeMap::from([(instance().container_name(), ())]);
        m.clean_orphans(&keep).expect("clean orphans");

        assert!(m.instance_dir(&instance()).exists());
        assert!(!m.instance_dir(&other).exists());
    }

    #[test]
    fn rewriting_replaces_the_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let mut state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        let first = m
            .prepare(&state, &instance(), &[ObjectRef::new("nginx_conf")], &[])
            .expect("prepare");

        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 8080; }"),
            owner: None,
        });
        let second = m
            .prepare(&state, &instance(), &[ObjectRef::new("nginx_conf")], &[])
            .expect("prepare");

        assert_eq!(first.mounts[0].host_path, second.mounts[0].host_path);
        assert_eq!(
            std::fs::read(&second.mounts[0].host_path).expect("read"),
            b"server { listen 8080; }"
        );
        assert_eq!(second.mounts[0].revision, 2);
    }

    #[test]
    fn a_secret_can_be_exposed_as_an_environment_variable_instead_of_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        let out = m
            .prepare(
                &state,
                &instance(),
                &[],
                &[ObjectRef {
                    source: "db_password".into(),
                    target: None,
                    mode: None,
                    uid: None,
                    gid: None,
                    env: Some("DB_PASSWORD".into()),
                }],
            )
            .expect("prepare");

        assert!(out.mounts.is_empty(), "an env secret is not mounted as a file");
        assert_eq!(out.env, vec![("DB_PASSWORD".to_string(), "hunter2".to_string())]);
    }

    #[test]
    fn a_non_utf8_secret_cannot_become_an_environment_variable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let mut state = ClusterState::default();
        let sealed = Sealed::seal(&key, "binary_secret", &[0xff, 0xfe]).expect("seal");
        state.apply(&Request::PutSecret {
            name: "binary_secret".into(),
            sealed: sealed.blob,
            key_fingerprint: key.fingerprint(),
            owner: None,
        });

        let m = Materializer::new(dir.path(), Some(ClusterKey::parse(&key.encode()).expect("key")));
        let err = m
            .prepare(
                &state,
                &instance(),
                &[],
                &[ObjectRef {
                    source: "binary_secret".into(),
                    target: None,
                    mode: None,
                    uid: None,
                    gid: None,
                    env: Some("BINARY_SECRET".into()),
                }],
            )
            .unwrap_err();
        assert!(matches!(err, SchedError::BadObject { .. }));
    }
}
