//! Service level references to cluster configs and secrets.
//!
//! Same shape as compose: a bare name, or a long form giving the mount target
//! and permissions. Secrets default to `/run/secrets/<name>`, matching docker,
//! so an image that already knows how to read a docker secret needs no changes.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where docker puts secrets, and so where we put them.
pub const SECRET_DIR: &str = "/run/secrets";

/// A config or secret mounted into a service.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawObjectRef", into = "RawObjectRef")]
pub struct ObjectRef {
    /// Name of the cluster object.
    pub source: String,
    /// Where it appears in the container. Defaults per kind.
    pub target: Option<PathBuf>,
    /// Octal file mode. Read only for everyone by default; a secret nobody can
    /// write is one fewer way for a container to leak it back out.
    pub mode: Option<u32>,
    pub uid: Option<String>,
    pub gid: Option<String>,
}

impl ObjectRef {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: None,
            mode: None,
            uid: None,
            gid: None,
        }
    }

    /// Path inside the container for a secret.
    pub fn secret_target(&self) -> PathBuf {
        self.target
            .clone()
            .unwrap_or_else(|| PathBuf::from(SECRET_DIR).join(&self.source))
    }

    /// Path inside the container for a config. Compose defaults to the root,
    /// which is rarely what anyone wants, but matching it avoids surprises.
    pub fn config_target(&self) -> PathBuf {
        self.target
            .clone()
            .unwrap_or_else(|| PathBuf::from("/").join(&self.source))
    }

    pub fn mode_or_default(&self) -> u32 {
        self.mode.unwrap_or(0o444)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RawObjectRef {
    Short(String),
    Long(LongObjectRef),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongObjectRef {
    source: String,
    #[serde(default)]
    target: Option<PathBuf>,
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    gid: Option<String>,
}

/// Compose writes modes as `0444`, which yaml reads as the decimal 444 unless
/// it is quoted. Accept both and treat a bare number as octal, since nobody
/// means decimal 444 when they write a file mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Mode {
    Number(u32),
    Text(String),
}

impl Mode {
    fn resolve(self) -> Result<u32, String> {
        let digits = match self {
            Mode::Number(n) => n.to_string(),
            Mode::Text(s) => s,
        };
        let trimmed = digits.trim().trim_start_matches("0o");
        u32::from_str_radix(trimmed, 8)
            .map_err(|_| format!("{digits} is not a valid octal file mode"))
    }
}

impl TryFrom<RawObjectRef> for ObjectRef {
    type Error = String;

    fn try_from(raw: RawObjectRef) -> Result<Self, Self::Error> {
        match raw {
            RawObjectRef::Short(source) => {
                if source.trim().is_empty() {
                    return Err("an object reference needs a name".into());
                }
                Ok(ObjectRef::new(source))
            }
            RawObjectRef::Long(long) => {
                if long.source.trim().is_empty() {
                    return Err("an object reference needs a source".into());
                }
                if let Some(target) = &long.target {
                    if !target.is_absolute() {
                        return Err(format!(
                            "target {} must be an absolute path in the container",
                            target.display()
                        ));
                    }
                }
                Ok(ObjectRef {
                    source: long.source,
                    target: long.target,
                    mode: long.mode.map(Mode::resolve).transpose()?,
                    uid: long.uid,
                    gid: long.gid,
                })
            }
        }
    }
}

impl From<ObjectRef> for RawObjectRef {
    fn from(r: ObjectRef) -> Self {
        if r.target.is_none() && r.mode.is_none() && r.uid.is_none() && r.gid.is_none() {
            return RawObjectRef::Short(r.source);
        }
        RawObjectRef::Long(LongObjectRef {
            source: r.source,
            target: r.target,
            mode: r.mode.map(|m| Mode::Number(octal_digits(m))),
            uid: r.uid,
            gid: r.gid,
        })
    }
}

/// Render a mode back as the decimal number whose digits read as octal, which
/// is the form compose files use.
fn octal_digits(mode: u32) -> u32 {
    format!("{mode:o}").parse().unwrap_or(444)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> ObjectRef {
        serde_yaml::from_str(yaml).expect("parse")
    }

    #[test]
    fn the_short_form_is_just_a_name() {
        let r = parse("db_password");
        assert_eq!(r.source, "db_password");
        assert_eq!(r.secret_target(), PathBuf::from("/run/secrets/db_password"));
        assert_eq!(r.mode_or_default(), 0o444);
    }

    #[test]
    fn the_long_form_sets_the_target() {
        let r = parse("source: nginx_conf\ntarget: /etc/nginx/nginx.conf\n");
        assert_eq!(r.source, "nginx_conf");
        assert_eq!(r.config_target(), PathBuf::from("/etc/nginx/nginx.conf"));
    }

    #[test]
    fn a_bare_mode_is_read_as_octal() {
        // 0444 in a compose file means the octal mode, not decimal 444.
        assert_eq!(parse("source: c\nmode: 0444\n").mode, Some(0o444));
        assert_eq!(parse("source: c\nmode: 0400\n").mode, Some(0o400));
        assert_eq!(parse("source: c\nmode: \"0640\"\n").mode, Some(0o640));
    }

    #[test]
    fn a_nonsense_mode_is_rejected() {
        assert!(serde_yaml::from_str::<ObjectRef>("source: c\nmode: 0999\n").is_err());
    }

    #[test]
    fn a_relative_target_is_rejected() {
        let err = serde_yaml::from_str::<ObjectRef>("source: c\ntarget: etc/thing\n").unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn an_empty_name_is_rejected() {
        assert!(serde_yaml::from_str::<ObjectRef>("\"\"").is_err());
    }

    #[test]
    fn references_round_trip() {
        for yaml in [
            "db_password",
            "source: nginx_conf\ntarget: /etc/nginx/nginx.conf\n",
        ] {
            let original = parse(yaml);
            let encoded = serde_yaml::to_string(&original).expect("encode");
            assert_eq!(parse(&encoded), original);
        }
    }

    #[test]
    fn secrets_default_to_the_docker_location() {
        // Images that already read docker secrets must work unchanged.
        assert_eq!(
            ObjectRef::new("token").secret_target(),
            PathBuf::from("/run/secrets/token")
        );
    }
}
