//! Fetches the ui for the backend that is already running.
//!
//! Self-update only stages a ui while it installs a release. A node that was updated by a binary
//! from before the ui was split out (or that lost its staged ui) would otherwise show the waiting
//! page until the next release, so the worker calls this whenever a manager has none to serve.

use std::path::Path;

use crate::{release, ui};

/// Resolves the release of backend `version` (the pinned tag `v<version>` first, then whatever
/// `release_url` points at), downloads its ui zip and stages it, paired with `version`.
pub async fn fetch_running_ui(
    release_url: &str,
    token: Option<&str>,
    data_dir: &Path,
    version: &str,
) -> Result<(), String> {
    let mut candidates = Vec::new();
    if let Ok(pinned) = release::pinned_source(release_url, &format!("v{version}")) {
        candidates.push(pinned);
    }
    candidates.push(release_url.to_string());

    let mut last_error = String::new();
    let mut found = None;
    for source in &candidates {
        match release::resolve(source, token).await {
            Ok(resolved) => match resolved.manifest.as_ref() {
                Some(m) if m.backend.version == version => {
                    found = Some(resolved);
                    break;
                }
                Some(m) => {
                    last_error = format!(
                        "release {} is backend {}, not {version}",
                        resolved.tag, m.backend.version
                    )
                }
                None => last_error = format!("release {} has no manifest.json", resolved.tag),
            },
            Err(e) => last_error = e,
        }
    }
    let resolved = found.ok_or(last_error)?;
    let (Some(manifest), Some(asset)) = (resolved.manifest.as_ref(), resolved.ui.as_ref()) else {
        return Err(format!("release {} has no ui asset", resolved.tag));
    };
    let http = release::download_client()?;
    let zip = release::download_asset(&http, &resolved.owner, &resolved.repo, asset, token).await?;
    let (dir, expect, version) = (
        data_dir.to_path_buf(),
        manifest.ui.blake3.clone(),
        version.to_string(),
    );
    tokio::task::spawn_blocking(move || ui::stage_zip(&dir, &zip, &expect, &version))
        .await
        .map_err(|e| format!("staging the ui: {e}"))?
        .map_err(|e| format!("staging the ui: {e}"))?;
    Ok(())
}
