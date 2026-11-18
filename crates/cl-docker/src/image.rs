//! Image pulling and digest resolution.

use bollard::query_parameters::CreateImageOptionsBuilder;
use futures::StreamExt;

use crate::client::Docker;
use crate::error::{classify, DockerError, Result};

/// One local image as `GET /images/json` reports it, reduced to what cleanup needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalImage {
    /// The local image ID (`sha256:...`), which is what a container's
    /// `image_digest` holds; neither a tag nor a registry digest.
    pub id: String,
    pub repo_tags: Vec<String>,
    pub repo_digests: Vec<String>,
    pub created: i64,
    pub size: u64,
}

/// A parsed docker image reference: repository plus an optional tag or digest.
/// A reference has a tag XOR a digest, never both (docker doesn't allow both).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ImageRef {
    /// Parse a docker image reference like `nginx`, `nginx:1.25` or
    /// `nginx@sha256:abcd...`. Does not validate the repository name.
    pub fn parse(image: &str) -> Self {
        if let Some((repo, digest)) = image.split_once('@') {
            return ImageRef {
                repository: repo.to_string(),
                tag: None,
                digest: Some(digest.to_string()),
            };
        }
        // A colon after the last '/' is a tag; a colon before it (e.g. a registry
        // port like `host:5000/repo`) is not.
        match image.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => ImageRef {
                repository: repo.to_string(),
                tag: Some(tag.to_string()),
                digest: None,
            },
            _ => ImageRef {
                repository: image.to_string(),
                tag: None,
                digest: None,
            },
        }
    }

    pub fn is_latest(&self) -> bool {
        self.digest.is_none() && matches!(self.tag.as_deref(), None | Some("latest"))
    }

    /// The registry host this reference resolves to, as docker's credential store
    /// keys it: `docker.io` for anything unqualified.
    ///
    /// The first path segment is a registry only if it has a dot or colon or is
    /// `localhost`; otherwise `myteam/app` would be read as registry `myteam` and
    /// Docker Hub images would look for credentials under a nonexistent host.
    pub fn registry(&self) -> String {
        match self.repository.split_once('/') {
            Some((head, _)) if head.contains('.') || head.contains(':') || head == "localhost" => {
                head.to_string()
            }
            _ => DOCKER_HUB.to_string(),
        }
    }
}

/// How Docker Hub is spelled in a credential store (its api lives at
/// `registry-1.docker.io`).
pub const DOCKER_HUB: &str = "docker.io";

const RATE_LIMIT_RETRIES: u32 = 4;

/// First backoff after a 429, doubling each time. Deliberately short: registries'
/// `retry-after` is well under a millisecond, and the goal is to stay inside the
/// reverse proxy's response timeout.
const RATE_LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(150);

/// A registry login: the subset of bollard's `DockerCredentials` we store, so
/// nothing outside this crate depends on bollard.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RegistryCredentials {
    pub username: String,
    pub password: String,
    pub server: String,
}

impl std::fmt::Debug for RegistryCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("server", &self.server)
            .finish()
    }
}

impl From<&RegistryCredentials> for bollard::auth::DockerCredentials {
    fn from(creds: &RegistryCredentials) -> Self {
        bollard::auth::DockerCredentials {
            username: Some(creds.username.clone()),
            password: Some(creds.password.clone()),
            serveraddress: Some(creds.server.clone()),
            ..Default::default()
        }
    }
}

fn extract_digest(info: &bollard::models::ImageInspect) -> Option<String> {
    info.repo_digests.as_ref()?.iter().find_map(|repo_digest| {
        repo_digest
            .split_once('@')
            .map(|(_, digest)| digest.to_string())
    })
}

/// The digest `repository` published this image under.
///
/// One local image can carry several repo digests (pulled from two registries,
/// or retagged). Taking whichever comes first is wrong when comparing against
/// one repository's current digest: it would never match and the image would
/// read as perpetually out of date. Falls back to the first digest when nothing
/// matches (the common single-digest case).
fn digest_for_repository(info: &bollard::models::ImageInspect, repository: &str) -> Option<String> {
    let digests = info.repo_digests.as_ref()?;
    digests
        .iter()
        .find(|d| {
            d.split_once('@')
                .is_some_and(|(repo, _)| repo == repository)
        })
        .or_else(|| digests.first())
        .and_then(|d| d.split_once('@'))
        .map(|(_, digest)| digest.to_string())
}

/// `/images/create` pulls every tag when `fromImage` has neither tag nor digest,
/// instead of defaulting to `latest` like the `docker` CLI does client-side.
fn pull_reference(image: &str) -> String {
    let parsed = ImageRef::parse(image);
    if parsed.tag.is_none() && parsed.digest.is_none() {
        format!("{image}:latest")
    } else {
        image.to_string()
    }
}

impl Docker {
    /// `GET /images/{name}/json` as docker returned it, for relaying.
    pub async fn inspect_image_json(&self, image: &str) -> Result<serde_json::Value> {
        let info = self.inner.inspect_image(image).await.map_err(classify)?;
        serde_json::to_value(info).map_err(|e| DockerError::Other(e.to_string()))
    }

