use crate::cluster::MAX_STOP_GRACE;
use crate::error::{ConfigError, Result};
use crate::stack::deploy::{parse_memory_bytes, MAX_REPLICAS};
use crate::stack::devices::DeviceMapping;
use crate::stack::extensions::DockerSocketMode;
use crate::stack::StackFile;
use std::collections::BTreeMap;

impl StackFile {
    /// `x-clouduler.terraform`: variable names, duplicates and file paths.
    pub(super) fn validate_terraform(&self) -> Result<()> {
        for (name, svc) in &self.services {
            if let Some(tf) = svc.x_clouduler.as_ref().and_then(|x| x.terraform.as_ref()) {
                let prefix = format!("services.{name}.x-clouduler.terraform");
                tf.validate(&prefix)?;
                // A bundled `file:` object belongs to the stack and is rewritten on every deploy.
                for (i, o) in tf.outputs.iter().enumerate() {
                    let bundled = match o.target() {
                        Some(crate::stack::TfOutputTarget::Secret(n)) => self
                            .secrets
                            .get(n)
                            .is_some_and(|s| s.as_ref().and_then(|s| s.file.as_ref()).is_some())
                            .then_some(("secret", n)),
                        Some(crate::stack::TfOutputTarget::Config(n)) => self
                            .configs
                            .get(n)
                            .is_some_and(|c| c.as_ref().and_then(|c| c.file.as_ref()).is_some())
                            .then_some(("config", n)),
                        None => None,
                    };
                    if let Some((kind, n)) = bundled {
                        return Err(ConfigError::invalid(
                            format!("{prefix}.outputs[{i}].{kind}"),
                            format!("{kind} {n:?} is a bundled file of this stack, so a module output cannot overwrite it"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `x-clouduler.alerts`: patterns compile, `services` only at stack level
    /// and naming real services, mute keys well formed.
    pub(super) fn validate_alerts(&self) -> Result<()> {
        if let Some(alerts) = self.x_clouduler.as_ref().and_then(|x| x.alerts.as_ref()) {
            alerts.validate("x-clouduler.alerts", false)?;
            for (i, rule) in alerts.logs.iter().enumerate() {
                for name in &rule.services {
                    if !self.services.contains_key(name) {
                        return Err(ConfigError::invalid(
                            format!("x-clouduler.alerts.logs[{i}].services"),
                            format!("no service named {name:?} in this stack"),
                        ));
                    }
                }
            }
        }
        for (name, svc) in &self.services {
            if let Some(alerts) = svc.x_clouduler.as_ref().and_then(|x| x.alerts.as_ref()) {
                alerts.validate(&format!("services.{name}.x-clouduler.alerts"), true)?;
            }
        }
        Ok(())
    }

    /// The clouduler socket only serves read-only image endpoints; a read-write
    /// request is refused rather than silently downgraded.
    pub(super) fn validate_docker_sockets(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(socket) = svc
                .x_clouduler
                .as_ref()
                .and_then(|x| x.docker_socket.as_ref())
            else {
                continue;
            };
            if socket.mode == DockerSocketMode::Clouduler && !socket.read_only {
                return Err(ConfigError::invalid(
                    format!("services.{name}.x-clouduler.docker_socket.read_only"),
                    "mode: clouduler is read-only; set read_only: true",
                ));
            }
        }
        Ok(())
    }

    /// Replica counts are capped at `MAX_REPLICAS`: the scheduler builds a record
    /// per replica every pass, so `replicas: 4000000000` would take down the leader.
    pub(super) fn validate_replicas(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let replicas = svc.deploy.as_ref().and_then(|d| d.replicas);
            if let Some(n) = replicas.filter(|n| *n > MAX_REPLICAS) {
                return Err(ConfigError::invalid(
                    format!("services.{name}.deploy.replicas"),
                    format!("{n} is more than the maximum of {MAX_REPLICAS}"),
                ));
            }
        }
        Ok(())
    }

    /// A memory size that overflows a u64 once the unit is applied is rejected
    /// (other unparseable sizes are still ignored).
    pub(super) fn validate_memory_sizes(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(resources) = svc.deploy.as_ref().and_then(|d| d.resources.as_ref()) else {
                continue;
            };
            for (kind, raw) in [
                (
                    "limits",
                    resources.limits.as_ref().and_then(|s| s.memory.as_deref()),
                ),
                (
                    "reservations",
                    resources
                        .reservations
                        .as_ref()
                        .and_then(|s| s.memory.as_deref()),
                ),
            ] {
                let Some(raw) = raw else {
                    continue;
                };
                if parse_memory_bytes(raw).is_none() && memory_overflows(raw) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.deploy.resources.{kind}.memory"),
                        format!("\"{raw}\" is too large to be a byte count"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// GPU device requests and `devices:` entries are well formed, and host
    /// devices stay under `/dev/`.
    pub(super) fn validate_devices(&self) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, entry) in svc.devices.iter().enumerate() {
                if let Err(msg) = DeviceMapping::parse(entry) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.devices[{idx}]"),
                        msg,
                    ));
                }
            }
            for (idx, request) in svc.device_requests().iter().enumerate() {
                if let Err(msg) = request.validate() {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.deploy.resources.reservations.devices[{idx}]"),
                        msg,
                    ));
                }
            }
        }
        Ok(())
    }

    /// `write: true` feeds container edits back into the cluster object: fine for
    /// a config, never for a secret (its plaintext must not be overwritable).
    pub(super) fn validate_writable_secrets(&self) -> Result<()> {
        for (name, svc) in &self.services {
            for (idx, secret) in svc.secrets.iter().enumerate() {
                if secret.write {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.secrets[{idx}]"),
                        "write is not allowed on a secret, only on a config",
                    ));
                }
            }
        }
        Ok(())
    }

    /// A `write: true` config is copied per instance and every copy feeds back
    /// into one object; multiple writers overwrite each other, so only a single
    /// instance may hold it writable.
    pub(super) fn validate_single_config_writer(&self) -> Result<()> {
        let mut writers: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, svc) in &self.services {
            let replicas = svc.deploy.as_ref().and_then(|d| d.replicas).unwrap_or(1);
            for (idx, config) in svc.configs.iter().enumerate().filter(|(_, c)| c.write) {
                let field = format!("services.{name}.configs[{idx}]");
                if replicas > 1 {
                    return Err(ConfigError::invalid(
                        field,
                        format!(
                            "{} is writable but this service runs {replicas} replicas; \
                             only one instance may write a config",
                            config.source
                        ),
                    ));
                }
                if let Some(other) = writers.insert(config.source.as_str(), name.as_str()) {
                    if other != name {
                        return Err(ConfigError::invalid(
                            field,
                            format!(
                                "{} is already writable in service {other}; \
                                 mount it read-only here so there is a single writer",
                                config.source
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `before`/`after` on `volume_sync` run a service's action, so they only
    /// make sense on a service and must name an action it defines.
    pub(super) fn validate_sync_hooks(&self) -> Result<()> {
        if let Some(defaults) = &self.x_clouduler {
            for (volume, o) in &defaults.volume_sync {
                if o.enabled.is_some() {
                    return Err(enabled_rejected(format!(
                        "x-clouduler.volume_sync.{volume}.enabled"
                    )));
                }
                if o.before.is_some() || o.after.is_some() {
                    return Err(ConfigError::invalid(
                        format!("x-clouduler.volume_sync.{volume}"),
                        "before/after run a service's action, set them on the service",
                    ));
                }
            }
        }
        for (name, svc) in &self.services {
            let Some(ext) = &svc.x_clouduler else {
                continue;
            };
            for (label, action) in &ext.actions {
                let Some(image) = &action.image else { continue };
                let field = format!("services.{name}.x-clouduler.actions.{label}.image");
                if !action.standalone {
                    return Err(ConfigError::invalid(
                        field,
                        "image needs standalone: true, other actions run inside the service's own container",
                    ));
                }
                if image.trim().is_empty() || image.contains(char::is_whitespace) {
                    return Err(ConfigError::invalid(
                        field,
                        "must be an image reference without spaces",
                    ));
                }
            }
            for (volume, o) in &ext.volume_sync {
                if o.enabled.is_some() {
                    return Err(enabled_rejected(format!(
                        "services.{name}.x-clouduler.volume_sync.{volume}.enabled"
                    )));
                }
                for (key, action) in [("before", &o.before), ("after", &o.after)] {
                    let Some(action) = action else { continue };
                    if !ext.actions.contains_key(action) {
                        return Err(ConfigError::invalid(
                            format!("services.{name}.x-clouduler.volume_sync.{volume}.{key}"),
                            format!("no action named {action:?} on this service"),
                        ));
                    }
                    if ext.actions[action].standalone {
                        return Err(ConfigError::invalid(
                            format!("services.{name}.x-clouduler.volume_sync.{volume}.{key}"),
                            format!(
                                "action {action:?} is standalone, which stops the service: a sync hook runs inside the running container"
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_colocate(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(ext) = &svc.x_clouduler else {
                continue;
            };
            for other in &ext.colocate {
                if other == name {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.x-clouduler.colocate"),
                        "a service cannot colocate with itself",
                    ));
                }
                if !self.services.contains_key(other) {
                    return Err(ConfigError::invalid(
                        format!("services.{name}.x-clouduler.colocate"),
                        format!("no service named {other} in this stack"),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_stop_signals(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(signal) = &svc.stop_signal else {
                continue;
            };
            let valid = signal.strip_prefix("SIG").is_some_and(|rest| {
                !rest.is_empty() && rest.chars().all(|c| c.is_ascii_uppercase())
            }) || (!signal.is_empty() && signal.chars().all(|c| c.is_ascii_digit()));
            if !valid {
                return Err(ConfigError::invalid(
                    format!("services.{name}.stop_signal"),
                    format!("\"{signal}\" is not a signal name (e.g. SIGINT) or number"),
                ));
            }
        }
        Ok(())
    }

    /// A service's `stop_grace_period` overrides `failover.stop_grace`, so it
    /// gets the same bounds: non-zero and at most `MAX_STOP_GRACE`.
    pub(super) fn validate_stop_grace_periods(&self) -> Result<()> {
        for (name, svc) in &self.services {
            let Some(grace) = svc.stop_grace_period else {
                continue;
            };
            let field = format!("services.{name}.stop_grace_period");
            if grace.is_zero() {
                return Err(ConfigError::invalid(field, "must be greater than zero"));
            }
            if grace > MAX_STOP_GRACE {
                return Err(ConfigError::invalid(
                    field,
                    format!("must be at most {MAX_STOP_GRACE:?}"),
                ));
            }
        }
        Ok(())
    }
}

/// `volume_sync.VOL.enabled` was accepted but never did anything. Replication
/// cannot be off per volume: failover needs a current replica of every mounted
/// volume, so a never-copied volume would pin the service to its node forever.
fn enabled_rejected(path: String) -> ConfigError {
    ConfigError::invalid(
        path,
        "per-volume replication cannot be turned off, so `enabled` is no longer accepted. \
         Remove it, set a longer `interval` for a volume that changes rarely, or put \
         scratch data on a drive that only one node implements (such a drive has nowhere \
         to replicate to)",
    )
}

/// Whether `raw` is all digits (plus an optional unit letter) but overflows a
/// u64, the one `parse_memory_bytes` failure worth an error.
fn memory_overflows(raw: &str) -> bool {
    let raw = raw.trim();
    let digits = raw.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &raw[digits.len()..];
    unit.len() <= 1
        && !digits.trim().is_empty()
        && digits.trim().chars().all(|c| c.is_ascii_digit())
}
