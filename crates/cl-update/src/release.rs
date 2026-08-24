//! Resolving a GitHub release and downloading its per-architecture asset.
//!
//! Replaces the old git-clone-and-build pipeline: an operator points
//! `SelfUpdateConfig` at either a repo (track its latest release) or one
//! specific release's page, and every node downloads the prebuilt binary for
//! its own architecture straight from GitHub instead of compiling it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Asset name convention this project's release workflow publishes under,
/// keyed by the same arch strings nodes report in `NodeState::arch`
/// (`std::env::consts::ARCH`, i.e. `x86_64` / `aarch64`).
fn asset_name(arch: &str) -> String {
    format!("clouduler-{arch}-unknown-linux-gnu")
}

/// What a release source resolves to before any asset is picked.
struct RepoRef {
    owner: String,
    repo: String,
    /// `None` means "the latest release".
    tag: Option<String>,
}

/// Parse either form the operator may paste in:
/// `https://github.com/OWNER/REPO` (track latest) or
/// `https://github.com/OWNER/REPO/releases/tag/TAG` (pin one release).
fn parse_source(source: &str) -> Result<RepoRef, String> {
    let trimmed = source.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .ok_or_else(|| format!("{source} is not a github.com url"))?;

    let mut parts = rest.splitn(5, '/');
    let owner = parts.next().filter(|s| !s.is_empty());
    let repo = parts.next().filter(|s| !s.is_empty());
    let (owner, repo) = match (owner, repo) {
        (Some(owner), Some(repo)) => (owner.to_string(), repo.to_string()),
        _ => return Err(format!("{source} does not name an owner and repo")),
    };

    match (parts.next(), parts.next(), parts.next()) {
        (None, None, None) => Ok(RepoRef { owner, repo, tag: None }),
        (Some("releases"), Some("tag"), Some(tag)) if !tag.is_empty() => {
            Ok(RepoRef { owner, repo, tag: Some(tag.to_string()) })
        }
        _ => Err(format!("{source} is neither a repo url nor a release tag url")),
    }
}

/// One resolved download target for one architecture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub id: u64,
    pub name: String,
}

/// Everything a run needs to know about the release it is rolling out,
/// resolved once by the leader and then carried in `UpdateRunState` so every
/// worker downloads a specific, agreed-upon asset rather than each re-asking
/// "what's latest" and risking a release landing mid-rollout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRelease {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    /// arch -> the asset built for it, for every arch this release published
    /// a binary for.
    pub assets: BTreeMap<String, ReleaseAsset>,
}

/// `octocrab::Error::GitHub` has no `#[snafu(display(..))]` override, so its
/// `Display` falls back to snafu's default of printing just the variant name
/// ("GitHub") — the actual reason GitHub gave (rate limited, not found, bad
/// credentials, ...) lives in the wrapped `GitHubError` and would otherwise
/// be silently dropped from every error message this module produces.
fn describe(e: &octocrab::Error) -> String {
    match e {
        octocrab::Error::GitHub { source, .. } => source.message.clone(),
        other => other.to_string(),
    }
}

/// Build an authenticated (if `token` is set) octocrab client. Anonymous
/// access works fine against public repos, just at GitHub's much lower
/// unauthenticated rate limit.
fn client(token: Option<&str>) -> Result<octocrab::Octocrab, String> {
    let mut builder = octocrab::Octocrab::builder();
    if let Some(token) = token {
        builder = builder.personal_token(token.to_string());
    }
    builder.build().map_err(|e| format!("building github client: {e}"))
}

/// Resolve `source` (a repo url or a release tag url) to the release it
/// names and the assets it published for the architectures clouduler builds
/// for.
pub async fn resolve(source: &str, token: Option<&str>) -> Result<ResolvedRelease, String> {
    let repo_ref = parse_source(source)?;
    let octocrab = client(token)?;
    let repo_handler = octocrab.repos(&repo_ref.owner, &repo_ref.repo);
    let releases = repo_handler.releases();
    let release = match &repo_ref.tag {
        Some(tag) => releases.get_by_tag(tag).await.map_err(|e| {
            format!("fetching release {tag} of {}/{}: {}", repo_ref.owner, repo_ref.repo, describe(&e))
        })?,
        None => releases.get_latest().await.map_err(|e| {
            format!("fetching the latest release of {}/{}: {}", repo_ref.owner, repo_ref.repo, describe(&e))
        })?,
    };

    let mut assets = BTreeMap::new();
    for arch in ["x86_64", "aarch64"] {
        let name = asset_name(arch);
        if let Some(asset) = release.assets.iter().find(|a| a.name == name) {
            assets.insert(arch.to_string(), ReleaseAsset { id: asset.id.0, name: asset.name.clone() });
        }
    }
    if assets.is_empty() {
        return Err(format!(
            "release {} has no clouduler-*-unknown-linux-gnu assets",
            release.tag_name
        ));
    }

    Ok(ResolvedRelease { owner: repo_ref.owner, repo: repo_ref.repo, tag: release.tag_name, assets })
}

/// Download one release asset's raw bytes via the GitHub API, which (unlike
/// the asset's public `browser_download_url`) works the same way whether the
/// repo is public or private, as long as `token` is set for the latter.
pub async fn download_asset(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    asset: &ReleaseAsset,
    token: Option<&str>,
) -> Result<Vec<u8>, String> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/assets/{}", asset.id);
    let mut request = client
        .get(&url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", "clouduler-self-update");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| format!("downloading {}: {e}", asset.name))?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("reading {} body: {e}", asset.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_repo_url_resolves_to_the_latest_release() {
        let repo_ref = parse_source("https://github.com/LiteApplication/clouduler").expect("parse");
        assert_eq!(repo_ref.owner, "LiteApplication");
        assert_eq!(repo_ref.repo, "clouduler");
        assert_eq!(repo_ref.tag, None);
    }

    #[test]
    fn a_trailing_slash_on_a_bare_repo_url_is_tolerated() {
        let repo_ref = parse_source("https://github.com/LiteApplication/clouduler/").expect("parse");
        assert_eq!(repo_ref.tag, None);
    }

    #[test]
    fn a_release_tag_url_pins_that_tag() {
        let repo_ref =
            parse_source("https://github.com/LiteApplication/clouduler/releases/tag/v0.1.0").expect("parse");
        assert_eq!(repo_ref.owner, "LiteApplication");
        assert_eq!(repo_ref.repo, "clouduler");
        assert_eq!(repo_ref.tag.as_deref(), Some("v0.1.0"));
    }

    #[test]
    fn a_non_github_url_is_rejected() {
        assert!(parse_source("https://gitlab.com/org/repo").is_err());
    }

    #[test]
    fn a_url_missing_the_repo_is_rejected() {
        assert!(parse_source("https://github.com/LiteApplication").is_err());
    }

    #[test]
    fn an_unrecognized_path_suffix_is_rejected() {
        assert!(parse_source("https://github.com/LiteApplication/clouduler/issues/1").is_err());
    }

    #[test]
    fn the_asset_name_matches_this_projects_release_convention() {
        assert_eq!(asset_name("x86_64"), "clouduler-x86_64-unknown-linux-gnu");
        assert_eq!(asset_name("aarch64"), "clouduler-aarch64-unknown-linux-gnu");
    }
}
