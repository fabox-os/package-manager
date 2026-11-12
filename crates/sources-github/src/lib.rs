//! GitHub Bridge: checks releases of repositories the user linked and finds the matching
//! download. Installing is added in a later slice.

mod assets;
mod cache;
mod client;
mod download;
mod error;
mod linker;
mod presets;
mod provenance;
mod release;

pub use assets::{
    asset_kind, find_checksum_source, host_arch, matches_pattern, parse_checksum,
    pattern_from_asset, select_asset, suggest_assets, AssetKind, ChecksumKind, ChecksumSource,
    Digest,
};
pub use cache::DiskCache;
pub use client::{classify_failure, Fetched, HttpFetcher, ReleaseFetcher};
pub use download::{
    digest_args, file_digest, has_deb_magic, prune_old_versions, safe_component, sha256_file,
    Downloader, HttpDownloader, MAX_CHECKSUM_BYTES,
};
pub use error::GithubError;
pub use linker::{
    parse_repo_input, suggest_package_name, AssetChoice, Linker, RepoPreview, NOT_INSTALLED,
};
pub use presets::{is_preset_repo, preset_for_package, presets, Preset};
pub use provenance::{github_provenance, ChecksumState};
pub use release::{is_github_https_url, parse_release, Asset, Release};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use package_manager_core::exec::{CommandRunner, PKEXEC};
use package_manager_core::version::is_newer;
use package_manager_core::{
    is_valid_package_name, AppSource, ApplyReport, GithubRepo, IconRef, InstalledApp,
    ProgressEvent, ProgressSender, SourceError, TrackedApp, TrackedStore, UpdateCandidate,
    UpdateKind, UpdateSource,
};

/// The latest release via an ETag-validated disk cache. A 304 uses the cached copy; offline
/// with a cached copy still answers (stale data beats an empty screen).
pub(crate) async fn fetch_cached(
    fetcher: &dyn ReleaseFetcher,
    cache: Option<&DiskCache>,
    repo: &GithubRepo,
) -> Result<Release, GithubError> {
    scope_to_repo(fetch_unscoped(fetcher, cache, repo).await?, repo)
}

/// Keeps only downloads hosted under `github.com/<owner>/<repo>/releases/download/`. The
/// provenance shown to the user names this repository, so a download from anywhere else (a
/// lying response, a moved or renamed repo) is never used.
fn scope_to_repo(mut release: Release, repo: &GithubRepo) -> Result<Release, GithubError> {
    let prefix = format!(
        "https://github.com/{}/{}/releases/download/",
        repo.owner, repo.repo
    )
    .to_lowercase();
    let before = release.assets.len();
    release
        .assets
        .retain(|a| a.browser_download_url.to_lowercase().starts_with(&prefix));
    if before > 0 && release.assets.is_empty() {
        return Err(GithubError::ForeignAssets(repo.full_name()));
    }
    if release.assets.len() < before {
        tracing::warn!(
            "{}: ignored {} download(s) hosted outside the repository",
            repo.full_name(),
            before - release.assets.len()
        );
    }
    Ok(release)
}

async fn fetch_unscoped(
    fetcher: &dyn ReleaseFetcher,
    cache: Option<&DiskCache>,
    repo: &GithubRepo,
) -> Result<Release, GithubError> {
    let cached = cache.and_then(|c| c.get(repo));
    let etag = cached.as_ref().and_then(|c| c.etag.clone());
    match fetcher.latest(repo, etag.as_deref()).await {
        Ok(Fetched::Fresh { release, etag }) => {
            if let Some(c) = cache {
                c.put(repo, etag, &release);
            }
            Ok(release)
        }
        Ok(Fetched::NotModified) => cached.map(|c| c.release).ok_or_else(|| {
            GithubError::InvalidResponse("GitHub reported no change, but nothing is cached".into())
        }),
        Err(GithubError::Offline) if cached.is_some() => {
            tracing::info!("offline: using the cached {} release", repo.full_name());
            cached.map(|c| c.release).ok_or(GithubError::Offline)
        }
        Err(e) => Err(e),
    }
}

/// A scan and the check that follows it ask for the same release; reuse the answer.
const MEMO_TTL: Duration = Duration::from_secs(30);

/// The user's explicit "install anyway" for a release with no published checksum.
/// Shared between the GUI (which asks the question) and the source (which obeys the answer).
/// Each consent is single-use: it covers one install.
#[derive(Clone, Default)]
pub struct Consent(Arc<Mutex<HashSet<String>>>);

impl Consent {
    pub fn allow(&self, app_id: &str) {
        if let Ok(mut s) = self.0.lock() {
            s.insert(app_id.to_owned());
        }
    }

    /// Withdraws any unused consent for this app.
    pub fn revoke(&self, app_id: &str) {
        if let Ok(mut s) = self.0.lock() {
            s.remove(app_id);
        }
    }

    /// True once, if the user allowed this app.
    pub fn take(&self, app_id: &str) -> bool {
        self.0.lock().map(|mut s| s.remove(app_id)).unwrap_or(false)
    }
}

/// What installing needs. Without it the source can still list and check updates.
pub struct InstallConfig {
    pub downloader: Arc<dyn Downloader>,
    /// `~/.cache/package-manager/downloads`
    pub downloads_dir: PathBuf,
    /// Absolute path of `package-manager-helper`, run through `pkexec`.
    pub helper: Option<PathBuf>,
    pub consent: Consent,
}

/// Old version folders kept per app so a bad update can be reinstalled by hand.
const KEEP_VERSIONS: usize = 2;
/// pkexec: 126 = dialog dismissed or not authorized, 127 = authentication failed.
const PKEXEC_AUTH_CODES: [i32; 2] = [126, 127];

pub struct GithubSource {
    fetcher: Arc<dyn ReleaseFetcher>,
    cache: Option<DiskCache>,
    store: Arc<dyn TrackedStore>,
    runner: Box<dyn CommandRunner>,
    memo: Mutex<HashMap<GithubRepo, (Instant, Release)>>,
    /// GitHub's limit applies to every repo, so after a rate-limit answer no request is sent
    /// until it lifts; callers get the remaining countdown instead.
    limited_until: Mutex<Option<Instant>>,
    install: Option<InstallConfig>,
}

