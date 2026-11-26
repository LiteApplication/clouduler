//! Resolving a GitHub release and downloading its per-architecture asset.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Asset name convention this project's release workflow publishes under,
/// keyed by the same arch strings nodes report in `NodeState::arch`
/// (`std::env::consts::ARCH`, i.e. `x86_64` / `aarch64`).
fn asset_name(arch: &str) -> String {
    format!("clouduler-{arch}-unknown-linux-gnu")
}

/// Asset names of the ui bundle and the manifest tying the release's pieces together.
pub const UI_ASSET: &str = "clouduler-ui.zip";
pub const MANIFEST_ASSET: &str = "manifest.json";

/// The one manifest schema this code understands.
const MANIFEST_SCHEMA: u32 = 1;

/// `manifest.json`: the compatibility contract of a release. A ui is only ever served with the
/// backend of the same manifest, and the published hashes are what nodes verify downloads against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub tag: String,
    pub backend: BackendManifest,
    pub ui: UiManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendManifest {
    /// What `cl_config::version()` reports for these binaries.
    pub version: String,
    pub source_hash: String,
    /// arch (`x86_64` / `aarch64`) -> its binary.
    pub arch: BTreeMap<String, ManifestAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestAsset {
    pub asset: String,
    pub blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiManifest {
    pub version: String,
    pub source_hash: String,
    pub asset: String,
    pub blake3: String,
}

/// Parses and sanity-checks a `manifest.json`.
pub fn parse_manifest(raw: &[u8]) -> Result<Manifest, String> {
    let manifest: Manifest =
        serde_json::from_slice(raw).map_err(|e| format!("{MANIFEST_ASSET} is malformed: {e}"))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(format!(
            "{MANIFEST_ASSET} has schema {}, this version understands {MANIFEST_SCHEMA}",
            manifest.schema
        ));
    }
    Ok(manifest)
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
        (None, None, None) => Ok(RepoRef {
            owner,
            repo,
            tag: None,
        }),
        (Some("releases"), Some("tag"), Some(tag)) if !tag.is_empty() => Ok(RepoRef {
            owner,
            repo,
            tag: Some(tag.to_string()),
        }),
        _ => Err(format!(
            "{source} is neither a repo url nor a release tag url"
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub id: u64,
    pub name: String,
}

/// The release being rolled out, resolved once by the leader so every worker
/// downloads the same agreed asset even if a newer release lands mid-rollout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRelease {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    /// arch -> the asset built for it, for every arch this release published
    /// a binary for.
    pub assets: BTreeMap<String, ReleaseAsset>,
    /// The release's `manifest.json`; `None` for a release from before the ui was shipped
    /// separately (its binary embeds the ui).
    #[serde(default)]
    pub manifest: Option<Manifest>,
    /// The ui zip asset; always present when `manifest` is.
    #[serde(default)]
    pub ui: Option<ReleaseAsset>,
}

/// Picks the assets a release is rolled out with from the names and ids it published. With a
/// manifest every asset it names must exist; without one only the legacy per-arch binaries count.
fn pick_assets(
    published: &[(String, u64)],
    manifest: Option<&Manifest>,
) -> Result<(BTreeMap<String, ReleaseAsset>, Option<ReleaseAsset>), String> {
    let find = |name: &str| {
        published
            .iter()
            .find(|(n, _)| n == name)
            .map(|(n, id)| ReleaseAsset {
                id: *id,
                name: n.clone(),
            })
    };
    let mut assets = BTreeMap::new();
    let Some(manifest) = manifest else {
        for arch in ["x86_64", "aarch64"] {
            if let Some(asset) = find(&asset_name(arch)) {
                assets.insert(arch.to_string(), asset);
            }
        }
        return Ok((assets, None));
    };
    for (arch, entry) in &manifest.backend.arch {
        let asset = find(&entry.asset).ok_or_else(|| {
            format!(
                "release {} lists {} for {arch} but did not publish it",
                manifest.tag, entry.asset
            )
        })?;
        assets.insert(arch.clone(), asset);
    }
    let ui = find(&manifest.ui.asset).ok_or_else(|| {
        format!(
            "release {} lists {} but did not publish it",
            manifest.tag, manifest.ui.asset
        )
    })?;
    Ok((assets, Some(ui)))
}

/// `octocrab::Error::GitHub` displays as just "GitHub"; this surfaces the
/// wrapped reason (rate limited, not found, ...).
fn describe(e: &octocrab::Error) -> String {
    match e {
        octocrab::Error::GitHub { source, .. } => source.message.clone(),
        other => other.to_string(),
    }
}

/// Authenticated if `token` is set; anonymous access has a much lower rate limit.
fn client(token: Option<&str>) -> Result<octocrab::Octocrab, String> {
    let mut builder = octocrab::Octocrab::builder();
    if let Some(token) = token {
        builder = builder.personal_token(token.to_string());
    }
    builder
        .build()
        .map_err(|e| format!("building github client: {e}"))
}

/// Lookup timeout; octocrab sets none, so a stalled connection would hang.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a download may go without receiving a single byte before it is
/// abandoned. An idle limit rather than a total one: a large binary on a
/// slow link is fine as long as it keeps moving, a dead connection is not.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The http client asset downloads use: unlike the daemon's shared internal
/// client it fails a connection that stalls instead of waiting forever.
pub fn download_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(IDLE_TIMEOUT)
        .build()
        .map_err(|e| format!("building the download client: {e}"))
}

/// Stable (non-draft, non-prerelease) release tags of `source`'s repo, newest first.
pub async fn list_tags(source: &str, token: Option<&str>) -> Result<Vec<String>, String> {
    tokio::time::timeout(RESOLVE_TIMEOUT, async {
        let repo_ref = parse_source(source)?;
        let octocrab = client(token)?;
        let page = octocrab
            .repos(&repo_ref.owner, &repo_ref.repo)
            .releases()
            .list()
            .per_page(100)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "listing releases of {}/{}: {}",
                    repo_ref.owner,
                    repo_ref.repo,
                    describe(&e)
                )
            })?;
        Ok(page
            .items
            .into_iter()
            .filter(|r| !r.draft && !r.prerelease)
            .map(|r| r.tag_name)
            .collect())
    })
    .await
    .map_err(|_| {
        format!(
            "github did not answer within {}s",
            RESOLVE_TIMEOUT.as_secs()
        )
    })?
}

