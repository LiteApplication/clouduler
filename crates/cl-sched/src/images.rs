//! Disk cleanup for docker images this node pulled for stack services.
//!
//! Nothing else ever removes an image, so every update would otherwise leave
//! the old one behind on every node. The rule, per repository a stack service
//! uses on this node:
//!
//! - an image any container uses (running or stopped, clouduler's or not) is
//!   kept;
//! - an image a service on this node is configured with right now (by tag or
//!   by pinned digest) is kept: it is the one running or the one about to run;
//! - of the remaining images of that repository, the newest is kept as the
//!   rollback target and the rest are removed.
//!
//! So after a successful update the new image and the one just before it stay.
//! A failed update keeps both, because cleanup does not run while an update on
//! this node is still in flight and the rule above never touches the pair.
//!
//! A repository this node has stopped wanting (its stack was removed or moved
//! away) is dropped entirely, but only after `RETIRE_AFTER_SECS`. Which
//! repositories clouduler pulled is remembered in a small file in the node's
//! data directory: an image of a repository that was never a stack service
//! here is never touched.
//!
//! Images are removed by ID, without force, so docker refuses one that is in
//! use. An image that carries several tags is untagged name by name instead,
//! because docker refuses to delete such an image by ID without force.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cl_docker::{Docker, ImageRef, LocalImage};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// How often the periodic sweep runs, besides once at daemon start.
pub const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a repository must have gone unwanted on this node before all its
/// unused images are removed.
pub const RETIRE_AFTER_SECS: i64 = 7 * 24 * 60 * 60;

const KNOWN_FILE: &str = "image-repos.json";

/// Repository name as docker prints it in `RepoTags`: the Docker Hub host and
/// the implicit `library/` namespace are dropped.
pub fn normalize_repo(repo: &str) -> String {
    let repo = repo
        .strip_prefix("index.docker.io/")
        .or_else(|| repo.strip_prefix("registry-1.docker.io/"))
        .or_else(|| repo.strip_prefix("docker.io/"))
        .unwrap_or(repo);
    repo.strip_prefix("library/").unwrap_or(repo).to_string()
}

/// The repository of a `repo:tag`, `repo@sha256:..` or bare `repo` reference.
pub fn repo_of(reference: &str) -> String {
    normalize_repo(&ImageRef::parse(reference).repository)
}

/// What a service is configured to run, in the two forms docker reports an
/// image under.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// `repo:tag`, normalized.
    pub tags: BTreeSet<String>,
    /// `repo@sha256:..`, normalized.
    pub digests: BTreeSet<String>,
    /// Repositories wanted without a specific version, such as the one a
    /// clouduler container on the node is running right now.
    pub extra_repos: BTreeSet<String>,
}

impl Wanted {
    pub fn add(&mut self, image: &str) {
        let parsed = ImageRef::parse(image);
        let repo = normalize_repo(&parsed.repository);
        match parsed.digest {
            Some(digest) => {
                self.digests.insert(format!("{repo}@{digest}"));
            }
            None => {
                let tag = parsed.tag.unwrap_or_else(|| "latest".to_string());
                self.tags.insert(format!("{repo}:{tag}"));
            }
        }
    }

    pub fn repos(&self) -> BTreeSet<String> {
        self.tags
            .iter()
            .map(|t| {
                t.rsplit_once(':')
                    .map_or(t.as_str(), |(r, _)| r)
                    .to_string()
            })
            .chain(
                self.digests
                    .iter()
                    .map(|d| d.split('@').next().unwrap_or(d).to_string()),
            )
            .chain(self.extra_repos.iter().cloned())
            .collect()
    }
}

/// One image the sweep decided to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub id: String,
    /// Names to untag one by one; empty means remove by `id`.
    pub untag: Vec<String>,
    pub repo: String,
    pub size: u64,
}

fn names_of(image: &LocalImage) -> Vec<String> {
    image
        .repo_tags
        .iter()
        .filter(|t| !t.starts_with("<none>"))
        .map(|t| normalize_repo_ref(t))
        .collect()
}

fn normalize_repo_ref(reference: &str) -> String {
    match reference.split_once('@') {
        Some((repo, digest)) => format!("{}@{digest}", normalize_repo(repo)),
        None => match reference.rsplit_once(':') {
            Some((repo, tag)) if !tag.contains('/') => format!("{}:{tag}", normalize_repo(repo)),
            _ => normalize_repo(reference),
        },
    }
}

/// Every repository an image is known under.
fn repos_of_image(image: &LocalImage) -> BTreeSet<String> {
    image
        .repo_tags
        .iter()
        .chain(&image.repo_digests)
        .filter(|r| !r.starts_with("<none>"))
        .map(|r| repo_of(r))
        .collect()
}