    pub async fn image_history_json(&self, image: &str) -> Result<serde_json::Value> {
        let history = self.inner.image_history(image).await.map_err(classify)?;
        serde_json::to_value(history).map_err(|e| DockerError::Other(e.to_string()))
    }

    pub async fn list_images_json(&self) -> Result<serde_json::Value> {
        let images = self
            .inner
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(classify)?;
        serde_json::to_value(images).map_err(|e| DockerError::Other(e.to_string()))
    }

    pub async fn list_local_images(&self) -> Result<Vec<LocalImage>> {
        let images = self
            .inner
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(classify)?;
        Ok(images
            .into_iter()
            .map(|i| LocalImage {
                id: i.id,
                repo_tags: i.repo_tags,
                repo_digests: i.repo_digests,
                created: i.created,
                size: i.size.max(0) as u64,
            })
            .collect())
    }

    /// The image ID of every container on the node, whoever created it.
    pub async fn all_container_image_ids(&self) -> Result<Vec<String>> {
        let options = bollard::query_parameters::ListContainersOptionsBuilder::new()
            .all(true)
            .build();
        let summaries = self
            .inner
            .list_containers(Some(options))
            .await
            .map_err(classify)?;
        Ok(summaries.into_iter().filter_map(|c| c.image_id).collect())
    }

    /// `DELETE /images/{name}` without force, so docker refuses an image a
    /// container uses. `name` is an image ID, or a `repo:tag` to untag that name.
    pub async fn remove_image(&self, name: &str) -> Result<()> {
        let options = bollard::query_parameters::RemoveImageOptionsBuilder::new()
            .force(false)
            .noprune(false)
            .build();
        self.inner
            .remove_image(name, Some(options), None)
            .await
            .map_err(classify)?;
        Ok(())
    }