/// `source`'s repo pinned to `tag`, as a release tag url.
pub fn pinned_source(source: &str, tag: &str) -> Result<String, String> {
    let repo_ref = parse_source(source)?;
    Ok(format!(
        "https://github.com/{}/{}/releases/tag/{tag}",
        repo_ref.owner, repo_ref.repo
    ))
}

/// The arches in `arches` that `assets` has no binary for, sorted and deduplicated.
pub fn missing_arches<'a>(
    assets: &BTreeMap<String, ReleaseAsset>,
    arches: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut missing: Vec<String> = arches
        .into_iter()
        .filter(|arch| !assets.contains_key(*arch))
        .map(str::to_string)
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// Resolves `tag` of `source` exactly as a self-update run pinned to it would, and checks it has
/// an asset for every arch in `arches`. Lets a caller refuse before it changes anything.
pub async fn validate_pinned<'a>(
    source: &str,
    tag: &str,
    token: Option<&str>,
    arches: impl IntoIterator<Item = &'a str>,
) -> Result<ResolvedRelease, String> {
    let pinned = pinned_source(source, tag)?;
    let resolved = resolve(&pinned, token).await?;
    let missing = missing_arches(&resolved.assets, arches);
    if !missing.is_empty() {
        return Err(format!(
            "release {} has no binary for node arch {}",
            resolved.tag,
            missing.join(", ")
        ));
    }
    Ok(resolved)
}

/// Resolve `source` to its release and the assets published per architecture.
pub async fn resolve(source: &str, token: Option<&str>) -> Result<ResolvedRelease, String> {
    tokio::time::timeout(RESOLVE_TIMEOUT, resolve_inner(source, token))
        .await
        .map_err(|_| {
            format!(
                "github did not answer within {}s",
                RESOLVE_TIMEOUT.as_secs()
            )
        })?
}