/// Which images to delete. Pure: everything it needs is passed in.
///
/// - `used`: image IDs of every container on the node, any state, any owner.
/// - `wanted`: what this node's services are configured to run.
/// - `retired`: repositories to clear out completely (past the grace period).
///
/// A repository is only touched when it is wanted or retired, and an image is
/// only touched when every repository it is known under is. Images with no
/// name at all (neither tag nor digest) are never touched. An image that
/// lost its tag to a newer one but keeps its `repo@sha256` digest still counts
/// as a version of that repository.
pub fn images_to_remove(
    images: &[LocalImage],
    used: &BTreeSet<String>,
    wanted: &Wanted,
    retired: &BTreeSet<String>,
) -> Vec<Removal> {
    let wanted_repos = wanted.repos();
    let eligible: BTreeSet<&String> = wanted_repos.iter().chain(retired).collect();

    // image id -> (repo it is filed under, pinned)
    let mut by_repo: BTreeMap<String, Vec<&LocalImage>> = BTreeMap::new();
    for image in images {
        let repos = repos_of_image(image);
        if repos.is_empty() || !repos.iter().all(|r| eligible.contains(r)) {
            continue;
        }
        let pinned = used.contains(&image.id)
            || names_of(image).iter().any(|t| wanted.tags.contains(t))
            || image
                .repo_digests
                .iter()
                .any(|d| wanted.digests.contains(&normalize_repo_ref(d)));
        if pinned {
            continue;
        }
        // Filed under its first repository; every one of them is eligible.
        let repo = repos.into_iter().next().unwrap_or_default();
        by_repo.entry(repo).or_default().push(image);
    }

    let mut out = Vec::new();
    for (repo, mut candidates) in by_repo {
        // Newest first; the ID breaks ties so the choice is stable.
        candidates.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
        let skip = usize::from(!retired.contains(&repo));
        for image in candidates.into_iter().skip(skip) {
            let untag = if image.repo_tags.len() > 1 {
                image.repo_tags.clone()
            } else {
                Vec::new()
            };
            out.push(Removal {
                id: image.id.clone(),
                untag,
                repo: repo.clone(),
                size: image.size,
            });
        }
    }
    out
}

/// Repositories remembered as pulled for a stack service, by the unix second
/// they were last wanted on this node.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Known {
    repos: BTreeMap<String, i64>,
}

fn load_known(path: &Path) -> Known {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_known(path: &Path, known: &Known) {
    let write = || -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec(known).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        warn!(error = %e, "could not remember which image repositories this node pulled");
    }
}

/// Repositories to retire: remembered, not wanted now, and unwanted for longer
/// than the grace period.
pub fn retired_repos(
    known: &BTreeMap<String, i64>,
    wanted: &BTreeSet<String>,
    now: i64,
) -> BTreeSet<String> {
    known
        .iter()
        .filter(|(repo, last)| !wanted.contains(*repo) && now - **last >= RETIRE_AFTER_SECS)
        .map(|(repo, _)| repo.clone())
        .collect()
}

/// Scheduling state for image cleanup on this node.
#[derive(Clone)]
pub struct ImageGc {
    inner: Arc<Inner>,
}

struct Inner {
    known_path: PathBuf,
    running: AtomicBool,
    state: Mutex<GcState>,
}

struct GcState {
    last_sweep: Option<Instant>,
    /// Finished updates (instance, record time) already followed by a sweep.
    cleaned: BTreeSet<(String, i64)>,
}

