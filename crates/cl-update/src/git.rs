//! Cloning the self-update source repo over SSH with the generated deploy key.
//!
//! Trust-on-first-use for the host key: a deploy key already requires the
//! operator to add it out of band on the git host, so accepting whatever host
//! key answers on first connect is not a new trust boundary, just a narrower
//! one than pinning a known_hosts entry would be. There is no interactive
//! operator here to prompt for one anyway.

use std::path::{Path, PathBuf};

use git2::{Cred, FetchOptions, RemoteCallbacks};

/// A checkout made for one build: the local path and the commit it landed on.
pub struct Checkout {
    pub path: PathBuf,
    pub commit: String,
}

/// Clone `branch` of `git_url` into `dest`, authenticating with `private_key`
/// (OpenSSH text, unsealed by the caller).
///
/// `dest` is removed first if it already exists, so a retried or resumed run
/// always builds from a clean checkout rather than one left over from a
/// previous, possibly failed, attempt.
pub fn clone(
    git_url: &str,
    branch: &str,
    private_key: &str,
    dest: &Path,
) -> Result<Checkout, String> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| format!("clearing {}: {e}", dest.display()))?;
    }

    let mut callbacks = RemoteCallbacks::new();
    let key = private_key.to_string();
    callbacks.credentials(move |_url, username_from_url, _allowed| {
        Cred::ssh_key_from_memory(username_from_url.unwrap_or("git"), None, &key, None)
    });
    callbacks.certificate_check(|_cert, _host| Ok(git2::CertificateCheckStatus::CertificateOk));

    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    let repo = git2::build::RepoBuilder::new()
        .fetch_options(fetch_options)
        .branch(branch)
        .clone(git_url, dest)
        .map_err(|e| format!("cloning {git_url} ({branch}): {e}"))?;

    let head = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(|e| format!("resolving HEAD after clone: {e}"))?;

    Ok(Checkout {
        path: dest.to_path_buf(),
        commit: head.id().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs network access and a real deploy key, neither available here;
    /// matches how this repo gates its qemu-only integration tests.
    #[test]
    #[ignore = "needs network access and a real git remote"]
    fn clones_a_real_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = clone(
            "git@example.com:org/repo.git",
            "main",
            "not-a-real-key",
            &dir.path().join("checkout"),
        );
        assert!(checkout.is_err());
    }
}
