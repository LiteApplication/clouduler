//! Volume mounts use clouduler's drive syntax instead of compose named
//! volumes or host binds: `drive/volume:/container/path[:ro]`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A parsed volume mount. `drive`/`volume` are `None` for an anonymous
/// container path with no backing drive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawVolume", into = "RawVolume")]
pub struct VolumeMount {
    pub drive: Option<String>,
    pub volume: Option<String>,
    pub target: PathBuf,
    pub read_only: bool,
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
                match long.source {
                    None => Ok(VolumeMount {
                        drive: None,
                        volume: None,
                        target: PathBuf::from(long.target),
                        read_only: long.read_only,
                    }),
                    Some(source) => {
                        let (drive, volume) = split_source(&source)?;
                        Ok(VolumeMount {
                            drive: Some(drive),
                            volume: Some(volume),
                            target: PathBuf::from(long.target),
                            read_only: long.read_only,
                        })
                    }
                }
            }
        }
    }
}

impl From<VolumeMount> for RawVolume {
    fn from(v: VolumeMount) -> Self {
        let source = match (&v.drive, &v.volume) {
            (Some(d), Some(vol)) => Some(format!("{d}/{vol}")),
            _ => None,
        };
        RawVolume::Long(LongVolume {
            kind: Some("volume".to_string()),
            source,
            target: v.target.to_string_lossy().to_string(),
            read_only: v.read_only,
        })
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
        Some((drive, volume)) if !drive.is_empty() && !volume.is_empty() && !volume.contains('/') => {
            Ok((drive.to_string(), volume.to_string()))
        }
        Some(_) => Err(format!("\"{source}\" is not a valid drive/volume reference")),
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
                other => return Err(format!("unknown volume mode \"{other}\", expected ro or rw")),
            };
            Ok(VolumeMount {
                drive: Some(drive),
                volume: Some(volume),
                target: PathBuf::from(target),
                read_only,
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
        let err = serde_yaml::from_str::<VolumeMount>(
            "{ type: bind, source: /srv/data, target: /data }",
        )
        .unwrap_err();
        assert!(err.to_string().contains("bind mounts are not supported"));
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
