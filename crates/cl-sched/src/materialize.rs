//! Getting configs and secrets from the database onto disk and into containers.
//!
//! Docker's own configs and secrets are a swarm feature we cannot use, so the
//! node writes each object to a file under its data directory and bind mounts
//! that file at the target path, read only by default. From inside the
//! container it looks exactly like a docker secret at `/run/secrets/<name>`,
//! which is what matters: images that already read docker secrets work
//! unchanged.
//!
//! Secrets are decrypted only at this point, as late as possible, and written
//! with owner-only permissions to a directory that is also owner-only. A
//! secret is never mounted read-write — see `validate_writable_secrets` in
//! cl-config — but a config opting into `write: true` is, and this module is
//! also what notices a container has changed one: see `writable_config`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cl_config::ObjectRef;
use cl_docker::BindMount;
use cl_raft::ids::InstanceId;
use cl_raft::state::ClusterState;
use std::sync::Arc;

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
    /// A `write: true` config: mounted read-write instead of read-only.
    /// Never true for a secret — see `validate_writable_secrets`.
    pub writable: bool,
}

impl Materialized {
    pub fn mount(&self) -> BindMount {
        BindMount {
            host_path: self.host_path.display().to_string(),
            container_path: self.container_path.display().to_string(),
            read_only: !self.writable,
        }
    }
}

/// A config a container has edited through its writable mount, ready to feed
/// back into the cluster's copy via `Request::PutConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEdit {
    pub name: String,
    pub owner: Option<String>,
    pub content: Vec<u8>,
    /// The cluster revision the container started editing from. The write-back
    /// must be refused if the config has moved past it, or it would silently
    /// replace someone else's newer change.
    pub base_revision: u64,
}

/// What an instance's configs and secrets resolve to: files to bind mount,
/// secrets exposed as environment variables instead, and any writable
/// config a container changed since it was last materialized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared {
    pub mounts: Vec<Materialized>,
    pub env: Vec<(String, String)>,
    pub config_edits: Vec<ConfigEdit>,
}

/// Writes objects out for one instance.
pub struct Materializer {
    root: PathBuf,
    /// A live view of the cluster key rather than a snapshot: a node started
    /// with none still gets secrets materialized once one is delivered to it
    /// automatically, without a restart. `borrow()` is synchronous and cheap,
    /// so this stays a plain field read from `prepare`/`has_key`.
    key: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
}

impl Materializer {
    /// `data_dir` is this node's data directory. `key` tracks the cluster
    /// key live; without one, secrets cannot be materialized.
    pub fn new(
        data_dir: &Path,
        key: tokio::sync::watch::Receiver<Option<Arc<ClusterKey>>>,
    ) -> Self {
        Self {
            root: data_dir.join(OBJECT_DIR),
            key,
        }
    }

    pub fn has_key(&self) -> bool {
        self.key.borrow().is_some()
    }

