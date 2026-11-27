//! Best-effort download of the web ui for the binary init is installing.
//!
//! The ui ships as its own release asset, so a freshly initialised manager
//! would otherwise answer 503 with the "waiting" page until its first
//! self-update. When the cluster config names a `self_update.release_url`,
//! init fetches the ui that matches this binary's version and stages it where
//! the daemon looks for it. Every failure is a warning, never an init error.

use std::path::Path;

use cl_config::{ClusterConfig, NodeRole};
use cl_update::release::{self, ReleaseAsset, ResolvedRelease};

/// What init needs from GitHub; a trait so tests can answer without a network.
pub(crate) trait ReleaseFetcher {
    async fn resolve(&self, source: &str) -> Result<ResolvedRelease, String>;
    async fn download(
        &self,
        release: &ResolvedRelease,
        asset: &ReleaseAsset,
    ) -> Result<Vec<u8>, String>;
}

/// Anonymous: the optional GitHub token is a sealed cluster secret, which only
/// exists once the cluster is running.
pub(crate) struct GithubFetcher;

impl ReleaseFetcher for GithubFetcher {
    async fn resolve(&self, source: &str) -> Result<ResolvedRelease, String> {
        release::resolve(source, None).await
    }

    async fn download(
        &self,
        release: &ResolvedRelease,
        asset: &ReleaseAsset,
    ) -> Result<Vec<u8>, String> {
        let client = release::download_client()?;
        release::download_asset(&client, &release.owner, &release.repo, asset, None).await
    }
}

/// Resolves the release whose backend is `version` (the pinned tag `v<version>`
/// first, then whatever `release_url` points at), downloads its ui zip, checks it
/// against the manifest and stages it under `data_dir`.
pub(crate) async fn stage_release_ui<F: ReleaseFetcher>(
    fetcher: &F,
    release_url: &str,
    data_dir: &Path,
    version: &str,
) -> Result<(), String> {
    let resolved = release::resolve_backend_release(release_url, version, |s| async move {
        fetcher.resolve(&s).await
    })
    .await?;
    let (Some(manifest), Some(asset)) = (resolved.manifest.as_ref(), resolved.ui.as_ref()) else {
        return Err(format!("release {} has no ui asset", resolved.tag));
    };
    let bytes = fetcher.download(&resolved, asset).await?;
    cl_update::ui::stage_zip(data_dir, &bytes, &manifest.ui.blake3, version)
        .map_err(|e| format!("staging the ui: {e}"))?;
    Ok(())
}