    /// `GET /images/{name}/get`: a `docker save` tar, streamed because images are large.
    pub fn save_image(
        &self,
        image: &str,
    ) -> impl futures::Stream<Item = Result<bytes::Bytes>> + Send + 'static {
        self.inner
            .export_image(image)
            .map(|chunk| chunk.map_err(classify))
    }

    /// Pull `image` to completion and return the digest docker resolved it to.
    pub async fn pull(&self, image: &str) -> Result<String> {
        self.pull_as(image, None).await
    }

    /// `pull`, authenticating as `credentials`.
    ///
    /// Separate because most pulls are anonymous, and docker forwards credentials
    /// to whatever host the reference names, so sending them for another
    /// registry is worse than none.
    pub async fn pull_as(
        &self,
        image: &str,
        credentials: Option<&RegistryCredentials>,
    ) -> Result<String> {
        let options = CreateImageOptionsBuilder::new()
            .from_image(&pull_reference(image))
            .build();
        let mut stream = self
            .inner
            .create_image(Some(options), None, credentials.map(Into::into));
        while let Some(item) = stream.next().await {
            item.map_err(classify)?;
        }
        self.resolve_digest(image).await?.ok_or_else(|| {
            DockerError::NotFound(format!("image not found locally after pull: {image}"))
        })
    }

    /// Whether `image` is present locally. Unlike `resolve_digest`, an image with
    /// no repo digest (built or loaded locally) still counts.
    pub async fn image_exists(&self, image: &str) -> Result<bool> {
        match self.inner.inspect_image(image).await {
            Ok(_) => Ok(true),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(false),
                other => Err(other),
            },
        }
    }

    /// Inspect a local image and return its content digest. `Ok(None)` covers
    /// both "not present" and "no repo digest" (e.g. built locally).
    pub async fn resolve_digest(&self, image: &str) -> Result<Option<String>> {
        match self.inner.inspect_image(image).await {
            Ok(info) => Ok(extract_digest(&info)),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(None),
                other => Err(other),
            },
        }
    }

    /// The digest a local image carries for `repository`, looked up by image id.
    ///
    /// The Updates view needs the digest of the bits a container actually runs,
    /// not what the (usually moved) tag resolves to now. A container's
    /// `image_digest` is a local image id, so this is the way back to a registry
    /// content digest.
    pub async fn digest_of_image(
        &self,
        image_id: &str,
        repository: &str,
    ) -> Result<Option<String>> {
        match self.inner.inspect_image(image_id).await {
            Ok(info) => Ok(digest_for_repository(&info, repository)),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(None),
                other => Err(other),
            },
        }
    }

    /// What the registry currently serves for `image`, without pulling it.
    ///
    /// `Ok(None)` when the registry published no digest. An unreachable registry
    /// or refused credentials is an `Err`: "could not find out" must not collapse
    /// into "nothing newer", or images behind an expired login read as up to date.
    pub async fn registry_digest(
        &self,
        image: &str,
        credentials: Option<&RegistryCredentials>,
    ) -> Result<Option<String>> {
        // Retried: a cluster's worth of checks hits registries in quick succession
        // and they throttle bursts well under their published allowance. A 429 is
        // "too fast", not "no"; giving up would flip a current image between known
        // and unknown run to run.
        let mut delay = RATE_LIMIT_BACKOFF;
        for attempt in 0..RATE_LIMIT_RETRIES {
            match self
                .inner
                .inspect_registry_image(image, credentials.map(Into::into))
                .await
                .map_err(classify)
            {
                Ok(remote) => return Ok(remote.descriptor.digest),
                Err(DockerError::RateLimited(_)) if attempt + 1 < RATE_LIMIT_RETRIES => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("the loop returns on the last attempt")
    }

    /// Compare the local image digest against the registry's without pulling
    /// (the `:latest` update check in the web UI).
    pub async fn has_newer(&self, image: &str) -> Result<bool> {
        let local = self.resolve_digest(image).await?;
        match (local, self.registry_digest(image, None).await?) {
            (Some(local), Some(remote)) => Ok(local != remote),
            (None, Some(_)) => Ok(true),
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_repository() {
        let r = ImageRef::parse("nginx");
        assert_eq!(r.repository, "nginx");
        assert_eq!(r.tag, None);
        assert_eq!(r.digest, None);
        assert!(r.is_latest());
    }

    #[test]
    fn parses_tagged_repository() {
        let r = ImageRef::parse("nginx:1.25");
        assert_eq!(r.repository, "nginx");
        assert_eq!(r.tag, Some("1.25".to_string()));
        assert!(!r.is_latest());
    }

    #[test]
    fn parses_explicit_latest_tag() {
        let r = ImageRef::parse("nginx:latest");
        assert_eq!(r.tag, Some("latest".to_string()));
        assert!(r.is_latest());
    }

    #[test]
    fn parses_digest_reference() {
        let r = ImageRef::parse("nginx@sha256:deadbeef");
        assert_eq!(r.repository, "nginx");
        assert_eq!(r.digest, Some("sha256:deadbeef".to_string()));
        assert_eq!(r.tag, None);
        assert!(!r.is_latest());
    }

    #[test]
    fn pull_reference_makes_a_bare_image_explicit() {
        // `/images/create` without a tag pulls every tag. For `alpine` that hits a
        // years-old tag in the deprecated schema1 manifest format, which current
        // containerd rejects, so the pull never succeeds.
        assert_eq!(pull_reference("alpine"), "alpine:latest");
    }

    #[test]
    fn pull_reference_leaves_a_tagged_or_digested_image_alone() {
        assert_eq!(pull_reference("nginx:1.25"), "nginx:1.25");
        assert_eq!(
            pull_reference("nginx@sha256:deadbeef"),
            "nginx@sha256:deadbeef"
        );
        assert_eq!(pull_reference("nginx:latest"), "nginx:latest");
    }

    #[test]
    fn registry_port_is_not_mistaken_for_a_tag() {
        let r = ImageRef::parse("registry.local:5000/team/app");
        assert_eq!(r.repository, "registry.local:5000/team/app");
        assert_eq!(r.tag, None);
        assert!(r.is_latest());
    }

    #[test]
    fn registry_port_with_tag_splits_correctly() {
        let r = ImageRef::parse("registry.local:5000/team/app:2.0");
        assert_eq!(r.repository, "registry.local:5000/team/app");
        assert_eq!(r.tag, Some("2.0".to_string()));
    }

    #[test]
    fn extract_digest_finds_matching_repo_digest() {
        let info = bollard::models::ImageInspect {
            repo_digests: Some(vec!["nginx@sha256:abc123".to_string()]),
            ..Default::default()
        };
        assert_eq!(extract_digest(&info), Some("sha256:abc123".to_string()));
    }

    #[test]
    fn extract_digest_none_when_no_repo_digests() {
        let info = bollard::models::ImageInspect::default();
        assert_eq!(extract_digest(&info), None);
    }

    #[test]
    fn digest_for_repository_picks_the_matching_repository() {
        let info = bollard::models::ImageInspect {
            repo_digests: Some(vec![
                "mirror.internal/nginx@sha256:mirror".to_string(),
                "nginx@sha256:hub".to_string(),
            ]),
            ..Default::default()
        };
        assert_eq!(
            digest_for_repository(&info, "nginx"),
            Some("sha256:hub".to_string())
        );
    }

    #[test]
    fn digest_for_repository_falls_back_to_the_only_digest() {
        let info = bollard::models::ImageInspect {
            repo_digests: Some(vec!["nginx@sha256:abc".to_string()]),
            ..Default::default()
        };
        assert_eq!(
            digest_for_repository(&info, "somewhere/else"),
            Some("sha256:abc".to_string())
        );
    }

    #[test]
    fn unqualified_images_belong_to_docker_hub() {
        assert_eq!(ImageRef::parse("nginx").registry(), "docker.io");
        assert_eq!(ImageRef::parse("myteam/app:1.0").registry(), "docker.io");
    }

    #[test]
    fn a_host_like_first_segment_is_the_registry() {
        assert_eq!(
            ImageRef::parse("git.example.com/team/app:1.0").registry(),
            "git.example.com"
        );
        assert_eq!(
            ImageRef::parse("registry.local:5000/app").registry(),
            "registry.local:5000"
        );
        assert_eq!(ImageRef::parse("localhost/app").registry(), "localhost");
    }
}