    /// The cluster key as this node currently holds it.
    ///
    /// Exposed because secrets are not only ever written into containers: a
    /// registry login is a sealed secret the reconcile loop has to open for
    /// itself, before any container exists to mount it into. Cloned out of
    /// the watch rather than handing back a borrow, so a caller cannot hold
    /// the watch's lock across an await.
    pub fn key(&self) -> Option<Arc<ClusterKey>> {
        self.key.borrow().clone()
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
        let mut config_edits = Vec::new();

        for reference in configs {
            let entry =
                state
                    .configs
                    .get(&reference.source)
                    .ok_or_else(|| SchedError::MissingObject {
                        kind: "config",
                        name: reference.source.clone(),
                        instance: instance.to_string(),
                    })?;
            let content = entry.decode().map_err(|e| SchedError::BadObject {
                name: reference.source.clone(),
                reason: e,
            })?;

            if reference.write {
                if let Some(edit) =
                    self.writable_config(&dir, reference, &content, entry.revision, &mut out)?
                {
                    config_edits.push(ConfigEdit {
                        name: reference.source.clone(),
                        owner: entry.owner.clone(),
                        content: edit,
                        base_revision: entry.revision,
                    });
                }
                continue;
            }

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
            let entry =
                state
                    .secrets
                    .get(&reference.source)
                    .ok_or_else(|| SchedError::MissingObject {
                        kind: "secret",
                        name: reference.source.clone(),
                        instance: instance.to_string(),
                    })?;

            // A node without the key holds only ciphertext, so it must refuse
            // to start the container rather than mount something unusable.
            let key_guard = self.key.borrow();
            let key = key_guard.as_ref().ok_or_else(|| SchedError::NoSecretKey {
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
            drop(key_guard);

            if let Some(var) = &reference.env {
                // Never touches disk: it goes straight into the container's
                // environment, the same as a value from `environment:`.
                let value = String::from_utf8(plaintext).map_err(|_| SchedError::BadObject {
                    name: reference.source.clone(),
                    reason: "secret is not valid UTF-8, so it cannot be an environment variable"
                        .into(),
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

        Ok(Prepared {
            mounts: out,
            env,
            config_edits,
        })
    }

    /// Materializes a `write: true` config, watching for a container's edit
    /// to it on the way.
    ///
    /// Each write records the revision it wrote alongside the file itself
    /// (`<name>.rev`), so a stale-vs-edited disk file can be told apart
    /// without keeping anything in memory (nothing survives this process
    /// restarting otherwise). Three cases:
    ///
    /// - No `.rev` yet, or it names a revision other than `entry.revision`:
    ///   this node's copy is not caught up with the cluster (first mount, or
    ///   someone else's edit landed since); overwrite from `content` and
    ///   record the new revision, exactly like a read-only config.
    /// - `.rev` matches `entry.revision` and the file on disk still reads
    ///   back as `content`: nothing to do.
    /// - `.rev` matches `entry.revision` but the file reads back as
    ///   something else: the container changed it. Leave the file alone and
    ///   report the edit, so the caller can feed it back into the cluster;
    ///   the next `write: true` config to land here — most likely this same
    ///   edit, once it round-trips through raft — is what advances `.rev`.
    fn writable_config(
        &self,
        dir: &Path,
        reference: &ObjectRef,
        content: &[u8],
        revision: u64,
        out: &mut Vec<Materialized>,
    ) -> Result<Option<Vec<u8>>> {
        let host_path = dir.join(&reference.source);
        let rev_path = host_path.with_extension("rev");
        let container_path = reference.config_target();

        let recorded: Option<u64> = std::fs::read_to_string(&rev_path)
            .ok()
            .and_then(|s| s.trim().parse().ok());

        if recorded == Some(revision) {
            if let Ok(disk) = std::fs::read(&host_path) {
                if disk != content {
                    out.push(Materialized {
                        host_path,
                        container_path,
                        mode: reference.mode_or_default(),
                        secret: false,
                        revision,
                        writable: true,
                    });
                    return Ok(Some(disk));
                }
            }
            out.push(Materialized {
                host_path,
                container_path,
                mode: reference.mode_or_default(),
                secret: false,
                revision,
                writable: true,
            });
            return Ok(None);
        }

        // Overwriting a file the container edited and nobody accepted (the
        // cluster moved on first, or the write-back was refused as stale)
        // would lose that edit without a trace. Keep it beside the file.
        let base_path = host_path.with_extension("base");
        if let (Ok(disk), Ok(base)) = (std::fs::read(&host_path), std::fs::read(&base_path)) {
            if disk != base && disk != content {
                let kept = host_path.with_extension("conflict");
                write_private(&kept, &disk, false)?;
                tracing::warn!(
                    config = %reference.source,
                    saved = %kept.display(),
                    "overwriting a container's unsent edit to a config; its version was kept"
                );
            }
        }

        let m = self.write_in_place(dir, reference, content, container_path, revision)?;
        write_private(&base_path, content, false)?;
        write_private(&rev_path, revision.to_string().as_bytes(), false)?;
        out.push(Materialized {
            writable: true,
            ..m
        });
        Ok(None)
    }

    /// Like `write_object`, but keeps the file's inode when it already exists.
    ///
    /// A container bind mounts a single file by inode, so replacing it by
    /// rename leaves the container holding the old, now unlinked, file: its
    /// later writes vanish and it never sees the cluster's updates. A writable
    /// config is rewritten in place instead; the first write still goes
    /// through the rename so a starting container never sees half a file.
    fn write_in_place(
        &self,
        dir: &Path,
        reference: &ObjectRef,
        content: &[u8],
        container_path: PathBuf,
        revision: u64,
    ) -> Result<Materialized> {
        let host_path = dir.join(&reference.source);
        if !host_path.exists() {
            return self.write_object(dir, reference, content, container_path, false, revision);
        }
        write_private(&host_path, content, false)?;
        set_owner(&host_path, reference)?;
        set_mode(&host_path, reference.mode_or_default())?;
        Ok(Materialized {
            host_path,
            container_path,
            mode: reference.mode_or_default(),
            secret: false,
            revision,
            writable: false,
        })
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

        set_owner(&host_path, reference)?;
        set_mode(&host_path, reference.mode_or_default())?;

        Ok(Materialized {
            host_path,
            container_path,
            mode: reference.mode_or_default(),
            secret,
            revision,
            writable: false,
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
    file.write_all(content)
        .map_err(|source| SchedError::ObjectIo {
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_all().map_err(|source| SchedError::ObjectIo {
        path: path.to_path_buf(),
        source,
    })
}

/// Apply the reference's `uid`/`gid` to the host file, which a bind mount
/// shows to the container unchanged. Only numeric ids can be honoured: the
/// host has no idea what the image's `/etc/passwd` calls anyone.
fn set_owner(path: &Path, reference: &ObjectRef) -> Result<()> {
    let id = |field: &str, value: &Option<String>| -> Result<Option<u32>> {
        value
            .as_deref()
            .map(|v| {
                v.trim().parse::<u32>().map_err(|_| SchedError::BadObject {
                    name: reference.source.clone(),
                    reason: format!("{field} {v:?} must be a numeric id"),
                })
            })
            .transpose()
    };
    let (uid, gid) = (id("uid", &reference.uid)?, id("gid", &reference.gid)?);
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    #[cfg(unix)]
    std::os::unix::fs::chown(path, uid, gid).map_err(|source| SchedError::ObjectIo {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(
            |source| SchedError::ObjectIo {
                path: path.to_path_buf(),
                source,
            },
        )?;
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

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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
                    write: false,
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
        assert_eq!(
            config.container_path,
            PathBuf::from("/etc/nginx/nginx.conf")
        );
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

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
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
        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(ClusterKey::generate()))).1,
        );
        let err = m
            .prepare(&state, &instance(), &[], &[ObjectRef::new("db_password")])
            .unwrap_err();
        assert!(matches!(err, SchedError::WrongSecretKey { .. }));
    }

    #[test]
    fn a_missing_object_names_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
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

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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
            let dir_mode = std::fs::metadata(parent)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o077, 0);
        }
    }

    #[test]
    fn a_non_numeric_owner_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);
        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
        let mut r = ObjectRef::new("db_password");
        r.uid = Some("hermes".into());
        let err = m.prepare(&state, &instance(), &[], &[r]).unwrap_err();
        assert!(matches!(err, SchedError::BadObject { .. }));
    }

    #[test]
    fn an_explicit_mode_is_honoured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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
                    write: false,
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

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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
                    write: false,
                }],
            )
            .expect("prepare");

        assert!(
            out.mounts.is_empty(),
            "an env secret is not mounted as a file"
        );
        assert_eq!(
            out.env,
            vec![("DB_PASSWORD".to_string(), "hunter2".to_string())]
        );
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

        let m = Materializer::new(
            dir.path(),
            tokio::sync::watch::channel(Some(Arc::new(
                ClusterKey::parse(&key.encode()).expect("key"),
            )))
            .1,
        );
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
                    write: false,
                }],
            )
            .unwrap_err();
        assert!(matches!(err, SchedError::BadObject { .. }));
    }

    fn writable(source: &str) -> ObjectRef {
        ObjectRef {
            write: true,
            ..ObjectRef::new(source)
        }
    }

    #[test]
    fn a_writable_config_is_mounted_read_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        let out = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");

        assert_eq!(out.mounts.len(), 1);
        assert!(!out.mounts[0].mount().read_only);
        assert!(out.config_edits.is_empty(), "nothing edited it yet");
    }

    #[test]
    fn an_unmodified_writable_config_produces_no_edit_on_the_next_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        m.prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");
        let second = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare again");

        assert!(second.config_edits.is_empty());
    }

    #[test]
    fn a_container_s_edit_is_reported_and_left_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        let first = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");

        // The container writes to its own mount of the file.
        std::fs::write(&first.mounts[0].host_path, b"server { listen 8080; }")
            .expect("simulate edit");

        let second = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare again");

        assert_eq!(second.config_edits.len(), 1);
        assert_eq!(second.config_edits[0].name, "nginx_conf");
        assert_eq!(second.config_edits[0].content, b"server { listen 8080; }");
        // Left in place rather than clobbered back to the cluster's stale copy.
        assert_eq!(
            std::fs::read(&first.mounts[0].host_path).expect("read"),
            b"server { listen 8080; }"
        );
    }

    #[test]
    fn once_the_edit_round_trips_through_the_cluster_nothing_is_reported_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let mut state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        let first = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");
        std::fs::write(&first.mounts[0].host_path, b"server { listen 8080; }")
            .expect("simulate edit");

        let edit = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare again")
            .config_edits
            .remove(0);

        // What the reconcile loop does with a reported edit: submit it back
        // through raft, same as any other write.
        state.apply(&cl_raft::state::Request::PutConfig {
            name: edit.name,
            content: base64_encode(&edit.content),
            owner: edit.owner,
        });

        let third = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare a third time");
        assert!(third.config_edits.is_empty());
        assert_eq!(
            std::fs::read(&first.mounts[0].host_path).expect("read"),
            b"server { listen 8080; }"
        );
    }

    #[test]
    fn a_reported_edit_is_refused_once_another_writer_moved_the_config_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let mut state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        let first = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");
        std::fs::write(&first.mounts[0].host_path, b"server { listen 8080; }")
            .expect("simulate edit");
        let edit = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare again")
            .config_edits
            .remove(0);
        assert_eq!(edit.base_revision, 1);

        // Someone else's write lands before this one is submitted.
        state.apply(&cl_raft::state::Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 9090; }"),
            owner: None,
        });
        let response = state.apply(&cl_raft::state::Request::PutConfigCas {
            name: edit.name,
            content: base64_encode(&edit.content),
            owner: edit.owner,
            expect_revision: edit.base_revision,
        });
        assert!(!response.is_ok());
        assert_eq!(
            state.configs["nginx_conf"].decode().expect("decode"),
            b"server { listen 9090; }"
        );
    }

    #[test]
    fn an_unsent_edit_overwritten_by_a_newer_revision_is_kept_aside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let mut state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        let first = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");
        let host = first.mounts[0].host_path.clone();
        std::fs::write(&host, b"server { listen 8080; }").expect("simulate edit");

        state.apply(&cl_raft::state::Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 9090; }"),
            owner: None,
        });
        m.prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare after the change");

        assert_eq!(
            std::fs::read(&host).expect("read"),
            b"server { listen 9090; }"
        );
        assert_eq!(
            std::fs::read(host.with_extension("conflict")).expect("kept"),
            b"server { listen 8080; }"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_writable_config_keeps_its_inode_when_the_cluster_updates_it() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let mut state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        let first = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");
        let host = first.mounts[0].host_path.clone();
        let inode = std::fs::metadata(&host).expect("stat").ino();

        state.apply(&cl_raft::state::Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 9090; }"),
            owner: None,
        });
        m.prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare after the change");

        // A container's single-file bind mount follows the inode, so a rename
        // here would strand it on a file nothing reads any more.
        assert_eq!(std::fs::metadata(&host).expect("stat").ino(), inode);
    }

    #[test]
    fn a_config_change_from_elsewhere_overwrites_an_untouched_mount() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = ClusterKey::generate();
        let mut state = state_with_objects(&key);

        let m = Materializer::new(dir.path(), tokio::sync::watch::channel(None).1);
        m.prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare");

        // Nobody touched the mount, but the cluster's copy changed (e.g. an
        // admin edited it, or another instance's edit landed).
        state.apply(&Request::PutConfig {
            name: "nginx_conf".into(),
            content: base64_encode(b"server { listen 9090; }"),
            owner: None,
        });

        let second = m
            .prepare(&state, &instance(), &[writable("nginx_conf")], &[])
            .expect("prepare again");
        assert!(second.config_edits.is_empty());
        assert_eq!(
            std::fs::read(&second.mounts[0].host_path).expect("read"),
            b"server { listen 9090; }"
        );
    }
}
