//! Bundling a stack file with the files it references.
//!
//! A stack's `configs` and `secrets` point at paths on whoever ran the deploy.
//! Those paths mean nothing on the node that ends up running the container, so
//! the contents are read once at deploy time and travel with the stack.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{ConfigError, Result};
use crate::stack::StackFile;

/// A stack plus the contents of every file it referenced.
#[derive(Debug, Clone, PartialEq)]
pub struct StackBundle {
    pub name: String,
    /// The yaml exactly as written, so the ui can round trip it.
    pub spec: String,
    pub parsed: StackFile,
    /// Keyed by the config or secret name, not the path, since the path is
    /// meaningless once it leaves this machine.
    pub files: BTreeMap<String, BundledFile>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BundledFile {
    pub content: Vec<u8>,
    /// Secrets are redacted in output and get stricter permissions on write.
    pub secret: bool,
    /// Where it came from, kept for error messages only.
    pub source: PathBuf,
}

impl StackBundle {
    /// Read a stack file and everything it points at. Relative paths resolve
    /// against the stack file's own directory, which is what compose does and
    /// what people expect.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let spec = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ConfigError::invalid(
                    path.display().to_string(),
                    "cannot derive a stack name from this filename",
                )
            })?
            .to_string();

        let base = path.parent().unwrap_or(Path::new("."));
        Self::from_spec(&name, &spec, base)
    }

    /// Parse a spec and gather its files relative to `base`.
    pub fn from_spec(name: &str, spec: &str, base: &Path) -> Result<Self> {
        let parsed = StackFile::parse(spec)?;
        let mut files = BTreeMap::new();

        for (key, config) in &parsed.configs {
            if let Some(file) = config.as_ref().and_then(|c| c.file.as_ref()) {
                files.insert(key.clone(), read_file(base, file, false)?);
            }
        }
        for (key, secret) in &parsed.secrets {
            if let Some(file) = secret.as_ref().and_then(|s| s.file.as_ref()) {
                files.insert(key.clone(), read_file(base, file, true)?);
            }
        }

        Ok(StackBundle {
            name: name.to_string(),
            spec: spec.to_string(),
            parsed,
            files,
        })
    }

    /// Total size of the bundled files, which the cluster caps.
    pub fn files_size(&self) -> usize {
        self.files.values().map(|f| f.content.len()).sum()
    }
}

fn read_file(base: &Path, rel: &Path, secret: bool) -> Result<BundledFile> {
    let path = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        base.join(rel)
    };

    let content = std::fs::read(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;

    Ok(BundledFile {
        content,
        secret,
        source: path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write");
        path
    }

    const SPEC: &str = r#"
services:
  web:
    image: nginx
configs:
  nginx_conf:
    file: ./nginx.conf
secrets:
  db_password:
    file: ./db.secret
"#;

    #[test]
    fn bundles_configs_and_secrets() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "nginx.conf", "server {}");
        write(dir.path(), "db.secret", "hunter2");
        let path = write(dir.path(), "site.yaml", SPEC);

        let bundle = StackBundle::load(&path).expect("bundle");
        assert_eq!(bundle.name, "site");
        assert_eq!(bundle.files.len(), 2);
        assert_eq!(bundle.files["nginx_conf"].content, b"server {}");
        assert!(!bundle.files["nginx_conf"].secret);
        assert!(bundle.files["db_password"].secret);
        assert_eq!(bundle.files_size(), 16);
    }

    #[test]
    fn relative_paths_resolve_against_the_stack_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("conf");
        std::fs::create_dir(&sub).expect("mkdir");
        write(&sub, "nginx.conf", "ok");

        let spec =
            "services:\n  web:\n    image: nginx\nconfigs:\n  c:\n    file: ./conf/nginx.conf\n";
        let path = write(dir.path(), "site.yaml", spec);

        let bundle = StackBundle::load(&path).expect("bundle");
        assert_eq!(bundle.files["c"].content, b"ok");
    }

    #[test]
    fn a_missing_referenced_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "site.yaml", SPEC);

        let err = StackBundle::load(&path).unwrap_err();
        assert!(err.to_string().contains("nginx.conf"), "{err}");
    }

    #[test]
    fn a_stack_with_no_files_bundles_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "site.yaml",
            "services:\n  web:\n    image: nginx\n",
        );

        let bundle = StackBundle::load(&path).expect("bundle");
        assert!(bundle.files.is_empty());
        assert_eq!(bundle.files_size(), 0);
    }

    #[test]
    fn the_stack_name_comes_from_the_filename() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "gitlab.yml",
            "services:\n  web:\n    image: nginx\n",
        );
        assert_eq!(StackBundle::load(&path).expect("bundle").name, "gitlab");
    }

    #[test]
    fn the_raw_spec_is_kept_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = "# a comment worth keeping\nservices:\n  web:\n    image: nginx\n";
        let path = write(dir.path(), "site.yaml", spec);

        // The ui edits this text, so comments and formatting must survive.
        assert_eq!(StackBundle::load(&path).expect("bundle").spec, spec);
    }
}