impl ImageGc {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            inner: Arc::new(Inner {
                known_path: data_dir.join(KNOWN_FILE),
                running: AtomicBool::new(false),
                state: Mutex::new(GcState {
                    last_sweep: None,
                    cleaned: BTreeSet::new(),
                }),
            }),
        }
    }

    /// Whether a sweep is due, and if so claim it. `succeeded` lists this
    /// node's updates that just finished successfully (instance, record
    /// time); `busy` means an update or pull is still in flight, which defers
    /// everything so a failed or running update keeps both images.
    pub fn due(&self, succeeded: &[(String, i64)], busy: bool) -> bool {
        if busy {
            return false;
        }
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = succeeded.iter().any(|s| !state.cleaned.contains(s));
        let periodic = state.last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY);
        if !(fresh || periodic) {
            return false;
        }
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        state.last_sweep = Some(Instant::now());
        state.cleaned = succeeded.iter().cloned().collect();
        true
    }

    /// Run one sweep. Never fails: problems are logged. Must follow a `true`
    /// from `due`.
    pub async fn sweep(
        &self,
        docker: &Docker,
        wanted: &Wanted,
        managed_repos: &BTreeSet<String>,
        now: i64,
    ) {
        self.sweep_inner(docker, wanted, managed_repos, now).await;
        self.inner.running.store(false, Ordering::SeqCst);
    }

    async fn sweep_inner(
        &self,
        docker: &Docker,
        wanted: &Wanted,
        managed_repos: &BTreeSet<String>,
        now: i64,
    ) {
        let mut known = load_known(&self.inner.known_path);
        let wanted_repos = wanted.repos();
        for repo in wanted_repos.iter().chain(managed_repos) {
            known.repos.insert(repo.clone(), now);
        }
        let retired = retired_repos(&known.repos, &wanted_repos, now);
        // A repository a managed container still uses is wanted, whatever the
        // service list says.
        let retired: BTreeSet<String> = retired
            .into_iter()
            .filter(|r| !managed_repos.contains(r))
            .collect();

        let (images, used) = match (
            docker.list_local_images().await,
            docker.all_container_image_ids().await,
        ) {
            (Ok(images), Ok(used)) => (images, used.into_iter().collect::<BTreeSet<_>>()),
            (Err(e), _) | (_, Err(e)) => {
                warn!(error = %e, "image cleanup skipped: cannot list images or containers");
                return;
            }
        };

        let mut effective = wanted.clone();
        effective.extra_repos.extend(managed_repos.iter().cloned());
        let plan = images_to_remove(&images, &used, &effective, &retired);

        let mut removed = 0usize;
        let mut freed = 0u64;
        for removal in &plan {
            match remove_one(docker, removal).await {
                Ok(()) => {
                    removed += 1;
                    freed += removal.size;
                    info!(image = %removal.id, repo = %removal.repo, size = removal.size, "removed old image");
                }
                Err(e) => {
                    warn!(image = %removal.id, repo = %removal.repo, error = %e, "could not remove old image, leaving it");
                }
            }
        }
        if removed > 0 {
            info!(removed, freed, "image cleanup finished");
        }

        for repo in &retired {
            known.repos.remove(repo);
        }
        save_known(&self.inner.known_path, &known);
    }
}