async fn resolve_inner(source: &str, token: Option<&str>) -> Result<ResolvedRelease, String> {
    let repo_ref = parse_source(source)?;
    let octocrab = client(token)?;
    let repo_handler = octocrab.repos(&repo_ref.owner, &repo_ref.repo);
    let releases = repo_handler.releases();
    let release = match &repo_ref.tag {
        Some(tag) => releases.get_by_tag(tag).await.map_err(|e| {
            format!(
                "fetching release {tag} of {}/{}: {}",
                repo_ref.owner,
                repo_ref.repo,
                describe(&e)
            )
        })?,
        None => releases.get_latest().await.map_err(|e| {
            format!(
                "fetching the latest release of {}/{}: {}",
                repo_ref.owner,
                repo_ref.repo,
                describe(&e)
            )
        })?,
    };

    let published: Vec<(String, u64)> = release
        .assets
        .iter()
        .map(|a| (a.name.clone(), a.id.0))
        .collect();
    let manifest = match published.iter().find(|(n, _)| n == MANIFEST_ASSET) {
        Some((name, id)) => {
            let asset = ReleaseAsset {
                id: *id,
                name: name.clone(),
            };
            let client = download_client()?;
            let raw =
                download_asset(&client, &repo_ref.owner, &repo_ref.repo, &asset, token).await?;
            Some(parse_manifest(&raw)?)
        }
        None => None,
    };
    let (assets, ui) = pick_assets(&published, manifest.as_ref())?;
    if assets.is_empty() {
        return Err(format!(
            "release {} has no clouduler-*-unknown-linux-gnu assets",
            release.tag_name
        ));
    }

    Ok(ResolvedRelease {
        owner: repo_ref.owner,
        repo: repo_ref.repo,
        tag: release.tag_name,
        assets,
        manifest,
        ui,
    })
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
    let url = format!(
        "https://api.github.com/repos/{owner}/{repo}/releases/assets/{}",
        asset.id
    );
    let mut request = client
        .get(&url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", "clouduler-self-update");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("downloading {}: {e}", asset.name))?;
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
    fn missing_arches_lists_only_absent_ones() {
        let mut assets = BTreeMap::new();
        assets.insert(
            "x86_64".to_string(),
            ReleaseAsset {
                id: 1,
                name: asset_name("x86_64"),
            },
        );
        assert!(missing_arches(&assets, ["x86_64", "x86_64"]).is_empty());
        assert_eq!(
            missing_arches(&assets, ["aarch64", "x86_64", "aarch64"]),
            vec!["aarch64".to_string()]
        );
    }

    fn sample_manifest() -> Manifest {
        parse_manifest(
            br#"{"schema":1,"tag":"v0.20.0",
            "backend":{"version":"0.20.0","source_hash":"s","arch":{
              "x86_64":{"asset":"clouduler-x86_64-unknown-linux-gnu","blake3":"aa"},
              "aarch64":{"asset":"clouduler-aarch64-unknown-linux-gnu","blake3":"bb"}}},
            "ui":{"version":"0.20.0","source_hash":"u","asset":"clouduler-ui.zip","blake3":"cc"}}"#,
        )
        .expect("parse")
    }

    #[test]
    fn a_manifest_parses_and_a_foreign_schema_is_refused() {
        let manifest = sample_manifest();
        assert_eq!(manifest.backend.arch["aarch64"].blake3, "bb");
        assert_eq!(manifest.ui.blake3, "cc");
        assert!(parse_manifest(b"{}").is_err());
        let v2 = serde_json::to_vec(&Manifest {
            schema: 2,
            ..manifest
        })
        .unwrap();
        assert!(parse_manifest(&v2).unwrap_err().contains("schema 2"));
    }

    #[test]
    fn assets_follow_the_manifest_and_all_must_exist() {
        let manifest = sample_manifest();
        let published = vec![
            ("clouduler-x86_64-unknown-linux-gnu".to_string(), 1),
            ("clouduler-aarch64-unknown-linux-gnu".to_string(), 2),
            ("clouduler-ui.zip".to_string(), 3),
            ("manifest.json".to_string(), 4),
        ];
        let (assets, ui) = pick_assets(&published, Some(&manifest)).unwrap();
        assert_eq!(assets["aarch64"].id, 2);
        assert_eq!(ui.unwrap().id, 3);
        assert!(pick_assets(&published[..2], Some(&manifest)).is_err());
        assert!(pick_assets(&published[1..], Some(&manifest)).is_err());
    }

    #[test]
    fn a_release_without_a_manifest_keeps_the_legacy_assets_and_no_ui() {
        let published = vec![("clouduler-x86_64-unknown-linux-gnu".to_string(), 9)];
        let (assets, ui) = pick_assets(&published, None).unwrap();
        assert_eq!(assets.len(), 1);
        assert!(ui.is_none());
    }

    #[test]
    fn pinned_source_replaces_any_tag() {
        for src in [
            "https://github.com/A/b",
            "https://github.com/A/b/",
            "https://github.com/A/b/releases/tag/v0.1.0",
        ] {
            assert_eq!(
                pinned_source(src, "v0.17.3").unwrap(),
                "https://github.com/A/b/releases/tag/v0.17.3"
            );
        }
        assert!(pinned_source("https://example.com/A/b", "v1").is_err());
    }

    #[test]
    fn a_bare_repo_url_resolves_to_the_latest_release() {
        let repo_ref = parse_source("https://github.com/LiteApplication/clouduler").expect("parse");
        assert_eq!(repo_ref.owner, "LiteApplication");
        assert_eq!(repo_ref.repo, "clouduler");
        assert_eq!(repo_ref.tag, None);
    }

    #[test]
    fn a_trailing_slash_on_a_bare_repo_url_is_tolerated() {
        let repo_ref =
            parse_source("https://github.com/LiteApplication/clouduler/").expect("parse");
        assert_eq!(repo_ref.tag, None);
    }

    #[test]
    fn a_release_tag_url_pins_that_tag() {
        let repo_ref =
            parse_source("https://github.com/LiteApplication/clouduler/releases/tag/v0.1.0")
                .expect("parse");
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
