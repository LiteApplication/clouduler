//! Image pulling and digest resolution.

use bollard::query_parameters::CreateImageOptionsBuilder;
use futures::StreamExt;

use crate::client::Docker;
use crate::error::{classify, DockerError, Result};

/// A parsed docker image reference: repository plus an optional tag or digest.
/// A reference has a tag XOR a digest, never both (docker doesn't allow both).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ImageRef {
    /// Parse a docker image reference like `nginx`, `nginx:1.25`, or
    /// `nginx@sha256:abcd...`. Does not validate the repository name itself.
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

    /// True when this reference floats: no tag, or an explicit `:latest`.
    pub fn is_latest(&self) -> bool {
        self.digest.is_none() && matches!(self.tag.as_deref(), None | Some("latest"))
    }

    /// The registry host this reference resolves to, in the form docker's own
    /// credential store keys on: `docker.io` for anything unqualified.
    ///
    /// The first path segment is a registry only if it looks like a host —
    /// it contains a dot or a colon, or it is exactly `localhost`. Without
    /// that test `myteam/app` would be read as the registry `myteam`, and
    /// every unqualified Docker Hub image would go looking for credentials
    /// under a host that does not exist.
    pub fn registry(&self) -> String {
        match self.repository.split_once('/') {
            Some((head, _)) if head.contains('.') || head.contains(':') || head == "localhost" => {
                head.to_string()
            }
            _ => DOCKER_HUB.to_string(),
        }
    }
}

/// How Docker Hub is spelled in a credential store, as opposed to
/// `registry-1.docker.io` where its api actually lives.
pub const DOCKER_HUB: &str = "docker.io";

/// Attempts a rate-limited registry lookup gets, including the first.
const RATE_LIMIT_RETRIES: u32 = 4;

/// First backoff after a 429, doubling each time. Deliberately short: the
/// `retry-after` these registries return is well under a millisecond, so this
/// is already generous, and the whole point is to stay inside the reverse
/// proxy's response timeout rather than to be polite over minutes.
const RATE_LIMIT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(150);

/// A registry login. Mirrors the subset of bollard's `DockerCredentials` that
/// clouduler stores, so nothing outside this crate has to depend on bollard.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RegistryCredentials {
    pub username: String,
    pub password: String,
    /// Registry host, as `ImageRef::registry` spells it.
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
/// One local image can carry several repo digests — the same bits pulled from
/// two registries, or retagged — and `extract_digest` takes whichever comes
/// first. That is fine when the caller only wants *a* digest, and wrong when
/// it is about to compare against what one particular repository currently
/// serves: the answer would be a digest from some other repository, which
/// never matches, and the image reads as perpetually out of date. Falls back
/// to the first digest when nothing matches, since an image with exactly one
/// repo digest is the overwhelmingly common case.
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

/// Docker's `/images/create` endpoint pulls every tag in the repository when
/// `fromImage` carries neither a tag nor a digest, rather than defaulting to
/// `latest` the way the `docker` CLI does client-side before ever calling the
/// API.
fn pull_reference(image: &str) -> String {
    let parsed = ImageRef::parse(image);
    if parsed.tag.is_none() && parsed.digest.is_none() {
        format!("{image}:latest")
    } else {
        image.to_string()
    }
}

impl Docker {
    /// `GET /images/{name}/json` as docker returned it, for a caller that
    /// relays the answer rather than reading fields out of it.
    pub async fn inspect_image_json(&self, image: &str) -> Result<serde_json::Value> {
        let info = self.inner.inspect_image(image).await.map_err(classify)?;
        serde_json::to_value(info).map_err(|e| DockerError::Other(e.to_string()))
    }

    /// `GET /images/{name}/history` as docker returned it.
    pub async fn image_history_json(&self, image: &str) -> Result<serde_json::Value> {
        let history = self.inner.image_history(image).await.map_err(classify)?;
        serde_json::to_value(history).map_err(|e| DockerError::Other(e.to_string()))
    }

    /// `GET /images/json`: every local image.
    pub async fn list_images_json(&self) -> Result<serde_json::Value> {
        let images = self
            .inner
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(classify)?;
        serde_json::to_value(images).map_err(|e| DockerError::Other(e.to_string()))
    }

    /// `GET /images/{name}/get`: the image as a `docker save` tar. Streamed,
    /// since an image is routinely larger than anything worth holding in memory.
    pub fn save_image(
        &self,
        image: &str,
    ) -> impl futures::Stream<Item = Result<bytes::Bytes>> + Send + 'static {
        self.inner
            .export_image(image)
            .map(|chunk| chunk.map_err(classify))
    }

    /// Pull `image`, streaming progress to completion, then return the digest
    /// docker resolved it to locally.
    pub async fn pull(&self, image: &str) -> Result<String> {
        self.pull_as(image, None).await
    }

    /// `pull`, authenticating to the registry as `credentials`.
    ///
    /// Separate from `pull` rather than replacing it because most pulls are
    /// anonymous, and passing credentials for a registry the image does not
    /// come from is worse than passing none: docker forwards them to whatever
    /// host the reference names.
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

    /// Inspect a local image and return its content digest, if any. `Ok(None)`
    /// covers both "image not present locally" and "image has no repo digest"
    /// (e.g. it was built locally rather than pulled).
    pub async fn resolve_digest(&self, image: &str) -> Result<Option<String>> {
        match self.inner.inspect_image(image).await {
            Ok(info) => Ok(extract_digest(&info)),
            Err(e) => match classify(e) {
                DockerError::NotFound(_) => Ok(None),
                other => Err(other),
            },
        }
    }

    /// The digest a local image carries for `repository`, looked up by image
    /// id rather than by tag.
    ///
    /// The Updates view needs the digest of the bits a container is *actually*
    /// running, which is not the same question as what the tag resolves to
    /// now: the tag has usually moved on, which is the whole point of asking.
    /// A container's `image_digest` is a local image id, so this is the only
    /// way back to a registry content digest for it.
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
    /// `Ok(None)` when the registry answered but published no digest at all.
    /// A registry that cannot be reached, or that refuses the credentials, is
    /// an `Err`: "we could not find out" and "there is nothing newer" are
    /// different answers, and collapsing them would report every image behind
    /// an expired login as up to date.
    pub async fn registry_digest(
        &self,
        image: &str,
        credentials: Option<&RegistryCredentials>,
    ) -> Result<Option<String>> {
        // Retried, because checking a cluster's worth of images means asking
        // several registries a few dozen times in quick succession, and they
        // throttle bursts well under their published per-minute allowance. A
        // 429 is "too fast", not "no": giving up on it reports a perfectly
        // current image as unknown, and does so unpredictably, so the same
        // image flips between known and unknown run to run.
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

    /// Compare the local image digest against what the registry currently has,
    /// without pulling. Used by the `:latest` update check in the web UI.
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
        // Docker's own `/images/create` pulls every tag in the repository
        // when `fromImage` has none, rather than defaulting to `latest`. For
        // `alpine` that walks into a years-old tag still served in the
        // deprecated schema1 manifest format, which current containerd
        // rejects, so the pull never succeeds.
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
