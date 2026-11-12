//! "Add from GitHub": turns something the user pasted (`owner/repo` or a github.com link)
//! into a previewed, explicitly confirmed link to a repository's `.deb` releases.
//!
//! Nothing is linked or installed here without the caller passing an explicit choice.

use std::sync::Arc;

use package_manager_core::exec::CommandRunner;
use package_manager_core::{
    is_valid_package_name, AppSource, GithubRepo, IconRef, InstalledApp, Provenance, TrackedApp,
    TrackedStore, UpdateCandidate, UpdateKind,
};

use crate::assets::{
    asset_kind, find_checksum_source, host_arch, pattern_from_asset, suggest_assets, AssetKind,
};
use crate::cache::DiskCache;
use crate::client::ReleaseFetcher;
use crate::error::GithubError;
use crate::presets::presets;
use crate::provenance::{github_provenance, ChecksumState};
use crate::release::is_github_https_url;

/// Version shown for an app that is being installed for the first time.
pub const NOT_INSTALLED: &str = "not installed";

// ----------------------------------------------------------------------- parsing input

/// Accepts `owner/repo`, `github.com/owner/repo`, `https://github.com/owner/repo`, and the
/// same followed by `/releases`, `/releases/latest` or `/releases/tag/<tag>` (what a person
/// copies from the browser). Anything else is refused with a plain explanation.
pub fn parse_repo_input(input: &str) -> Result<GithubRepo, GithubError> {
    let bad = |m: &str| GithubError::BadRepoInput(m.to_owned());
    let mut s = input.trim();
    if s.is_empty() {
        return Err(bad(
            "Paste a GitHub repository, like owner/repo or its releases link.",
        ));
    }
    if s.starts_with("http://") {
        return Err(bad("Only https:// links are accepted."));
    }
    s = s.strip_prefix("https://").unwrap_or(s);
    s = s
        .split(['?', '#'])
        .next()
        .unwrap_or(s)
        .trim_end_matches('/');
    s = s.strip_prefix("www.").unwrap_or(s);
    let mut parts = s.split('/');
    let first = parts.next().unwrap_or("");
    // A dot in the first part means it is a host name (GitHub user names cannot contain one).
    let owner = if first.contains('.') {
        if first != "github.com" {
            return Err(bad("Only github.com repositories are supported."));
        }
        parts.next().unwrap_or("")
    } else {
        first
    };
    let repo = parts.next().unwrap_or("");
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let rest: Vec<&str> = parts.collect();
    if !(rest.is_empty() || rest[0] == "releases") {
        return Err(bad(
            "Use the repository link, like github.com/owner/repo/releases.",
        ));
    }
    GithubRepo::parse(&format!("{owner}/{repo}"))
        .ok_or_else(|| bad("That does not look like a GitHub repository (owner/repo)."))
}

/// Best guess of the Debian package name from a `.deb` file name, which usually follows
/// `name_version_arch.deb` or `name-version-….deb`. Only a suggestion: the user can edit it,
/// and the installer refuses a file whose real package name differs.
pub fn suggest_package_name(asset_name: &str) -> String {
    let stem = asset_name.strip_suffix(".deb").unwrap_or(asset_name);
    let name = match stem.split_once('_') {
        Some((n, _)) => n,
        None => {
            // `tool-1.2.3-linux`: cut at the first `-` followed by a digit or `v<digit>`.
            let bytes: Vec<char> = stem.chars().collect();
            let cut = (0..bytes.len().saturating_sub(1)).find(|&i| {
                bytes[i] == '-'
                    && (bytes[i + 1].is_ascii_digit()
                        || (bytes[i + 1] == 'v'
                            && bytes.get(i + 2).is_some_and(char::is_ascii_digit)))
            });
            match cut {
                Some(i) => &stem[..stem.char_indices().nth(i).map_or(stem.len(), |(b, _)| b)],
                None => stem,
            }
        }
    };
    let name = name.to_lowercase();
    if is_valid_package_name(&name) {
        name
    } else {
        String::new()
    }
}