async fn remove_one(docker: &Docker, removal: &Removal) -> cl_docker::Result<()> {
    if removal.untag.is_empty() {
        return docker.remove_image(&removal.id).await;
    }
    for name in &removal.untag {
        docker.remove_image(name).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str, tags: &[&str], digests: &[&str], created: i64) -> LocalImage {
        LocalImage {
            id: id.to_string(),
            repo_tags: tags.iter().map(|s| s.to_string()).collect(),
            repo_digests: digests.iter().map(|s| s.to_string()).collect(),
            created,
            size: 100,
        }
    }

    fn wanted(images: &[&str]) -> Wanted {
        let mut w = Wanted::default();
        for i in images {
            w.add(i);
        }
        w
    }

    fn ids(removals: &[Removal]) -> Vec<&str> {
        removals.iter().map(|r| r.id.as_str()).collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keeps_current_and_the_one_before_and_removes_older() {
        let images = [
            image("sha256:v1", &["app:1"], &[], 100),
            image("sha256:v2", &["app:2"], &[], 200),
            image("sha256:v3", &["app:3"], &[], 300),
            image("sha256:v4", &["app:4"], &[], 400),
        ];
        // v4 is running and configured; v3 is the rollback target.
        let plan = images_to_remove(
            &images,
            &set(&["sha256:v4"]),
            &wanted(&["app:4"]),
            &set(&[]),
        );
        assert_eq!(ids(&plan), ["sha256:v2", "sha256:v1"]);
    }

    #[test]
    fn an_image_used_by_any_container_is_never_removed() {
        let images = [
            image("sha256:v1", &["app:1"], &[], 100),
            image("sha256:v2", &["app:2"], &[], 200),
            image("sha256:v3", &["app:3"], &[], 300),
        ];
        // A stopped container (maybe not clouduler's) still uses v1.
        let used = set(&["sha256:v1", "sha256:v3"]);
        let plan = images_to_remove(&images, &used, &wanted(&["app:3"]), &set(&[]));
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn the_configured_next_version_is_kept_before_it_runs() {
        let images = [
            image("sha256:old", &["app:1"], &[], 100),
            image("sha256:cur", &["app:2"], &[], 200),
            image("sha256:next", &["app:3"], &[], 300),
        ];
        let plan = images_to_remove(
            &images,
            &set(&["sha256:cur"]),
            &wanted(&["app:3"]),
            &set(&[]),
        );
        // cur (in use) and next (configured) stay; old is the newest spare.
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn unrelated_repositories_are_never_touched() {
        let images = [
            image("sha256:a", &["redis:6"], &[], 100),
            image("sha256:b", &["redis:7"], &[], 200),
            image("sha256:c", &["app:1"], &[], 100),
            image("sha256:d", &["app:2"], &[], 200),
        ];
        let plan = images_to_remove(&images, &set(&["sha256:d"]), &wanted(&["app:2"]), &set(&[]));
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn an_image_shared_with_an_unrelated_repository_is_left_alone() {
        let images = [
            image("sha256:a", &["app:1", "other:1"], &[], 100),
            image("sha256:b", &["app:2"], &[], 200),
            image("sha256:c", &["app:3"], &[], 300),
        ];
        let plan = images_to_remove(&images, &set(&["sha256:c"]), &wanted(&["app:3"]), &set(&[]));
        assert_eq!(ids(&plan), Vec::<&str>::new().as_slice());
    }

    #[test]
    fn multi_tag_images_are_untagged_by_name() {
        let images = [
            image("sha256:a", &["app:1", "app:one"], &[], 100),
            image("sha256:b", &["app:2"], &[], 200),
            image("sha256:c", &["app:3"], &[], 300),
        ];
        let plan = images_to_remove(&images, &set(&["sha256:c"]), &wanted(&["app:3"]), &set(&[]));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].id, "sha256:a");
        assert_eq!(plan[0].untag, ["app:1", "app:one"]);
    }

    #[test]
    fn digest_pinned_services_match_repo_digests() {
        let images = [
            image("sha256:a", &[], &["ghcr.io/x/app@sha256:aaa"], 100),
            image("sha256:b", &[], &["ghcr.io/x/app@sha256:bbb"], 200),
            image("sha256:c", &[], &["ghcr.io/x/app@sha256:ccc"], 300),
        ];
        let plan = images_to_remove(
            &images,
            &set(&["sha256:c"]),
            &wanted(&["ghcr.io/x/app@sha256:ccc"]),
            &set(&[]),
        );
        assert_eq!(ids(&plan), ["sha256:a"]);
    }

    #[test]
    fn docker_hub_names_are_normalized() {
        let images = [
            image("sha256:a", &["nginx:1"], &[], 100),
            image("sha256:b", &["nginx:2"], &[], 200),
            image("sha256:c", &["nginx:3"], &[], 300),
        ];
        let plan = images_to_remove(
            &images,
            &set(&["sha256:c"]),
            &wanted(&["docker.io/library/nginx:3"]),
            &set(&[]),
        );
        assert_eq!(ids(&plan), ["sha256:a"]);
        assert_eq!(repo_of("library/nginx"), "nginx");
        assert_eq!(repo_of("nginx"), "nginx");
        assert_eq!(repo_of("localhost:5000/app:1"), "localhost:5000/app");
    }

    #[test]
    fn a_moved_tag_keeps_the_previous_digest_only_image_as_rollback() {
        let images = [
            // `latest` now points at the new image.
            image("sha256:new", &["app:latest"], &["app@sha256:n"], 300),
            // The previous one lost its tag but keeps its digest.
            image("sha256:prev", &[], &["app@sha256:p"], 200),
            image("sha256:older", &[], &["app@sha256:o"], 100),
        ];
        let plan = images_to_remove(
            &images,
            &set(&["sha256:new"]),
            &wanted(&["app:latest"]),
            &set(&[]),
        );
        assert_eq!(ids(&plan), ["sha256:older"]);

        let two = &images[..2];
        let plan = images_to_remove(
            two,
            &set(&["sha256:new"]),
            &wanted(&["app:latest"]),
            &set(&[]),
        );
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn nameless_images_are_never_touched() {
        let images = [image("sha256:a", &[], &[], 100)];
        let plan = images_to_remove(&images, &set(&[]), &wanted(&["app:1"]), &set(&[]));
        assert!(plan.is_empty());
    }

    #[test]
    fn a_retired_repository_is_cleared_except_for_images_in_use() {
        let images = [
            image("sha256:a", &["gone:1"], &[], 100),
            image("sha256:b", &["gone:2"], &[], 200),
            image("sha256:c", &["gone:3"], &[], 300),
        ];
        let plan = images_to_remove(
            &images,
            &set(&["sha256:c"]),
            &Wanted::default(),
            &set(&["gone"]),
        );
        assert_eq!(ids(&plan), ["sha256:b", "sha256:a"]);
    }

    #[test]
    fn repositories_retire_only_after_the_grace_period() {
        let mut known = BTreeMap::new();
        known.insert("old".to_string(), 0);
        known.insert("recent".to_string(), RETIRE_AFTER_SECS);
        known.insert("wanted".to_string(), 0);
        let now = RETIRE_AFTER_SECS + 10;
        let retired = retired_repos(&known, &set(&["wanted"]), now);
        assert_eq!(retired, set(&["old"]));
    }
}