/// The init step: returns the line to show the user, or `None` when there is
/// nothing to do (worker, `CL_UI_DIR` set).
pub(crate) async fn download_ui<F: ReleaseFetcher>(
    fetcher: &F,
    config: &ClusterConfig,
    node_name: &str,
    ui_dir_override: bool,
) -> Option<String> {
    let node = config.node(node_name)?;
    if node.role != NodeRole::Manager || ui_dir_override {
        return None;
    }
    let version = cl_config::version();
    let waiting = "the dashboard shows the waiting page until the first self-update";
    let Some(self_update) = config.self_update.as_ref() else {
        return Some(format!(
            "no self_update.release_url configured, so no web ui was downloaded; {waiting}"
        ));
    };
    match stage_release_ui(
        fetcher,
        &self_update.release_url,
        node.data_dir(config),
        version,
    )
    .await
    {
        Ok(()) => Some(format!("downloaded the web ui for {version}")),
        Err(e) => {
            tracing::warn!("web ui download failed: {e}");
            Some(format!("could not download the web ui ({e}); {waiting}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use cl_update::release::{BackendManifest, Manifest, UiManifest};

    use super::*;

    struct Fake {
        release: Result<ResolvedRelease, String>,
        zip: Vec<u8>,
    }

    impl ReleaseFetcher for Fake {
        async fn resolve(&self, _source: &str) -> Result<ResolvedRelease, String> {
            self.release.clone()
        }
        async fn download(&self, _: &ResolvedRelease, _: &ReleaseAsset) -> Result<Vec<u8>, String> {
            Ok(self.zip.clone())
        }
    }

    fn zip_bytes() -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("index.html", zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(b"<html>").unwrap();
        w.finish().unwrap().into_inner()
    }

    fn release_for(version: &str, ui_hash: &str) -> ResolvedRelease {
        ResolvedRelease {
            owner: "o".into(),
            repo: "r".into(),
            tag: format!("v{version}"),
            assets: Default::default(),
            manifest: Some(Manifest {
                schema: 1,
                tag: format!("v{version}"),
                backend: BackendManifest {
                    version: version.into(),
                    source_hash: String::new(),
                    arch: Default::default(),
                },
                ui: UiManifest {
                    version: version.into(),
                    source_hash: String::new(),
                    asset: release::UI_ASSET.into(),
                    blake3: ui_hash.into(),
                },
            }),
            ui: Some(ReleaseAsset {
                id: 1,
                name: release::UI_ASSET.into(),
            }),
        }
    }

    const URL: &str = "https://github.com/o/r";

    #[tokio::test]
    async fn stages_the_ui_for_this_version() {
        let zip = zip_bytes();
        let hash = blake3::hash(&zip).to_hex().to_string();
        let version = cl_config::version();
        let fake = Fake {
            release: Ok(release_for(version, &hash)),
            zip,
        };
        let dir = tempfile::tempdir().unwrap();
        stage_release_ui(&fake, URL, dir.path(), version)
            .await
            .unwrap();
        assert!(cl_update::ui::serving_dir(dir.path(), version).is_some());
    }

    #[tokio::test]
    async fn a_hash_mismatch_is_an_error_not_a_staged_ui() {
        let version = cl_config::version();
        let fake = Fake {
            release: Ok(release_for(version, "deadbeef")),
            zip: zip_bytes(),
        };
        let dir = tempfile::tempdir().unwrap();
        let err = stage_release_ui(&fake, URL, dir.path(), version)
            .await
            .unwrap_err();
        assert!(err.contains("staging the ui"), "{err}");
        assert!(cl_update::ui::serving_dir(dir.path(), version).is_none());
    }

    #[tokio::test]
    async fn a_release_of_another_backend_is_refused() {
        let fake = Fake {
            release: Ok(release_for("0.0.1-other", "x")),
            zip: vec![],
        };
        let dir = tempfile::tempdir().unwrap();
        let err = stage_release_ui(&fake, URL, dir.path(), "9.9.9")
            .await
            .unwrap_err();
        assert!(err.contains("not 9.9.9"), "{err}");
    }

    fn manager_config(url: Option<&str>) -> ClusterConfig {
        let mut config = crate::tests::founding_config();
        config.self_update = url.map(|u| cl_config::SelfUpdateConfig {
            release_url: u.into(),
            ..serde_yaml::from_str("release_url: x").unwrap()
        });
        config
    }

    #[tokio::test]
    async fn no_release_url_warns_and_does_not_fail() {
        let fake = Fake {
            release: Err("unused".into()),
            zip: vec![],
        };
        let config = manager_config(None);
        let name = config.nodes.keys().next().unwrap().clone();
        let line = download_ui(&fake, &config, &name, false).await.unwrap();
        assert!(line.contains("waiting page"), "{line}");
    }

    #[tokio::test]
    async fn an_offline_node_warns_and_does_not_fail() {
        let fake = Fake {
            release: Err("connection refused".into()),
            zip: vec![],
        };
        let config = manager_config(Some(URL));
        let name = config.nodes.keys().next().unwrap().clone();
        let line = download_ui(&fake, &config, &name, false).await.unwrap();
        assert!(line.contains("connection refused") && line.contains("waiting page"));
    }

    #[tokio::test]
    async fn skipped_when_the_ui_dir_is_overridden() {
        let fake = Fake {
            release: Err("unused".into()),
            zip: vec![],
        };
        let config = manager_config(Some(URL));
        let name = config.nodes.keys().next().unwrap().clone();
        assert!(download_ui(&fake, &config, &name, true).await.is_none());
    }
}