// ------------------------------------------------------------------------- preview

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetChoice {
    pub name: String,
    pub size: u64,
    pub url: String,
    /// Reusable pattern for future releases: the version is replaced by `{version}`.
    pub pattern: String,
    pub checksum: ChecksumState,
    /// Package name to suggest if the user picks this file (a preset's name wins).
    pub suggested_package: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPreview {
    pub repo: GithubRepo,
    pub tag: String,
    pub version: String,
    pub release_url: String,
    /// Start of the release notes, for context.
    pub notes: String,
    /// Installable `.deb` files for this CPU, best first. Never empty.
    pub choices: Vec<AssetChoice>,
    /// The repo is one of the shipped presets (its package name is known).
    pub preset_name: Option<String>,
    pub suggested_package: String,
}

impl RepoPreview {
    /// Trust the way it will be shown for the chosen file.
    pub fn provenance(&self, choice: usize) -> Provenance {
        let state = self
            .choices
            .get(choice)
            .map_or(ChecksumState::Unknown, |c| c.checksum);
        github_provenance(
            &self.repo.owner,
            &self.repo.repo,
            &self.release_url,
            self.preset_name.is_some(),
            state,
        )
    }
}

pub struct Linker {
    fetcher: Arc<dyn ReleaseFetcher>,
    cache: Option<DiskCache>,
    store: Arc<dyn TrackedStore>,
    runner: Box<dyn CommandRunner>,
}

impl Linker {
    pub fn new(
        fetcher: Arc<dyn ReleaseFetcher>,
        cache: Option<DiskCache>,
        store: Arc<dyn TrackedStore>,
        runner: Box<dyn CommandRunner>,
    ) -> Self {
        Self {
            fetcher,
            cache,
            store,
            runner,
        }
    }

    /// Looks up the latest release of what the user pasted and finds the matching `.deb`.
    pub async fn preview(&self, input: &str) -> Result<RepoPreview, GithubError> {
        let repo = parse_repo_input(input)?;
        let release =
            crate::fetch_cached(self.fetcher.as_ref(), self.cache.as_ref(), &repo).await?;
        let version = release.version().to_owned();

        let debs: Vec<_> = suggest_assets(&release.assets, host_arch())
            .into_iter()
            .filter(|a| asset_kind(&a.name) == Some(AssetKind::Deb))
            .collect();
        if debs.is_empty() {
            let others: Vec<&str> = suggest_assets(&release.assets, host_arch())
                .iter()
                .map(|a| a.name.as_str())
                .collect();
            let hint = if others.is_empty() {
                "The release has no downloads this app can install.".to_owned()
            } else {
                format!(
                    "Only {} found, and those cannot be installed yet.",
                    others.join(", ")
                )
            };
            return Err(GithubError::NoInstallablePackage {
                repo: repo.full_name(),
                tag: release.tag_name.clone(),
                hint,
            });
        }

        let preset = presets().iter().find(|p| p.repo().as_ref() == Some(&repo));
        let choices: Vec<AssetChoice> = debs
            .iter()
            .map(|a| AssetChoice {
                suggested_package: preset
                    .map(|p| p.package_name.clone())
                    .unwrap_or_else(|| suggest_package_name(&a.name)),
                name: a.name.clone(),
                size: a.size,
                url: a.browser_download_url.clone(),
                pattern: pattern_from_asset(&a.name, &version),
                checksum: match find_checksum_source(&release.assets, &a.name) {
                    Some(src) => ChecksumState::Published(src.kind),
                    None => ChecksumState::Missing,
                },
            })
            .collect();

        let suggested_package = choices[0].suggested_package.clone();
        let release_url = Some(release.html_url.clone())
            .filter(|u| is_github_https_url(u))
            .unwrap_or_else(|| format!("https://github.com/{}/releases", repo.full_name()));

        Ok(RepoPreview {
            repo,
            tag: release.tag_name.clone(),
            version,
            release_url,
            notes: release
                .body
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(400)
                .collect(),
            choices,
            preset_name: preset.map(|p| p.name.clone()),
            suggested_package,
        })
    }

    /// `Some(version)` if the package is installed.
    pub async fn installed_version(&self, package: &str) -> Option<String> {
        if !is_valid_package_name(package) {
            return None;
        }
        let out = self
            .runner
            .run(
                "dpkg-query",
                &["-W", "-f=${Version}\t${db:Status-Abbrev}\n", package],
            )
            .await
            .ok()
            .filter(|o| o.success)?;
        let (version, status) = out.stdout.lines().next()?.split_once('\t')?;
        (status.starts_with("ii") && !version.is_empty()).then(|| version.to_owned())
    }

    /// Saves the link. Updates for this package then come from this repository.
    pub async fn link(
        &self,
        preview: &RepoPreview,
        choice: usize,
        package: &str,
    ) -> Result<TrackedApp, GithubError> {
        let package = package.trim();
        if !is_valid_package_name(package) {
            return Err(GithubError::CannotLink(
                "The package name must be lowercase letters, digits and . + - (at least 2 characters).".into(),
            ));
        }
        let choice = preview
            .choices
            .get(choice)
            .ok_or_else(|| GithubError::CannotLink("Choose a download first.".into()))?;
        let tracked = TrackedApp {
            package_name: package.to_owned(),
            repo: preview.repo.clone(),
            asset_pattern: choice.pattern.clone(),
            is_preset: preview.preset_name.is_some(),
        };
        self.store
            .save(&tracked)
            .await
            .map_err(|e| GithubError::CannotLink(e.to_string()))?;
        Ok(tracked)
    }

    /// The update to run for a freshly linked app: install the previewed file. It goes
    /// through exactly the same verified download and installer as any update.
    pub fn install_candidate(
        &self,
        preview: &RepoPreview,
        choice: usize,
        tracked: &TrackedApp,
    ) -> Option<UpdateCandidate> {
        let c = preview.choices.get(choice)?;
        Some(UpdateCandidate {
            app: InstalledApp {
                id: format!("github:{}", tracked.repo.full_name()),
                name: tracked.package_name.clone(),
                version: NOT_INSTALLED.to_owned(),
                source: AppSource::Github {
                    repo: tracked.repo.clone(),
                    asset_pattern: tracked.asset_pattern.clone(),
                },
                provenance: preview.provenance(choice),
                icon: IconRef::initial_for(&tracked.package_name),
                desktop_file: None,
                homepage: Some(format!("https://github.com/{}", tracked.repo.full_name())),
                installed_at: None,
                is_system: false,
            },
            latest_version: preview.version.clone(),
            changelog_url: Some(preview.release_url.clone()),
            download_url: Some(c.url.clone()),
            size_bytes: Some(c.size),
            is_security: false,
            source_kind: UpdateKind::Github,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> String {
        parse_repo_input(input)
            .unwrap_or_else(|e| panic!("{input:?}: {e}"))
            .full_name()
    }

    #[test]
    fn accepts_every_form_a_person_would_paste() {
        for input in [
            "hydralauncher/hydra",
            "  hydralauncher/hydra  ",
            "github.com/hydralauncher/hydra",
            "https://github.com/hydralauncher/hydra",
            "https://github.com/hydralauncher/hydra/",
            "https://github.com/hydralauncher/hydra.git",
            "https://github.com/hydralauncher/hydra/releases",
            "https://github.com/hydralauncher/hydra/releases/latest",
            "https://github.com/hydralauncher/hydra/releases/tag/v4.4.0",
            "https://www.github.com/hydralauncher/hydra/releases?q=x#top",
        ] {
            assert_eq!(ok(input), "hydralauncher/hydra", "{input}");
        }
        assert_eq!(ok("some-user/my.repo_x"), "some-user/my.repo_x");
    }

    #[test]
    fn rejects_everything_else_with_a_reason() {
        for (input, needle) in [
            ("", "Paste a GitHub"),
            ("http://github.com/a/b", "https://"),
            ("https://gitlab.com/a/b", "github.com"),
            ("https://github.com.evil.test/a/b", "github.com"),
            ("evil.test/github.com/a/b", "github.com"),
            ("https://github.com/a", "owner/repo"),
            ("justone", "owner/repo"),
            ("https://github.com/a/b/issues/3", "repository link"),
            ("a/b/c", "repository link"),
            ("a/../b", "owner/repo"),
            ("../b", "github.com"),
            ("a/b c", "owner/repo"),
            ("a/b;rm -rf", "owner/repo"),
        ] {
            let err = parse_repo_input(input).expect_err(input).to_string();
            assert!(err.contains(needle), "{input:?} -> {err}");
        }
    }

    #[test]
    fn package_names_are_guessed_from_common_deb_naming() {
        for (file, want) in [
            ("hydralauncher_4.4.0_amd64.deb", "hydralauncher"),
            ("MyApp_1.0_amd64.deb", "myapp"),
            ("tool-1.2.3-linux-amd64.deb", "tool"),
            ("tool-v2.0.0.deb", "tool"),
            ("my-cool-tool-1.0.deb", "my-cool-tool"),
            ("single.deb", "single"),
            ("_1.0_amd64.deb", ""),
            ("a_1.0_amd64.deb", ""),
            ("Weird Name_1.0.deb", ""),
        ] {
            assert_eq!(suggest_package_name(file), want, "{file}");
        }
    }
}
