//! Loads the Cloudflare API token from disk.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use cl_config::CloudflareConfig;

use crate::error::{DnsError, Result};

/// Reads the token from `cfg.api_token_file`. Returns `None` when the config
/// has no file set, which is valid: the token can be filled in later from
/// the web UI.
pub fn load(cfg: &CloudflareConfig) -> Result<Option<String>> {
    match &cfg.api_token_file {
        Some(path) => read_token(path).map(Some),
        None => Ok(None),
    }
}

fn read_token(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path).map_err(|source| DnsError::TokenRead {
        path: path.to_path_buf(),
        source,
    })?;

    // World readable secrets are a common misconfiguration on multi user
    // boxes; refuse rather than silently trust the file permissions.
    if meta.permissions().mode() & 0o004 != 0 {
        return Err(DnsError::TokenPermissions(path.to_path_buf()));
    }

    let raw = std::fs::read_to_string(path).map_err(|source| DnsError::TokenRead {
        path: path.to_path_buf(),
        source,
    })?;

    let token = raw.trim().to_string();
    if token.is_empty() {
        return Err(DnsError::TokenEmpty(path.to_path_buf()));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_token(dir: &Path, name: &str, content: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn loads_and_trims_token() {
        let dir = tempdir();
        let path = write_token(dir.path(), "cf.token", "  secret-123\n", 0o600);
        let cfg = CloudflareConfig {
            api_token_file: Some(path),
            zones: vec![],
        };
        assert_eq!(load(&cfg).unwrap(), Some("secret-123".to_string()));
    }

    #[test]
    fn none_when_no_file_configured() {
        let cfg = CloudflareConfig {
            api_token_file: None,
            zones: vec![],
        };
        assert_eq!(load(&cfg).unwrap(), None);
    }

    #[test]
    fn errors_on_missing_file() {
        let cfg = CloudflareConfig {
            api_token_file: Some(std::path::PathBuf::from("/nonexistent/cf.token")),
            zones: vec![],
        };
        assert!(matches!(load(&cfg), Err(DnsError::TokenRead { .. })));
    }

    #[test]
    fn errors_on_world_readable_file() {
        let dir = tempdir();
        let path = write_token(dir.path(), "cf.token", "secret", 0o644);
        let cfg = CloudflareConfig {
            api_token_file: Some(path),
            zones: vec![],
        };
        assert!(matches!(load(&cfg), Err(DnsError::TokenPermissions(_))));
    }

    #[test]
    fn errors_on_empty_file() {
        let dir = tempdir();
        let path = write_token(dir.path(), "cf.token", "   \n", 0o600);
        let cfg = CloudflareConfig {
            api_token_file: Some(path),
            zones: vec![],
        };
        assert!(matches!(load(&cfg), Err(DnsError::TokenEmpty(_))));
    }

    // Tiny stand-in for tempfile: a unique dir under std::env::temp_dir,
    // cleaned up on drop.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cl-dns-test-{}-{n}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        TempDir(dir)
    }
}
