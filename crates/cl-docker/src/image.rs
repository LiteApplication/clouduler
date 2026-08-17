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
}

fn extract_digest(info: &bollard::models::ImageInspect) -> Option<String> {
    info.repo_digests.as_ref()?.iter().find_map(|repo_digest| {
        repo_digest
            .split_once('@')
            .map(|(_, digest)| digest.to_string())
    })
}

impl Docker {
    /// Pull `image`, streaming progress to completion, then return the digest
    /// docker resolved it to locally.
    pub async fn pull(&self, image: &str) -> Result<String> {
        let options = CreateImageOptionsBuilder::new().from_image(image).build();
        let mut stream = self.inner.create_image(Some(options), None, None);
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

    /// Compare the local image digest against what the registry currently has,
    /// without pulling. Used by the `:latest` update check in the web UI.
    pub async fn has_newer(&self, image: &str) -> Result<bool> {
        let local = self.resolve_digest(image).await?;
        let remote = self
            .inner
            .inspect_registry_image(image, None)
            .await
            .map_err(classify)?;
        let remote_digest = remote.descriptor.digest;
        match (local, remote_digest) {
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
}
