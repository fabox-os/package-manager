use std::fs;
use std::path::PathBuf;

use package_manager_core::GithubRepo;
use serde::{Deserialize, Serialize};

use crate::release::{Release, MAX_RESPONSE_BYTES};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cached {
    pub etag: Option<String>,
    pub release: Release,
}

/// One JSON file per repo in `~/.cache/package-manager/github-cache/`. Best effort: a missing
/// or corrupt file just means a normal request, and write failures are logged, not fatal.
pub struct DiskCache {
    dir: PathBuf,
}

impl DiskCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `GithubRepo::parse` only allows [A-Za-z0-9._-] and rejects `.`/`..`, so the repo can
    /// never escape the cache folder.
    fn file(&self, repo: &GithubRepo) -> PathBuf {
        // `@` cannot appear in an owner or repo name, so two repos never share a file.
        self.dir.join(format!("{}@{}.json", repo.owner, repo.repo))
    }

    pub fn get(&self, repo: &GithubRepo) -> Option<Cached> {
        let path = self.file(repo);
        if fs::metadata(&path).ok()?.len() as usize > MAX_RESPONSE_BYTES {
            return None;
        }
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    pub fn put(&self, repo: &GithubRepo, etag: Option<String>, release: &Release) {
        let cached = Cached {
            etag,
            release: release.clone(),
        };
        let result = fs::create_dir_all(&self.dir)
            .and_then(|()| serde_json::to_vec(&cached).map_err(Into::into))
            .and_then(|bytes| {
                // Write then rename so a crash never leaves a half-written file.
                let path = self.file(repo);
                let tmp = path.with_extension("json.tmp");
                fs::write(&tmp, bytes)?;
                fs::rename(tmp, path)
            });
        if let Err(e) = result {
            tracing::warn!("could not cache the {} release: {e}", repo.full_name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::parse_release;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("um-ghcache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn release() -> Release {
        parse_release(include_bytes!("../tests/fixtures/hydra-release.json")).expect("fixture")
    }

    #[test]
    fn round_trips_etag_and_release() {
        let d = dir("roundtrip");
        let cache = DiskCache::new(d.clone());
        let repo = GithubRepo::parse("hydralauncher/hydra").expect("repo");
        assert!(cache.get(&repo).is_none());
        cache.put(&repo, Some("W/\"abc\"".into()), &release());
        let got = cache.get(&repo).expect("cached");
        assert_eq!(got.etag.as_deref(), Some("W/\"abc\""));
        assert_eq!(got.release, release());
        assert!(!d.join("hydralauncher@hydra.json.tmp").exists());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_cache_files_are_ignored() {
        let d = dir("corrupt");
        fs::create_dir_all(&d).expect("mk");
        fs::write(d.join("a@b.json"), b"{not json").expect("write");
        let repo = GithubRepo::parse("a/b").expect("repo");
        assert!(DiskCache::new(d.clone()).get(&repo).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn different_repos_do_not_share_files() {
        let d = dir("separate");
        let cache = DiskCache::new(d.clone());
        let a = GithubRepo::parse("o/a").expect("a");
        let b = GithubRepo::parse("o/b").expect("b");
        cache.put(&a, None, &release());
        assert!(cache.get(&b).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sec_repos_whose_names_join_to_the_same_text_do_not_share_a_cache_file() {
        let d = dir("collide");
        let cache = DiskCache::new(d.clone());
        let a = GithubRepo::parse("a/b__c").expect("a");
        let b = GithubRepo::parse("a__b/c").expect("b");
        cache.put(&a, Some("etag-a".into()), &release());
        assert!(
            cache.get(&b).is_none(),
            "repo b must not see repo a's cached release"
        );
        let _ = fs::remove_dir_all(&d);
    }
}