impl GithubSource {
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
            memo: Mutex::new(HashMap::new()),
            limited_until: Mutex::new(None),
            install: None,
        }
    }

    /// Enables `apply`.
    pub fn with_install(mut self, install: InstallConfig) -> Self {
        self.install = Some(install);
        self
    }

    /// The latest release, using the in-memory answer, then an ETag-validated disk cache.
    /// Offline with a cached copy still answers (stale data beats an empty screen).
    async fn release_for(&self, repo: &GithubRepo) -> Result<Release, GithubError> {
        if let Some((at, rel)) = self.memo.lock().ok().and_then(|m| m.get(repo).cloned()) {
            if at.elapsed() < MEMO_TTL {
                return Ok(rel);
            }
        }
        if let Some(until) = self.limited_until.lock().ok().and_then(|l| *l) {
            let left = until.saturating_duration_since(Instant::now()).as_secs();
            if left > 0 {
                return Err(GithubError::RateLimited {
                    retry_after_secs: left,
                });
            }
        }
        let release = match fetch_cached(self.fetcher.as_ref(), self.cache.as_ref(), repo).await {
            Ok(release) => release,
            Err(e) => {
                if let GithubError::RateLimited { retry_after_secs } = &e {
                    if let Ok(mut l) = self.limited_until.lock() {
                        *l = Some(Instant::now() + Duration::from_secs(*retry_after_secs));
                    }
                }
                return Err(e);
            }
        };
        if let Ok(mut memo) = self.memo.lock() {
            memo.insert(repo.clone(), (Instant::now(), release.clone()));
        }
        Ok(release)
    }

    /// `Some(version)` if the package is fully installed.
    async fn installed_version(&self, package: &str) -> Option<String> {
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

    fn checksum_state(release: &Release, asset: &Asset) -> ChecksumState {
        match find_checksum_source(&release.assets, &asset.name) {
            Some(src) => ChecksumState::Published(src.kind),
            None => ChecksumState::Missing,
        }
    }

    fn provenance_for(
        tracked_repo: &GithubRepo,
        pattern: &str,
        release: Option<&Release>,
    ) -> package_manager_core::Provenance {
        let preset = is_preset_repo(tracked_repo);
        let url = release
            .map(|r| r.html_url.clone())
            .filter(|u| is_github_https_url(u))
            .unwrap_or_else(|| format!("https://github.com/{}/releases", tracked_repo.full_name()));
        let state = release
            .and_then(|r| {
                select_asset(&r.assets, pattern, r.version(), host_arch())
                    .map(|a| Self::checksum_state(r, a))
            })
            .unwrap_or(ChecksumState::Unknown);
        github_provenance(&tracked_repo.owner, &tracked_repo.repo, &url, preset, state)
    }

    async fn app_for(&self, t: &TrackedApp) -> Option<InstalledApp> {
        let version = self.installed_version(&t.package_name).await?;
        // A failed lookup (offline, rate limited) just leaves the trust level at "unknown".
        let release = self.release_for(&t.repo).await.ok();
        Some(InstalledApp {
            id: format!("github:{}", t.repo.full_name()),
            icon: IconRef::initial_for(&t.package_name),
            name: t.package_name.clone(),
            version,
            source: AppSource::Github {
                repo: t.repo.clone(),
                asset_pattern: t.asset_pattern.clone(),
            },
            provenance: Self::provenance_for(&t.repo, &t.asset_pattern, release.as_ref()),
            desktop_file: None,
            homepage: Some(format!("https://github.com/{}", t.repo.full_name())),
            installed_at: None,
            is_system: false,
        })
    }

    async fn check_one(
        &self,
        app: &InstalledApp,
        repo: &GithubRepo,
        pattern: &str,
    ) -> Result<Option<UpdateCandidate>, GithubError> {
        let release = self.release_for(repo).await?;
        if release.draft || release.prerelease || !is_newer(&app.version, release.version()) {
            return Ok(None);
        }
        let asset = select_asset(&release.assets, pattern, release.version(), host_arch())
            .ok_or_else(|| GithubError::NoMatchingAsset {
                repo: repo.full_name(),
                tag: release.tag_name.clone(),
                pattern: pattern.to_owned(),
            })?;
        let mut app = app.clone();
        app.provenance = Self::provenance_for(repo, pattern, Some(&release));
        Ok(Some(UpdateCandidate {
            app,
            latest_version: release.version().to_owned(),
            changelog_url: Some(release.html_url.clone()).filter(|u| is_github_https_url(u)),
            download_url: Some(asset.browser_download_url.clone()),
            size_bytes: Some(asset.size),
            is_security: false,
            source_kind: UpdateKind::Github,
        }))
    }
}

#[async_trait]
impl UpdateSource for GithubSource {
    fn kind(&self) -> &'static str {
        "github"
    }

    /// Linked apps that are installed. Nothing is ever discovered or linked automatically.
    async fn scan(&self) -> Result<Vec<InstalledApp>, SourceError> {
        let mut apps = Vec::new();
        for t in self.store.list().await? {
            if let Some(app) = self.app_for(&t).await {
                apps.push(app);
            }
        }
        Ok(apps)
    }

    async fn check_updates(
        &self,
        apps: &[InstalledApp],
    ) -> Result<Vec<UpdateCandidate>, SourceError> {
        let mut found = Vec::new();
        let mut first_error = None;
        for app in apps {
            let AppSource::Github {
                repo,
                asset_pattern,
            } = &app.source
            else {
                continue;
            };
            match self.check_one(app, repo, asset_pattern).await {
                Ok(Some(c)) => found.push(c),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("{}: {e}", repo.full_name());
                    first_error.get_or_insert(e);
                }
            }
        }
        // One broken repo must not hide updates for the others.
        match (found.is_empty(), first_error) {
            (true, Some(e)) => Err(e.into()),
            _ => Ok(found),
        }
    }

    async fn apply(
        &self,
        candidates: &[UpdateCandidate],
        progress: ProgressSender,
    ) -> Result<ApplyReport, SourceError> {
        let mut report = ApplyReport::default();
        for c in candidates {
            let id = c.app.id.clone();
            // A closed receiver only means nobody is listening anymore.
            let _ = progress.send(ProgressEvent::Started { app_id: id.clone() });
            let outcome = self.install_one(c, &progress).await;
            // The user's "install anyway" covers this one attempt, whatever its outcome.
            if let Some(cfg) = &self.install {
                cfg.consent.revoke(&id);
            }
            match outcome {
                Ok(()) => {
                    let _ = progress.send(ProgressEvent::Finished { app_id: id.clone() });
                    report.succeeded.push(id);
                }
                // These stop the whole run: retrying the next app would fail the same way.
                Err(e @ (SourceError::Cancelled | SourceError::Locked)) => return Err(e),
                Err(e) => {
                    let reason = e.to_string();
                    let _ = progress.send(ProgressEvent::Failed {
                        app_id: id.clone(),
                        reason: reason.clone(),
                    });
                    report.failed.push((id, reason));
                }
            }
        }
        Ok(report)
    }
}

