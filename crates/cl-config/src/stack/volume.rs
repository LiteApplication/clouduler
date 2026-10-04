//! Volume mounts use clouduler's drive syntax instead of compose named
//! volumes or host binds: `drive/volume:/container/path[:ro]`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::stack::bytesize::ByteSize;

/// A parsed volume mount. `drive`/`volume` are `None` for an anonymous
/// container path with no backing drive, and also for a `tmpfs` mount (which
/// carries its own settings in `tmpfs` instead).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawVolume", into = "RawVolume")]
pub struct VolumeMount {
    pub drive: Option<String>,
    pub volume: Option<String>,
    pub target: PathBuf,
    pub read_only: bool,
    /// Set for an in-memory volume with no backing drive. Mutually exclusive
    /// with `drive`/`volume`.
    pub tmpfs: Option<TmpfsOptions>,
}

/// Settings for a `type: tmpfs` volume.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TmpfsOptions {
    /// Maximum size. `None` leaves docker's own default (half the host's RAM)
    /// in effect.
    pub size: Option<ByteSize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RawVolume {
    Short(String),
    Long(LongVolume),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LongVolume {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    source: Option<String>,
    target: String,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    tmpfs: Option<RawTmpfsOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawTmpfsOptions {
    #[serde(default)]
    size: Option<ByteSize>,
}

impl TryFrom<RawVolume> for VolumeMount {
    type Error = String;

    fn try_from(raw: RawVolume) -> Result<Self, Self::Error> {
        match raw {
            RawVolume::Short(s) => parse_short(&s),
            RawVolume::Long(long) => {
                if long.kind.as_deref() == Some("bind") {
                    return Err(
                        "bind mounts are not supported, volumes must live on a drive so they can be replicated and migrated"
                            .to_string(),
                    );
                }
                if long.kind.as_deref() == Some("tmpfs") {
                    if long.source.is_some() {
                        return Err(
                            "a tmpfs volume cannot have a source, it has no backing drive"
                                .to_string(),
                        );
                    }
                    return Ok(VolumeMount {
                        drive: None,
                        volume: None,
                        target: PathBuf::from(long.target),
                        read_only: long.read_only,
                        tmpfs: Some(TmpfsOptions {
                            size: long.tmpfs.and_then(|t| t.size),
                        }),
                    });
                }
                match long.source {
                    None => Ok(VolumeMount {
                        drive: None,
                        volume: None,
                        target: PathBuf::from(long.target),
                        read_only: long.read_only,
                        tmpfs: None,
                    }),
                    Some(source) => {
                        let (drive, volume) = split_source(&source)?;
                        Ok(VolumeMount {
                            drive: Some(drive),
                            volume: Some(volume),
                            target: PathBuf::from(long.target),
                            read_only: long.read_only,
                            tmpfs: None,
                        })
                    }
                }
            }
        }
    }
}

impl From<VolumeMount> for RawVolume {
    /// Prefers the short `drive/volume:target[:ro]` form, since that's what
    /// everyone writes by hand; the long map form is only reached for an
    /// anonymous, read-only mount, which the short syntax can't express
    /// (its target-only case is always read-write).
    fn from(v: VolumeMount) -> Self {
        let target = v.target.to_string_lossy();
        if let Some(tmpfs) = v.tmpfs {
            return RawVolume::Long(LongVolume {
                kind: Some("tmpfs".to_string()),
                source: None,
                target: target.to_string(),
                read_only: v.read_only,
                tmpfs: Some(RawTmpfsOptions { size: tmpfs.size }),
            });
        }
        match (&v.drive, &v.volume) {
            (Some(d), Some(vol)) => RawVolume::Short(if v.read_only {
                format!("{d}/{vol}:{target}:ro")
            } else {
                format!("{d}/{vol}:{target}")
            }),
            _ if !v.read_only => RawVolume::Short(target.to_string()),
            _ => RawVolume::Long(LongVolume {
                kind: Some("volume".to_string()),
                source: None,
                target: target.to_string(),
                read_only: v.read_only,
                tmpfs: None,
            }),
        }
    }
}

/// `drive/volume` split on the first slash. A bare word with no slash isn't
/// accepted: clouduler has no notion of an unbacked named volume.
fn split_source(source: &str) -> Result<(String, String), String> {
    if source.starts_with('/') {
        return Err(format!(
            "\"{source}\" is a host path, volumes must live on a drive so they can be replicated and migrated"
        ));
    }
    match source.split_once('/') {
        Some((drive, volume))
            if !drive.is_empty()
                && !volume.is_empty()
                && !volume.contains('/')
                && !drive.contains('\0')
                && !volume.contains('\0')
                && !matches!(drive, "." | "..")
                // `.` alone is the whole-drive mount; every other all-dots
                // name (`..`, `...`) is a traversal or a lookalike of one.
                && (volume == "." || !volume.chars().all(|c| c == '.')) =>
        {
            Ok((drive.to_string(), volume.to_string()))
        }
        Some(_) => Err(format!(
            "\"{source}\" is not a valid drive/volume reference"
        )),
        None => Err(format!(
            "\"{source}\" must use drive/volume syntax, e.g. fast/gitlab-data"
        )),
    }
}

fn parse_short(raw: &str) -> Result<VolumeMount, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    match parts.as_slice() {
        [target] => Ok(VolumeMount {
            drive: None,
            volume: None,
            target: PathBuf::from(target),
            read_only: false,
            tmpfs: None,
        }),
        [source, target] => {
            if source.starts_with('/') {
                return Err(format!(
                    "\"{source}\" is a host path, volumes must live on a drive so they can be replicated and migrated"
                ));
            }
            let (drive, volume) = split_source(source)?;
            Ok(VolumeMount {
                drive: Some(drive),
                volume: Some(volume),
                target: PathBuf::from(target),
                read_only: false,
                tmpfs: None,
            })
        }
        [source, target, mode] => {
            if source.starts_with('/') {
                return Err(format!(
                    "\"{source}\" is a host path, volumes must live on a drive so they can be replicated and migrated"
                ));
            }
            let (drive, volume) = split_source(source)?;
            let read_only = match *mode {
                "ro" => true,
                "rw" => false,
                other => {
                    return Err(format!(
                        "unknown volume mode \"{other}\", expected ro or rw"
                    ))
                }
            };
            Ok(VolumeMount {
                drive: Some(drive),
                volume: Some(volume),
                target: PathBuf::from(target),
                read_only,
                tmpfs: None,
            })
        }
        _ => Err(format!("invalid volume syntax \"{raw}\"")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<VolumeMount, serde_yaml::Error> {
        serde_yaml::from_str::<VolumeMount>(&format!("\"{s}\""))
    }

    #[test]
    fn rejects_traversal_and_lookalike_volume_names() {
        for bad in [
            "fast/..:/x",
            "fast/...:/x",
            "../data:/x",
            "./data:/x",
            "fast/a\\u0000b:/x",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let whole = parse("fast/.:/x").unwrap();
        assert_eq!(whole.volume.as_deref(), Some("."));
    }

    #[test]
    fn drive_volume_target() {
        let v = parse("fast/gitlab-data:/var/opt/gitlab").unwrap();
        assert_eq!(v.drive.as_deref(), Some("fast"));
        assert_eq!(v.volume.as_deref(), Some("gitlab-data"));
        assert_eq!(v.target, PathBuf::from("/var/opt/gitlab"));
        assert!(!v.read_only);
    }

    #[test]
    fn read_only_suffix() {
        let v = parse("fast/gitlab-data:/data:ro").unwrap();
        assert!(v.read_only);
    }

    #[test]
    fn anonymous_container_path() {
        let v = parse("/var/lib/whatever").unwrap();
        assert_eq!(v.drive, None);
        assert_eq!(v.volume, None);
        assert_eq!(v.target, PathBuf::from("/var/lib/whatever"));
    }

    #[test]
    fn host_bind_mount_rejected() {
        let err = parse("/srv/data:/data").unwrap_err();
        assert!(err.to_string().contains("host path"));
    }

    #[test]
    fn bare_source_without_slash_rejected() {
        let err = parse("gitlab-data:/data").unwrap_err();
        assert!(err.to_string().contains("drive/volume syntax"));
    }

    #[test]
    fn long_form_bind_rejected() {
        let err =
            serde_yaml::from_str::<VolumeMount>("{ type: bind, source: /srv/data, target: /data }")
                .unwrap_err();
        assert!(err.to_string().contains("bind mounts are not supported"));
    }

    #[test]
    fn serializes_back_to_the_short_form() {
        let v = parse("fast/gitlab-data:/data").unwrap();
        assert_eq!(
            serde_yaml::to_string(&v).unwrap().trim(),
            "fast/gitlab-data:/data"
        );

        let v = parse("fast/gitlab-data:/data:ro").unwrap();
        assert_eq!(
            serde_yaml::to_string(&v).unwrap().trim(),
            "fast/gitlab-data:/data:ro"
        );

        let v = parse("/anon").unwrap();
        assert_eq!(serde_yaml::to_string(&v).unwrap().trim(), "/anon");
    }

    #[test]
    fn an_anonymous_read_only_mount_falls_back_to_the_long_form() {
        // The short syntax's target-only case is always read-write, so this
        // is the one shape that can't round trip through it.
        let v: VolumeMount = serde_yaml::from_str("{ target: /data, read_only: true }").unwrap();
        let encoded = serde_yaml::to_string(&v).unwrap();
        let reparsed: VolumeMount = serde_yaml::from_str(&encoded).unwrap();
        assert_eq!(reparsed, v);
    }

    #[test]
    fn tmpfs_volume_parses_with_a_size() {
        let v: VolumeMount =
            serde_yaml::from_str("{ type: tmpfs, target: /cache, tmpfs: { size: 64m } }").unwrap();
        assert_eq!(v.drive, None);
        assert_eq!(v.volume, None);
        assert_eq!(v.target, PathBuf::from("/cache"));
        let tmpfs = v.tmpfs.expect("tmpfs options");
        assert_eq!(tmpfs.size, Some(ByteSize(64 * 1024 * 1024)));
    }

    #[test]
    fn tmpfs_volume_without_a_size_is_allowed() {
        let v: VolumeMount = serde_yaml::from_str("{ type: tmpfs, target: /cache }").unwrap();
        assert_eq!(v.tmpfs.expect("tmpfs options").size, None);
    }

    #[test]
    fn tmpfs_volume_with_a_source_is_rejected() {
        let err = serde_yaml::from_str::<VolumeMount>(
            "{ type: tmpfs, source: fast/data, target: /cache }",
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot have a source"), "{err}");
    }

    #[test]
    fn tmpfs_volume_round_trips() {
        let v: VolumeMount =
            serde_yaml::from_str("{ type: tmpfs, target: /cache, tmpfs: { size: 64MiB } }")
                .unwrap();
        let encoded = serde_yaml::to_string(&v).unwrap();
        let reparsed: VolumeMount = serde_yaml::from_str(&encoded).unwrap();
        assert_eq!(reparsed, v);
    }

    #[test]
    fn long_form_drive_volume() {
        let v: VolumeMount = serde_yaml::from_str(
            "{ type: volume, source: fast/gitlab-data, target: /data, read_only: true }",
        )
        .unwrap();
        assert_eq!(v.drive.as_deref(), Some("fast"));
        assert!(v.read_only);
    }
}