impl GithubSource {
    /// Download, verify, install and re-check one update. Any failure leaves the system as it
    /// was: nothing reaches the installer unless every earlier check passed.
    async fn install_one(
        &self,
        c: &UpdateCandidate,
        progress: &ProgressSender,
    ) -> Result<(), SourceError> {
        let cfg = self
            .install
            .as_ref()
            .ok_or(GithubError::NotConfigured("installing is disabled"))?;
        let helper = cfg.helper.as_ref().ok_or(GithubError::NotConfigured(
            "the update helper was not found",
        ))?;
        let AppSource::Github {
            repo,
            asset_pattern,
        } = &c.app.source
        else {
            return Err(SourceError::Other("This is not a GitHub app.".into()));
        };

        // The package to install comes from the user's own link in the database, never from
        // anything the release or the UI says.
        let tracked = self.store.list().await?;
        let package = tracked
            .iter()
            .find(|t| &t.repo == repo && &t.asset_pattern == asset_pattern)
            .map(|t| t.package_name.clone())
            .ok_or_else(|| {
                SourceError::Other(format!("{} is not linked to a package.", repo.full_name()))
            })?;
        if !is_valid_package_name(&package) {
            return Err(SourceError::Other(format!(
                "Refusing to install a package named {package:?}."
            )));
        }

        // Re-derive the download from the release; it must be exactly what the user was shown.
        let release = self.release_for(repo).await?;
        if release.version() != c.latest_version {
            return Err(GithubError::ReleaseChanged.into());
        }
        let asset = select_asset(
            &release.assets,
            asset_pattern,
            release.version(),
            host_arch(),
        )
        .filter(|a| Some(&a.browser_download_url) == c.download_url.as_ref())
        .ok_or(GithubError::ReleaseChanged)?;
        if asset_kind(&asset.name) != Some(AssetKind::Deb) {
            return Err(GithubError::UnsupportedAsset(asset.name.clone()).into());
        }

        let checksum_src = find_checksum_source(&release.assets, &asset.name);
        let checksum_bytes = match &checksum_src {
            Some(src) => Some(
                cfg.downloader
                    .fetch_bytes(&src.asset.browser_download_url, MAX_CHECKSUM_BYTES)
                    .await?,
            ),
            None => None,
        };
        // Checksum: mandatory when the maintainer publishes one.
        let published = match (&checksum_src, &checksum_bytes) {
            (Some(src), Some(bytes)) => {
                let text = String::from_utf8_lossy(bytes);
                Some(
                    parse_checksum(src.kind, &text, &asset.name)
                        .ok_or(GithubError::ChecksumUnverifiable)?,
                )
            }
            _ if cfg.consent.take(&c.app.id) => None,
            _ => return Err(GithubError::NeedsConfirmation.into()),
        };

        // Download to <downloads>/<package>/<version>/<file>.
        let (version_dir, file_name) = match (
            safe_component(&package),
            safe_component(release.version()),
            safe_component(&asset.name),
        ) {
            (Some(p), Some(v), Some(f)) => (cfg.downloads_dir.join(p).join(v), f),
            _ => {
                return Err(SourceError::Other(
                    "The release has an unusable name.".into(),
                ))
            }
        };
        tokio::fs::create_dir_all(&version_dir).await.map_err(|e| {
            SourceError::Other(format!("Could not create the download folder: {e}"))
        })?;
        let final_path = version_dir.join(file_name);
        let part = version_dir.join(format!("{file_name}.part"));
        if final_path.exists() {
            // A finished earlier download is re-verified like a fresh one.
            let _ = tokio::fs::rename(&final_path, &part).await;
        }
        let app_id = c.app.id.clone();
        let tx = progress.clone();
        cfg.downloader
            .download(
                &asset.browser_download_url,
                &part,
                asset.size,
                &move |percent| {
                    let _ = tx.send(ProgressEvent::Percent {
                        app_id: app_id.clone(),
                        percent,
                    });
                },
            )
            .await?;

        // Verify off the async threads: hashing a large file blocks.
        let (verify_part, expected_size, expected) = (part.clone(), asset.size, published.clone());
        let pin = tokio::task::spawn_blocking(move || {
            verify_download(&verify_part, expected_size, expected.as_ref())
        })
        .await
        .map_err(|e| SourceError::Other(format!("Verification failed to run: {e}")))?;
        let pin = match pin {
            Ok(d) => d,
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e.into());
            }
        };
        tokio::fs::rename(&part, &final_path)
            .await
            .map_err(|e| SourceError::Other(format!("Could not finish the download: {e}")))?;

        // Install as root. The helper re-checks the path, the checksum (on its own private copy
        // of the file) and the package name.
        let (algo, hex) = digest_args(&pin);
        let path_str = final_path
            .to_str()
            .ok_or_else(|| SourceError::Other("The download path is not valid text.".into()))?;
        let helper_str = helper
            .to_str()
            .filter(|_| helper.is_absolute())
            .ok_or_else(|| SourceError::Other("The update helper path is invalid.".into()))?;
        let out = self
            .runner
            .run_logged(
                PKEXEC,
                &[helper_str, "install-deb", path_str, &package, algo, &hex],
            )
            .await?;
        match out.code {
            Some(code) if PKEXEC_AUTH_CODES.contains(&code) => return Err(SourceError::Cancelled),
            _ if out.stderr.contains("Could not get lock") || out.stderr.contains("is locked") => {
                return Err(SourceError::Locked);
            }
            _ => {}
        }
        if !out.success {
            let detail = out
                .stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("no details")
                .trim();
            return Err(GithubError::InstallFailed(detail.to_owned()).into());
        }

        // Verify what is really installed now.
        match self.installed_version(&package).await {
            Some(v) if !is_newer(&v, &c.latest_version) => {}
            Some(v) => return Err(GithubError::VerifyFailed(v).into()),
            None => return Err(GithubError::VerifyFailed("not installed".into()).into()),
        }

        if let Some(package_dir) = version_dir.parent() {
            let (dir, current) = (package_dir.to_path_buf(), version_dir.clone());
            let _ = tokio::task::spawn_blocking(move || {
                prune_old_versions(&dir, &current, KEEP_VERSIONS)
            })
            .await;
        }
        Ok(())
    }
}

fn verify_download(
    path: &std::path::Path,
    expected_size: u64,
    published: Option<&Digest>,
) -> Result<Digest, GithubError> {
    let io = |e: std::io::Error| {
        GithubError::DownloadFailed(format!("could not read the download: {e}"))
    };
    let actual = std::fs::metadata(path).map_err(io)?.len();
    if actual != expected_size {
        return Err(GithubError::SizeMismatch {
            expected: expected_size,
            actual,
        });
    }
    if !has_deb_magic(path).map_err(io)? {
        return Err(GithubError::NotDebianPackage);
    }
    match published {
        Some(want) => {
            if &file_digest(path, want).map_err(io)? == want {
                Ok(want.clone())
            } else {
                Err(GithubError::ChecksumMismatch)
            }
        }
        None => sha256_file(path).map_err(io),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use package_manager_core::exec::scripted::ScriptedRunner;
    use package_manager_core::{SignatureStatus, TrustLevel};

    use super::*;

    const HYDRA_JSON: &[u8] = include_bytes!("../tests/fixtures/hydra-release.json");
    const DPKG: &str = "dpkg-query -W -f=${Version}\t${db:Status-Abbrev}\n hydralauncher";

    struct FakeFetcher {
        responses: Mutex<VecDeque<Result<Fetched, GithubError>>>,
        etags_seen: Mutex<Vec<Option<String>>>,
    }

    impl FakeFetcher {
        fn new(responses: Vec<Result<Fetched, GithubError>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                etags_seen: Mutex::new(vec![]),
            })
        }
    }

    #[async_trait]
    impl ReleaseFetcher for FakeFetcher {
        async fn latest(
            &self,
            repo: &GithubRepo,
            etag: Option<&str>,
        ) -> Result<Fetched, GithubError> {
            self.etags_seen
                .lock()
                .expect("lock")
                .push(etag.map(str::to_owned));
            let response = self
                .responses
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or(Err(GithubError::Offline));
            // Like GitHub, serve the release's downloads from the repository that was asked
            // for (the fixture comes from hydralauncher/hydra).
            response.map(|f| match f {
                Fetched::Fresh { mut release, etag } => {
                    let to = format!("github.com/{}/", repo.full_name());
                    for a in &mut release.assets {
                        a.browser_download_url = a
                            .browser_download_url
                            .replace("github.com/hydralauncher/hydra/", &to);
                    }
                    Fetched::Fresh { release, etag }
                }
                other => other,
            })
        }
    }

    struct FakeStore(Mutex<Vec<TrackedApp>>);

    impl FakeStore {
        fn new(apps: Vec<TrackedApp>) -> Self {
            Self(Mutex::new(apps))
        }
    }

    #[async_trait]
    impl TrackedStore for FakeStore {
        async fn list(&self) -> Result<Vec<TrackedApp>, SourceError> {
            Ok(self.0.lock().expect("lock").clone())
        }

        async fn save(&self, app: &TrackedApp) -> Result<(), SourceError> {
            let mut all = self.0.lock().expect("lock");
            all.retain(|t| t.package_name != app.package_name);
            all.push(app.clone());
            Ok(())
        }
    }

    fn fresh(json: &[u8], etag: Option<&str>) -> Result<Fetched, GithubError> {
        Ok(Fetched::Fresh {
            release: parse_release(json).expect("fixture"),
            etag: etag.map(str::to_owned),
        })
    }

    fn hydra() -> TrackedApp {
        let p = preset_for_package("hydralauncher").expect("preset");
        TrackedApp {
            package_name: p.package_name.clone(),
            repo: p.repo().expect("repo"),
            asset_pattern: p.asset_pattern.clone(),
            is_preset: true,
        }
    }

    fn installed(version: &str) -> ScriptedRunner {
        ScriptedRunner::new().with(DPKG, &format!("{version}\tii \n"))
    }

    fn source(
        fetcher: Arc<FakeFetcher>,
        tracked: Vec<TrackedApp>,
        runner: ScriptedRunner,
        cache: Option<DiskCache>,
    ) -> GithubSource {
        GithubSource::new(
            fetcher,
            cache,
            Arc::new(FakeStore::new(tracked)),
            Box::new(runner),
        )
    }

    fn temp_cache(name: &str) -> (DiskCache, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("um-ghsrc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        (DiskCache::new(d.clone()), d)
    }

    #[tokio::test]
    async fn scan_lists_an_installed_preset_as_verified_github_app() {
        let src = source(
            FakeFetcher::new(vec![fresh(HYDRA_JSON, None)]),
            vec![hydra()],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        assert_eq!(apps.len(), 1);
        let a = &apps[0];
        assert_eq!(
            (a.id.as_str(), a.version.as_str()),
            ("github:hydralauncher/hydra", "4.3.0")
        );
        assert_eq!(a.provenance.display_name, "github.com/hydralauncher/hydra");
        assert_eq!(a.provenance.trust, TrustLevel::Verified);
        assert!(matches!(a.source, AppSource::Github { .. }));
    }

    #[tokio::test]
    async fn scan_skips_linked_apps_that_are_not_installed() {
        let runner = ScriptedRunner::new().with_failure(DPKG, "dpkg-query: no packages found");
        let src = source(FakeFetcher::new(vec![]), vec![hydra()], runner, None);
        assert!(src.scan().await.expect("scan").is_empty());
        let removed = ScriptedRunner::new().with(DPKG, "4.3.0\trc \n");
        let src = source(FakeFetcher::new(vec![]), vec![hydra()], removed, None);
        assert!(src.scan().await.expect("scan").is_empty());
    }

    #[tokio::test]
    async fn offline_scan_still_lists_the_app_without_claiming_trust() {
        let src = source(
            FakeFetcher::new(vec![Err(GithubError::Offline)]),
            vec![hydra()],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        assert_eq!(apps[0].provenance.trust, TrustLevel::Community);
        assert_eq!(apps[0].provenance.signature, SignatureStatus::NotChecked);
    }

    #[tokio::test]
    async fn check_reports_the_newer_release_with_download_size_and_changelog() {
        let src = source(
            FakeFetcher::new(vec![fresh(HYDRA_JSON, None)]),
            vec![hydra()],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        let updates = src.check_updates(&apps).await.expect("check");
        assert_eq!(updates.len(), 1);
        let u = &updates[0];
        assert_eq!(u.latest_version, "4.4.0");
        assert_eq!(u.size_bytes, Some(178_442_846));
        assert!(u
            .download_url
            .as_deref()
            .is_some_and(|d| d.ends_with("hydralauncher_4.4.0_amd64.deb")));
        assert_eq!(
            u.changelog_url.as_deref(),
            Some("https://github.com/hydralauncher/hydra/releases/tag/v4.4.0")
        );
        assert_eq!(u.source_kind, UpdateKind::Github);
        assert_eq!(u.app.provenance.trust, TrustLevel::Verified);
    }

    #[tokio::test]
    async fn equal_versions_are_never_reported_as_updates() {
        // `v4.4.0` in the tag equals `4.4.0` installed.
        let src = source(
            FakeFetcher::new(vec![fresh(HYDRA_JSON, None)]),
            vec![hydra()],
            installed("4.4.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        assert!(src.check_updates(&apps).await.expect("check").is_empty());
    }

    #[tokio::test]
    async fn a_newer_installed_version_is_not_downgraded() {
        let src = source(
            FakeFetcher::new(vec![fresh(HYDRA_JSON, None)]),
            vec![hydra()],
            installed("5.0.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        assert!(src.check_updates(&apps).await.expect("check").is_empty());
    }

    #[tokio::test]
    async fn a_pattern_that_matches_nothing_is_a_clear_error() {
        let mut t = hydra();
        t.asset_pattern = "hydra_{version}_amd64.deb".into(); // the wrong name from the old design doc
        let src = source(
            FakeFetcher::new(vec![fresh(HYDRA_JSON, None)]),
            vec![t],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        let err = src.check_updates(&apps).await.expect_err("no asset");
        assert!(err.to_string().contains("no download matching"), "{err}");
    }

    #[tokio::test]
    async fn user_linked_forks_are_community_not_verified() {
        let mut t = hydra();
        t.repo = GithubRepo::parse("someone/hydra-fork").expect("repo");
        t.is_preset = false;
        let src = source(
            FakeFetcher::new(vec![fresh(HYDRA_JSON, None)]),
            vec![t],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        assert_eq!(apps[0].provenance.trust, TrustLevel::Community);
    }

    #[tokio::test]
    async fn a_release_without_checksum_is_marked_signature_missing() {
        let mut json: serde_json::Value = serde_json::from_slice(HYDRA_JSON).expect("json");
        json["assets"]
            .as_array_mut()
            .expect("assets")
            .retain(|a| a["name"] != "latest-linux.yml");
        let bytes = serde_json::to_vec(&json).expect("encode");
        let src = source(
            FakeFetcher::new(vec![fresh(&bytes, None)]),
            vec![hydra()],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        assert_eq!(apps[0].provenance.signature, SignatureStatus::Missing);
        assert_eq!(apps[0].provenance.trust, TrustLevel::Community);
    }

    #[tokio::test]
    async fn prereleases_and_drafts_are_ignored() {
        let mut json: serde_json::Value = serde_json::from_slice(HYDRA_JSON).expect("json");
        json["prerelease"] = true.into();
        let bytes = serde_json::to_vec(&json).expect("encode");
        let src = source(
            FakeFetcher::new(vec![fresh(&bytes, None)]),
            vec![hydra()],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        assert!(src.check_updates(&apps).await.expect("check").is_empty());
    }

    #[tokio::test]
    async fn second_request_sends_the_etag_and_a_304_uses_the_cache() {
        let (cache, dir) = temp_cache("etag");
        let first = FakeFetcher::new(vec![fresh(HYDRA_JSON, Some("W/\"v1\""))]);
        let src = source(first, vec![hydra()], installed("4.3.0"), Some(cache));
        assert_eq!(src.scan().await.expect("scan").len(), 1);

        // A new run (new source, empty memo) revalidates with the stored ETag.
        let second = FakeFetcher::new(vec![Ok(Fetched::NotModified)]);
        let src2 = source(
            Arc::clone(&second),
            vec![hydra()],
            installed("4.3.0"),
            Some(DiskCache::new(dir.clone())),
        );
        let apps = src2.scan().await.expect("scan");
        assert_eq!(
            second.etags_seen.lock().expect("lock").as_slice(),
            &[Some("W/\"v1\"".to_owned())]
        );
        let updates = src2.check_updates(&apps).await.expect("check");
        assert_eq!(updates[0].latest_version, "4.4.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scan_and_check_share_one_request() {
        let f = FakeFetcher::new(vec![fresh(HYDRA_JSON, None)]);
        let src = source(Arc::clone(&f), vec![hydra()], installed("4.3.0"), None);
        let apps = src.scan().await.expect("scan");
        src.check_updates(&apps).await.expect("check");
        assert_eq!(f.etags_seen.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn offline_with_a_cached_release_still_answers() {
        let (cache, dir) = temp_cache("offline");
        let repo = hydra().repo;
        cache.put(
            &repo,
            Some("W/\"v1\"".into()),
            &parse_release(HYDRA_JSON).expect("fixture"),
        );
        let src = source(
            FakeFetcher::new(vec![Err(GithubError::Offline)]),
            vec![hydra()],
            installed("4.3.0"),
            Some(cache),
        );
        let apps = src.scan().await.expect("scan");
        let updates = src.check_updates(&apps).await.expect("check");
        assert_eq!(updates.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offline_without_a_cache_reports_offline() {
        let src = source(
            FakeFetcher::new(vec![Err(GithubError::Offline)]),
            vec![hydra()],
            installed("4.3.0"),
            None,
        );
        let apps = src.scan().await.expect("scan");
        let err = src.check_updates(&apps).await.expect_err("offline");
        assert!(matches!(err, SourceError::Offline));
    }

    #[tokio::test]
    async fn rate_limit_surfaces_with_a_countdown_and_stops_further_requests() {
        let f = FakeFetcher::new(vec![Err(GithubError::RateLimited {
            retry_after_secs: 90,
        })]);
        let src = source(Arc::clone(&f), vec![hydra()], installed("4.3.0"), None);
        let apps = src.scan().await.expect("scan"); // scan tolerates the failure
        let err = src.check_updates(&apps).await.expect_err("limited");
        // The check made no new request; it reports what is left of the 90 s.
        assert!(
            matches!(
                err,
                SourceError::RateLimited {
                    retry_after_secs: 89..=90
                }
            ),
            "{err}"
        );
        assert_eq!(f.etags_seen.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn one_broken_repo_does_not_hide_another_repos_update() {
        let good = hydra();
        let mut bad = hydra();
        bad.package_name = "other".into();
        bad.repo = GithubRepo::parse("o/missing").expect("repo");
        let runner = installed("4.3.0").with(
            "dpkg-query -W -f=${Version}\t${db:Status-Abbrev}\n other",
            "1.0\tii \n",
        );
        // Order of requests: scan(good), scan(bad), check(good) is memoized, check(bad) memoized.
        let f = FakeFetcher::new(vec![
            fresh(HYDRA_JSON, None),
            Err(GithubError::NotFound("o/missing".into())),
        ]);
        let src = source(f, vec![good, bad], runner, None);
        let apps = src.scan().await.expect("scan");
        assert_eq!(apps.len(), 2);
        let updates = src
            .check_updates(&apps)
            .await
            .expect("one repo still works");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].app.id, "github:hydralauncher/hydra");
    }

    // ------------------------------------------------------------------ installing

    use sha2::{Digest as _, Sha256};

    const DEB_NAME: &str = "hydralauncher_4.4.0_amd64.deb";
    const HELPER: &str = "/usr/libexec/package-manager/package-manager-helper";

    fn deb_bytes() -> Vec<u8> {
        let mut v = b"!<arch>\ndebian-binary   ".to_vec();
        v.extend_from_slice(&[9u8; 500]);
        v
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        to_hex(&Sha256::digest(bytes))
    }

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The real Hydra release, reshaped so its .deb has the size of `size` and (optionally) a
    /// SHA256SUMS file instead of Hydra's electron-builder yml.
    fn release_json(size: usize, sums: bool, only_appimage: bool) -> Vec<u8> {
        let mut json: serde_json::Value = serde_json::from_slice(HYDRA_JSON).expect("json");
        let assets = json["assets"].as_array_mut().expect("assets");
        assets.retain(|a| a["name"] != "latest-linux.yml");
        if only_appimage {
            assets.retain(|a| !a["name"].as_str().unwrap_or("").ends_with(".deb"));
        }
        for a in assets.iter_mut() {
            if a["name"] == DEB_NAME {
                a["size"] = size.into();
            }
        }
        if sums {
            assets.push(serde_json::json!({
                "name": "SHA256SUMS", "size": 100,
                "browser_download_url": "https://github.com/hydralauncher/hydra/releases/download/v4.4.0/SHA256SUMS"
            }));
        }
        serde_json::to_vec(&json).expect("encode")
    }

    const DEB_URL: &str = "https://github.com/hydralauncher/hydra/releases/download/v4.4.0/hydralauncher_4.4.0_amd64.deb";
    const SUMS_URL: &str =
        "https://github.com/hydralauncher/hydra/releases/download/v4.4.0/SHA256SUMS";

    struct FakeDownloader {
        files: HashMap<String, Vec<u8>>,
        downloads: Mutex<usize>,
    }

    impl FakeDownloader {
        fn new(files: Vec<(&str, Vec<u8>)>) -> Arc<Self> {
            Arc::new(Self {
                files: files.into_iter().map(|(u, b)| (u.to_owned(), b)).collect(),
                downloads: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl Downloader for FakeDownloader {
        async fn fetch_bytes(&self, url: &str, _max: usize) -> Result<Vec<u8>, GithubError> {
            self.files.get(url).cloned().ok_or(GithubError::Http(404))
        }

        async fn download(
            &self,
            url: &str,
            part: &std::path::Path,
            _size: u64,
            progress: &(dyn Fn(u8) + Send + Sync),
        ) -> Result<(), GithubError> {
            *self.downloads.lock().expect("lock") += 1;
            let b = self.files.get(url).ok_or(GithubError::Http(404))?;
            std::fs::write(part, b).map_err(|e| GithubError::DownloadFailed(e.to_string()))?;
            progress(50);
            progress(100);
            Ok(())
        }
    }

    struct Rig {
        src: GithubSource,
        downloader: Arc<FakeDownloader>,
        consent: Consent,
        dir: std::path::PathBuf,
    }

    fn rig(
        name: &str,
        release: Vec<u8>,
        files: Vec<(&str, Vec<u8>)>,
        runner: ScriptedRunner,
        tracked: Vec<TrackedApp>,
        helper: Option<&str>,
    ) -> Rig {
        let dir = std::env::temp_dir().join(format!("um-apply-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let downloader = FakeDownloader::new(files);
        let consent = Consent::default();
        let src = GithubSource::new(
            FakeFetcher::new(vec![fresh(&release, None)]),
            None,
            Arc::new(FakeStore::new(tracked)),
            Box::new(runner),
        )
        .with_install(InstallConfig {
            downloader: downloader.clone(),
            downloads_dir: dir.clone(),
            helper: helper.map(PathBuf::from),
            consent: consent.clone(),
        });
        Rig {
            src,
            downloader,
            consent,
            dir,
        }
    }

    fn candidate(url: &str, version: &str) -> UpdateCandidate {
        let t = hydra();
        UpdateCandidate {
            app: InstalledApp {
                id: "github:hydralauncher/hydra".into(),
                name: "Hydra".into(), // a display name: the package comes from the database
                version: "4.3.0".into(),
                source: AppSource::Github {
                    repo: t.repo,
                    asset_pattern: t.asset_pattern,
                },
                provenance: package_manager_core::Provenance::unknown(),
                icon: IconRef::Initial('H'),
                desktop_file: None,
                homepage: None,
                installed_at: None,
                is_system: false,
            },
            latest_version: version.into(),
            changelog_url: None,
            download_url: Some(url.into()),
            size_bytes: None,
            is_security: false,
            source_kind: UpdateKind::Github,
        }
    }

    fn install_key(rig: &Rig, algo: &str, hex: &str) -> String {
        let path = rig.dir.join("hydralauncher/4.4.0").join(DEB_NAME);
        format!(
            "/usr/bin/pkexec {HELPER} install-deb {} hydralauncher {algo} {hex}",
            path.display()
        )
    }

    fn sums_for(bytes: &[u8]) -> Vec<u8> {
        format!("{}  {DEB_NAME}\n", sha256_hex(bytes)).into_bytes()
    }

    async fn apply_one(
        rig: &Rig,
        c: UpdateCandidate,
    ) -> (Result<ApplyReport, SourceError>, Vec<ProgressEvent>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = rig.src.apply(&[c], tx).await;
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        (result, events)
    }

    fn failure_text(r: &Result<ApplyReport, SourceError>) -> String {
        match r {
            Ok(rep) => rep
                .failed
                .iter()
                .map(|(_, m)| m.clone())
                .collect::<Vec<_>>()
                .join("; "),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn verified_download_is_installed_and_the_new_version_confirmed() {
        let bytes = deb_bytes();
        let runner = installed("4.4.0");
        let mut r = rig(
            "happy",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            runner,
            vec![hydra()],
            Some(HELPER),
        );
        // The installer call must carry the verified checksum and the approved package name.
        r.src = {
            let runner = installed("4.4.0").with(
                &install_key(&r, "sha256", &sha256_hex(&bytes)),
                "Checksum verified\n",
            );
            GithubSource::new(
                FakeFetcher::new(vec![fresh(&release_json(bytes.len(), true, false), None)]),
                None,
                Arc::new(FakeStore::new(vec![hydra()])),
                Box::new(runner),
            )
            .with_install(InstallConfig {
                downloader: r.downloader.clone(),
                downloads_dir: r.dir.clone(),
                helper: Some(HELPER.into()),
                consent: r.consent.clone(),
            })
        };
        let (result, events) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        let report = result.expect("apply");
        assert_eq!(
            report.succeeded,
            vec!["github:hydralauncher/hydra"],
            "{}",
            failure_text(&Ok(report.clone()))
        );
        assert!(r.dir.join("hydralauncher/4.4.0").join(DEB_NAME).exists());
        assert!(matches!(
            events.first(),
            Some(ProgressEvent::Started { .. })
        ));
        assert!(events
            .iter()
            .any(|e| matches!(e, ProgressEvent::Percent { percent: 100, .. })));
        assert!(matches!(
            events.last(),
            Some(ProgressEvent::Finished { .. })
        ));
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn a_tampered_download_is_deleted_and_never_reaches_the_installer() {
        let good = deb_bytes();
        let mut evil = good.clone();
        evil[100] ^= 0xff; // same size and header, different content
        let r = rig(
            "tamper",
            release_json(good.len(), true, false),
            vec![(DEB_URL, evil), (SUMS_URL, sums_for(&good))],
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let (result, events) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        let text = failure_text(&result);
        assert!(text.contains("does not match the checksum"), "{text}");
        assert!(
            !text.contains("unscripted"),
            "the installer must not have been called"
        );
        assert!(!r.dir.join("hydralauncher/4.4.0").join(DEB_NAME).exists());
        assert!(!r
            .dir
            .join("hydralauncher/4.4.0")
            .join(format!("{DEB_NAME}.part"))
            .exists());
        assert!(matches!(events.last(), Some(ProgressEvent::Failed { .. })));
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn a_download_of_the_wrong_size_is_rejected() {
        let bytes = deb_bytes();
        let r = rig(
            "size",
            release_json(bytes.len() + 10, true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("bytes but"),
            "{}",
            failure_text(&result)
        );
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn a_matching_checksum_does_not_make_a_non_deb_installable() {
        let script = b"#!/bin/sh\necho pwned\n".repeat(30);
        let r = rig(
            "notdeb",
            release_json(script.len(), true, false),
            vec![(DEB_URL, script.clone()), (SUMS_URL, sums_for(&script))],
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("not a Debian package"),
            "{}",
            failure_text(&result)
        );
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn a_checksum_file_that_does_not_list_the_download_blocks_the_install() {
        let bytes = deb_bytes();
        let other = format!("{}  some_other_file.deb\n", sha256_hex(&bytes)).into_bytes();
        let r = rig(
            "unlisted",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes), (SUMS_URL, other)],
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("cannot be verified"),
            "{}",
            failure_text(&result)
        );
        assert_eq!(
            *r.downloader.downloads.lock().expect("lock"),
            0,
            "nothing should be downloaded"
        );
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn no_checksum_means_no_download_until_the_user_confirms() {
        let bytes = deb_bytes();
        let r = rig(
            "noconsent",
            release_json(bytes.len(), false, false),
            vec![(DEB_URL, bytes)],
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("No checksum provided by maintainer"),
            "{}",
            failure_text(&result)
        );
        assert_eq!(*r.downloader.downloads.lock().expect("lock"), 0);
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn confirmed_installs_without_checksum_pin_the_downloaded_bytes_and_consent_is_single_use(
    ) {
        let bytes = deb_bytes();
        let dir_rig = rig(
            "consent-dir",
            release_json(bytes.len(), false, false),
            vec![],
            ScriptedRunner::new(),
            vec![hydra()],
            Some(HELPER),
        );
        let key = install_key(&dir_rig, "sha256", &sha256_hex(&bytes));
        let runner = installed("4.4.0").with(&key, "ok\n");
        let r = rig(
            "consent-dir",
            release_json(bytes.len(), false, false),
            vec![(DEB_URL, bytes)],
            runner,
            vec![hydra()],
            Some(HELPER),
        );
        r.consent.allow("github:hydralauncher/hydra");
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert_eq!(result.expect("apply").succeeded.len(), 1);
        // The consent was used up: a second attempt asks again.
        assert!(!r.consent.take("github:hydralauncher/hydra"));
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn a_candidate_that_no_longer_matches_the_release_is_refused() {
        let bytes = deb_bytes();
        let files = || vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))];
        // Newer release published since the check.
        let r = rig(
            "changed",
            release_json(bytes.len(), true, false),
            files(),
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.3.9")).await;
        assert!(
            failure_text(&result).contains("newer release appeared"),
            "{}",
            failure_text(&result)
        );
        // A candidate whose download URL was altered.
        let r = rig(
            "badurl",
            release_json(bytes.len(), true, false),
            files(),
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(
            &r,
            candidate(
                "https://github.com/evil/repo/releases/download/v4.4.0/x.deb",
                "4.4.0",
            ),
        )
        .await;
        assert!(
            failure_text(&result).contains("newer release appeared"),
            "{}",
            failure_text(&result)
        );
        assert_eq!(*r.downloader.downloads.lock().expect("lock"), 0);
    }

    #[tokio::test]
    async fn appimage_only_releases_are_not_installed_yet() {
        let r = rig(
            "appimage",
            release_json(100, true, true),
            vec![],
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        let mut t = hydra();
        t.asset_pattern = "*.AppImage".into();
        let mut c = candidate("https://github.com/hydralauncher/hydra/releases/download/v4.4.0/hydralauncher-4.4.0.AppImage", "4.4.0");
        c.app.source = AppSource::Github {
            repo: t.repo,
            asset_pattern: t.asset_pattern.clone(),
        };
        let r = GithubSource::new(
            FakeFetcher::new(vec![fresh(&release_json(100, true, true), None)]),
            None,
            Arc::new(FakeStore::new(vec![TrackedApp {
                asset_pattern: t.asset_pattern,
                ..hydra()
            }])),
            Box::new(installed("4.4.0")),
        )
        .with_install(InstallConfig {
            downloader: FakeDownloader::new(vec![]),
            downloads_dir: r.dir.clone(),
            helper: Some(HELPER.into()),
            consent: Consent::default(),
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let report = r.apply(&[c], tx).await.expect("apply");
        assert!(
            report.failed[0].1.contains("Only .deb updates"),
            "{:?}",
            report.failed
        );
    }

    #[tokio::test]
    async fn dismissing_the_password_dialog_cancels_the_run() {
        let bytes = deb_bytes();
        let probe = rig(
            "cancel-probe",
            release_json(bytes.len(), true, false),
            vec![],
            ScriptedRunner::new(),
            vec![hydra()],
            Some(HELPER),
        );
        let key = install_key(&probe, "sha256", &sha256_hex(&bytes));
        let runner = installed("4.4.0").with_exit(&key, 126, "");
        let r = rig(
            "cancel-probe",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            runner,
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(matches!(result, Err(SourceError::Cancelled)), "{result:?}");
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn an_installer_failure_is_reported_with_its_last_message() {
        let bytes = deb_bytes();
        let probe = rig(
            "fail-probe",
            release_json(bytes.len(), true, false),
            vec![],
            ScriptedRunner::new(),
            vec![hydra()],
            Some(HELPER),
        );
        let key = install_key(&probe, "sha256", &sha256_hex(&bytes));
        let runner = installed("4.3.0").with_exit(
            &key,
            100,
            "dpkg: dependency problems\nE: Unmet dependencies\n",
        );
        let r = rig(
            "fail-probe",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            runner,
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("E: Unmet dependencies"),
            "{}",
            failure_text(&result)
        );
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn success_is_only_claimed_if_the_new_version_is_really_installed() {
        let bytes = deb_bytes();
        let probe = rig(
            "verify-probe",
            release_json(bytes.len(), true, false),
            vec![],
            ScriptedRunner::new(),
            vec![hydra()],
            Some(HELPER),
        );
        let key = install_key(&probe, "sha256", &sha256_hex(&bytes));
        // The installer "succeeds" but dpkg still reports the old version.
        let runner = installed("4.3.0").with(&key, "ok\n");
        let r = rig(
            "verify-probe",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            runner,
            vec![hydra()],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        let report = result.expect("report");
        assert!(report.succeeded.is_empty());
        assert!(
            report.failed[0].1.contains("still at version 4.3.0"),
            "{:?}",
            report.failed
        );
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn the_package_comes_from_the_database_and_must_be_a_valid_name() {
        let bytes = deb_bytes();
        // Not linked at all.
        let r = rig(
            "unlinked",
            release_json(bytes.len(), true, false),
            vec![],
            installed("4.4.0"),
            vec![],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("not linked"),
            "{}",
            failure_text(&result)
        );
        // A hostile name stored in the database is refused before any download or command.
        let mut bad = hydra();
        bad.package_name = "-oAPT::Update::Pre-Invoke::=sh".into();
        let r = rig(
            "badpkg",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            installed("4.4.0"),
            vec![bad],
            Some(HELPER),
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("Refusing to install a package"),
            "{}",
            failure_text(&result)
        );
        assert_eq!(*r.downloader.downloads.lock().expect("lock"), 0);
    }

    #[tokio::test]
    async fn installing_without_a_helper_fails_clearly_before_downloading() {
        let bytes = deb_bytes();
        let r = rig(
            "nohelper",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            installed("4.4.0"),
            vec![hydra()],
            None,
        );
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert!(
            failure_text(&result).contains("helper was not found"),
            "{}",
            failure_text(&result)
        );
        assert_eq!(*r.downloader.downloads.lock().expect("lock"), 0);
        let plain = source(
            FakeFetcher::new(vec![]),
            vec![],
            ScriptedRunner::new(),
            None,
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rep = plain
            .apply(&[candidate(DEB_URL, "4.4.0")], tx)
            .await
            .expect("report");
        assert!(rep.failed[0].1.contains("not set up"));
    }

    #[tokio::test]
    async fn old_versions_beyond_the_last_two_are_cleaned_up_after_a_successful_install() {
        let bytes = deb_bytes();
        let probe = rig(
            "prune-probe",
            release_json(bytes.len(), true, false),
            vec![],
            ScriptedRunner::new(),
            vec![hydra()],
            Some(HELPER),
        );
        let key = install_key(&probe, "sha256", &sha256_hex(&bytes));
        let runner = installed("4.4.0").with(&key, "ok\n");
        let r = rig(
            "prune-probe",
            release_json(bytes.len(), true, false),
            vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))],
            runner,
            vec![hydra()],
            Some(HELPER),
        );
        for (i, v) in ["4.1.0", "4.2.0", "4.3.0"].iter().enumerate() {
            let d = r.dir.join("hydralauncher").join(v);
            std::fs::create_dir_all(&d).expect("mk");
            std::fs::write(d.join("x.deb"), b"x").expect("w");
            let t = std::time::SystemTime::now() - Duration::from_secs(5000 - i as u64 * 1000);
            std::fs::File::open(&d)
                .expect("o")
                .set_modified(t)
                .expect("mtime");
        }
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.4.0")).await;
        assert_eq!(result.expect("apply").succeeded.len(), 1);
        let mut left: Vec<_> = std::fs::read_dir(r.dir.join("hydralauncher"))
            .expect("ls")
            .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["4.3.0", "4.4.0"], "newest two versions are kept");
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    // ---------------------------------------------------------------- add from GitHub

    fn linker(release: Vec<u8>, store: Arc<FakeStore>, runner: ScriptedRunner) -> Linker {
        Linker::new(
            FakeFetcher::new(vec![fresh(&release, None)]),
            None,
            store,
            Box::new(runner),
        )
    }

    #[tokio::test]
    async fn pasting_the_hydra_releases_link_finds_the_right_deb() {
        let l = linker(
            HYDRA_JSON.to_vec(),
            Arc::new(FakeStore::new(vec![])),
            ScriptedRunner::new(),
        );
        let p = l
            .preview("https://github.com/hydralauncher/hydra/releases")
            .await
            .expect("preview");
        assert_eq!((p.tag.as_str(), p.version.as_str()), ("v4.4.0", "4.4.0"));
        assert_eq!(p.choices.len(), 1, "one .deb for this CPU: {:?}", p.choices);
        let c = &p.choices[0];
        assert_eq!(c.name, "hydralauncher_4.4.0_amd64.deb");
        assert_eq!(c.size, 178_442_846);
        assert_eq!(c.pattern, "hydralauncher_{version}_amd64.deb");
        assert!(matches!(
            c.checksum,
            ChecksumState::Published(ChecksumKind::ElectronYml)
        ));
        assert_eq!(p.preset_name.as_deref(), Some("Hydra Launcher"));
        assert_eq!(p.suggested_package, "hydralauncher");
        assert_eq!(p.provenance(0).trust, TrustLevel::Verified);
        assert!(p.notes.starts_with("## What's Changed"));
    }

    #[tokio::test]
    async fn an_unknown_repo_is_community_and_its_package_name_is_guessed_from_the_file() {
        let l = linker(
            HYDRA_JSON.to_vec(),
            Arc::new(FakeStore::new(vec![])),
            ScriptedRunner::new(),
        );
        let p = l.preview("someone/hydra-fork").await.expect("preview");
        assert!(p.preset_name.is_none());
        assert_eq!(p.suggested_package, "hydralauncher");
        assert_eq!(p.provenance(0).trust, TrustLevel::Community);
    }

    #[tokio::test]
    async fn a_release_without_checksum_is_flagged_in_the_preview() {
        let l = linker(
            release_json(1000, false, false),
            Arc::new(FakeStore::new(vec![])),
            ScriptedRunner::new(),
        );
        let p = l.preview("a/b").await.expect("preview");
        assert_eq!(p.choices[0].checksum, ChecksumState::Missing);
        assert_eq!(p.provenance(0).signature, SignatureStatus::Missing);
    }

    #[tokio::test]
    async fn releases_without_an_installable_deb_say_what_they_do_have() {
        let l = linker(
            release_json(1000, false, true),
            Arc::new(FakeStore::new(vec![])),
            ScriptedRunner::new(),
        );
        let err = l.preview("a/b").await.expect_err("no deb").to_string();
        assert!(
            err.contains("AppImage") && err.contains("cannot be installed yet"),
            "{err}"
        );

        let mut json: serde_json::Value = serde_json::from_slice(HYDRA_JSON).expect("json");
        json["assets"] = serde_json::json!([{"name": "setup.exe", "size": 1, "browser_download_url": "https://github.com/a/b/releases/download/v1/setup.exe"}]);
        let l = linker(
            serde_json::to_vec(&json).expect("enc"),
            Arc::new(FakeStore::new(vec![])),
            ScriptedRunner::new(),
        );
        assert!(l
            .preview("a/b")
            .await
            .expect_err("none")
            .to_string()
            .contains("no downloads this app can install"));
    }

    #[tokio::test]
    async fn bad_input_never_reaches_the_network() {
        let fetcher = FakeFetcher::new(vec![]);
        let l = Linker::new(
            fetcher.clone(),
            None,
            Arc::new(FakeStore::new(vec![])),
            Box::new(ScriptedRunner::new()),
        );
        for input in ["", "http://github.com/a/b", "https://evil.test/a/b", "../x"] {
            assert!(l.preview(input).await.is_err(), "{input:?}");
        }
        assert!(fetcher.etags_seen.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn network_problems_surface_as_friendly_errors() {
        let l = Linker::new(
            FakeFetcher::new(vec![Err(GithubError::Offline)]),
            None,
            Arc::new(FakeStore::new(vec![])),
            Box::new(ScriptedRunner::new()),
        );
        assert!(matches!(l.preview("a/b").await, Err(GithubError::Offline)));
        let l = Linker::new(
            FakeFetcher::new(vec![Err(GithubError::NotFound("a/b".into()))]),
            None,
            Arc::new(FakeStore::new(vec![])),
            Box::new(ScriptedRunner::new()),
        );
        assert!(l
            .preview("a/b")
            .await
            .expect_err("404")
            .to_string()
            .contains("not found on GitHub"));
    }

    #[tokio::test]
    async fn linking_saves_the_package_repo_and_pattern_and_validates_the_name() {
        let store = Arc::new(FakeStore::new(vec![]));
        let l = linker(
            HYDRA_JSON.to_vec(),
            Arc::clone(&store),
            ScriptedRunner::new(),
        );
        let p = l.preview("hydralauncher/hydra").await.expect("preview");
        for bad in ["", "-oAPT::x", "Hydra", "a b", "../x", "x"] {
            assert!(l.link(&p, 0, bad).await.is_err(), "{bad:?}");
        }
        assert!(
            l.link(&p, 7, "hydralauncher").await.is_err(),
            "unknown choice"
        );
        assert!(
            store.list().await.expect("list").is_empty(),
            "nothing saved by failed attempts"
        );
        let t = l.link(&p, 0, " hydralauncher ").await.expect("link");
        assert_eq!(t.package_name, "hydralauncher");
        assert!(t.is_preset);
        assert_eq!(store.list().await.expect("list"), vec![t]);
    }

    #[tokio::test]
    async fn installed_version_is_read_from_dpkg() {
        let l = linker(
            HYDRA_JSON.to_vec(),
            Arc::new(FakeStore::new(vec![])),
            installed("4.3.0"),
        );
        assert_eq!(
            l.installed_version("hydralauncher").await.as_deref(),
            Some("4.3.0")
        );
        assert_eq!(l.installed_version("--help").await, None);
        let none = linker(
            HYDRA_JSON.to_vec(),
            Arc::new(FakeStore::new(vec![])),
            ScriptedRunner::new().with_failure(DPKG, "no packages"),
        );
        assert_eq!(none.installed_version("hydralauncher").await, None);
    }

    /// Paste -> preview -> link -> install: the whole new flow, using the same verified
    /// download and installer path as an update.
    #[tokio::test]
    async fn a_pasted_repo_can_be_linked_and_installed_for_the_first_time() {
        let bytes = deb_bytes();
        let release = release_json(bytes.len(), true, false);
        let store = Arc::new(FakeStore::new(vec![]));
        let l = linker(release.clone(), Arc::clone(&store), ScriptedRunner::new());
        let preview = l
            .preview("https://github.com/hydralauncher/hydra/releases")
            .await
            .expect("preview");
        let tracked = l.link(&preview, 0, "hydralauncher").await.expect("link");
        let candidate = l
            .install_candidate(&preview, 0, &tracked)
            .expect("candidate");
        assert_eq!(candidate.app.version, NOT_INSTALLED);
        assert_eq!(candidate.download_url.as_deref(), Some(DEB_URL));

        let dir = std::env::temp_dir().join(format!("um-addflow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = format!(
            "/usr/bin/pkexec {HELPER} install-deb {} hydralauncher sha256 {}",
            dir.join("hydralauncher/4.4.0").join(DEB_NAME).display(),
            sha256_hex(&bytes)
        );
        let runner = installed("4.4.0").with(&key, "Checksum verified\n");
        let downloader =
            FakeDownloader::new(vec![(DEB_URL, bytes.clone()), (SUMS_URL, sums_for(&bytes))]);
        let source = GithubSource::new(
            FakeFetcher::new(vec![fresh(&release, None)]),
            None,
            store,
            Box::new(runner),
        )
        .with_install(InstallConfig {
            downloader,
            downloads_dir: dir.clone(),
            helper: Some(HELPER.into()),
            consent: Consent::default(),
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let report = source.apply(&[candidate], tx).await.expect("apply");
        assert_eq!(
            report.succeeded,
            vec!["github:hydralauncher/hydra"],
            "{:?}",
            report.failed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_wrong_package_name_guess_cannot_install_the_wrong_thing() {
        // The user links the Hydra .deb under another package's name. The installer compares the
        // name inside the .deb with the approved one, so this fails at the helper (modelled here
        // by the helper's non-zero exit), never silently replacing the other package.
        let bytes = deb_bytes();
        let release = release_json(bytes.len(), true, false);
        let store = Arc::new(FakeStore::new(vec![]));
        let l = linker(release.clone(), Arc::clone(&store), ScriptedRunner::new());
        let preview = l.preview("hydralauncher/hydra").await.expect("preview");
        let tracked = l.link(&preview, 0, "bash").await.expect("link");
        let candidate = l
            .install_candidate(&preview, 0, &tracked)
            .expect("candidate");
        let dir = std::env::temp_dir().join(format!("um-wrongname-{}", std::process::id()));
        let key = format!(
            "/usr/bin/pkexec {HELPER} install-deb {} bash sha256 {}",
            dir.join("bash/4.4.0").join(DEB_NAME).display(),
            sha256_hex(&bytes)
        );
        let runner = installed("5.2").with_exit(&key, 5, "package-manager-helper: the .deb is package \"hydralauncher\", but \"bash\" was approved\n");
        let source = GithubSource::new(
            FakeFetcher::new(vec![fresh(&release, None)]),
            None,
            store,
            Box::new(runner),
        )
        .with_install(InstallConfig {
            downloader: FakeDownloader::new(vec![
                (DEB_URL, bytes.clone()),
                (SUMS_URL, sums_for(&bytes)),
            ]),
            downloads_dir: dir.clone(),
            helper: Some(HELPER.into()),
            consent: Consent::default(),
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let report = source.apply(&[candidate], tx).await.expect("report");
        assert!(report.succeeded.is_empty());
        assert!(
            report.failed[0].1.contains("was approved"),
            "{:?}",
            report.failed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ security review

    #[tokio::test]
    async fn sec_consent_is_not_left_behind_when_an_install_fails_early() {
        // The user ticked "install without a checksum", but the install failed before the
        // consent was used (the candidate no longer matches the release). The tick must not
        // survive to silently approve a *later* install of the same app.
        let bytes = deb_bytes();
        let r = rig(
            "sec-consent",
            release_json(bytes.len(), false, false),
            vec![(DEB_URL, bytes)],
            installed("4.4.0"),
            vec![hydra()],
            Some(HELPER),
        );
        r.consent.allow("github:hydralauncher/hydra");
        let (result, _) = apply_one(&r, candidate(DEB_URL, "4.3.9")).await; // stale version -> fails early
        assert!(
            failure_text(&result).contains("newer release appeared"),
            "{}",
            failure_text(&result)
        );
        assert!(
            !r.consent.take("github:hydralauncher/hydra"),
            "consent must not outlive the attempt"
        );
        let _ = std::fs::remove_dir_all(&r.dir);
    }

    #[tokio::test]
    async fn sec_downloads_must_belong_to_the_linked_repository() {
        // A release whose asset points at a different repository (a lying or confused API
        // response, or a moved repo) must never be downloaded: the download domain/path has to
        // match the provenance shown to the user.
        let mut json: serde_json::Value = serde_json::from_slice(HYDRA_JSON).expect("json");
        for a in json["assets"].as_array_mut().expect("assets") {
            let url = a["browser_download_url"]
                .as_str()
                .expect("url")
                .replace("hydralauncher/hydra", "evil/other");
            a["browser_download_url"] = url.into();
        }
        let bytes = serde_json::to_vec(&json).expect("enc");
        let l = linker(
            bytes,
            Arc::new(FakeStore::new(vec![])),
            ScriptedRunner::new(),
        );
        let err = l
            .preview("hydralauncher/hydra")
            .await
            .expect_err("foreign assets must be refused");
        assert!(err.to_string().contains("different repository"), "{err}");
    }
}
