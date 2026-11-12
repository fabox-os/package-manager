//! Composition root: the only crate that knows which concrete sources exist.
//! The GUI depends on this and on `core`, never on `sources-*` directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use package_manager_config::{Database, TrackedRecord};
use package_manager_core::exec::SystemRunner;
use package_manager_core::log::{LogBroadcaster, LogEvent};
use package_manager_core::orchestrator::Orchestrator;
use package_manager_core::{
    AppSource, GithubRepo, IconRef, InstalledApp, SourceError, TrackedApp, TrackedStore,
    UpdateSource,
};
use package_manager_icons::{find_desktop_file, flatpak_desktop_dirs, read_desktop, IconResolver};
use package_manager_sources_apt::AptSource;
use package_manager_sources_flatpak::FlatpakSource;
use package_manager_sources_github::{
    Consent, DiskCache, GithubSource, HttpDownloader, HttpFetcher, InstallConfig, Linker,
};

const HELPER_NAME: &str = "package-manager-helper";
const INSTALLED_HELPER: &str = "/usr/libexec/package-manager/package-manager-helper";
const HELPER_ENV: &str = "UPDATE_MANAGER_HELPER";

/// Where the privileged helper lives.
///
/// The app asks `pkexec` to run this file as root, so a path an attacker can influence is a
/// way to root. Release builds therefore use only the installed location, and only if that
/// file (and its folder) belongs to root and cannot be modified by anyone else. Development
/// builds also accept `$UPDATE_MANAGER_HELPER` and a helper next to the executable, so
/// `cargo run` works.
pub fn locate_helper() -> Option<PathBuf> {
    let dev = cfg!(debug_assertions);
    let from_env = dev
        .then(|| std::env::var_os(HELPER_ENV).map(PathBuf::from))
        .flatten();
    let beside_exe = dev
        .then(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join(HELPER_NAME)))
        })
        .flatten();
    pick_helper(
        dev,
        [from_env, beside_exe, Some(PathBuf::from(INSTALLED_HELPER))],
    )
}

/// Root-owned and not writable by group or others.
pub fn is_root_only_writable(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

fn root_controls(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).is_ok_and(|m| is_root_only_writable(m.uid(), m.mode()))
}

fn pick_helper(
    dev: bool,
    candidates: impl IntoIterator<Item = Option<PathBuf>>,
) -> Option<PathBuf> {
    candidates.into_iter().flatten().find(|p| {
        p.is_absolute()
            && p.is_file()
            // Outside development, the file and the folder holding it must be root's.
            && (dev || (root_controls(p) && p.parent().is_some_and(root_controls)))
    })
}

/// Everything the GUI needs from the backend.
pub struct Backend {
    pub orchestrator: Arc<Orchestrator>,
    /// Terminal-panel log stream; every source reports to it.
    pub logs: LogBroadcaster,
    /// Lets the GUI record the user's "install anyway" for a release with no checksum.
    pub github_consent: Consent,
    /// "Add from GitHub". `None` when the GitHub source is off or settings cannot be stored.
    pub linker: Option<Arc<Linker>>,
}

/// Which real sources to use (from `config.toml`).
#[derive(Debug, Clone, Copy)]
pub struct Enabled {
    pub apt: bool,
    pub flatpak: bool,
    pub github: bool,
}

/// Storage the sources need.
#[derive(Default)]
pub struct Services {
    /// Linked GitHub apps live here. Without it the GitHub source has nothing to check.
    pub db: Option<Database>,
    pub github_cache_dir: Option<PathBuf>,
    /// Where verified GitHub downloads are stored before installing.
    pub downloads_dir: Option<PathBuf>,
}

/// Linked GitHub apps, read from the SQLite registry off the async threads.
struct DbTrackedStore(Database);

#[async_trait]
impl TrackedStore for DbTrackedStore {
    async fn list(&self) -> Result<Vec<TrackedApp>, SourceError> {
        let db = self.0.clone();
        let rows = tokio::task::spawn_blocking(move || db.list_tracked())
            .await
            .map_err(|e| SourceError::Other(format!("Could not read linked apps: {e}")))?
            .map_err(|e| SourceError::Other(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let Some(repo) = GithubRepo::parse(&r.github_repo) else {
                    tracing::warn!(
                        "ignoring linked app {} with invalid repo {:?}",
                        r.package_name,
                        r.github_repo
                    );
                    return None;
                };
                Some(TrackedApp {
                    package_name: r.package_name,
                    repo,
                    asset_pattern: r.asset_pattern,
                    is_preset: r.is_preset,
                })
            })
            .collect())
    }

    async fn save(&self, app: &TrackedApp) -> Result<(), SourceError> {
        let db = self.0.clone();
        let record = TrackedRecord {
            package_name: app.package_name.clone(),
            github_repo: app.repo.full_name(),
            asset_pattern: app.asset_pattern.clone(),
            is_preset: app.is_preset,
        };
        tokio::task::spawn_blocking(move || db.upsert_tracked(&record))
            .await
            .map_err(|e| SourceError::Other(format!("Could not save the link: {e}")))?
            .map_err(|e| SourceError::Other(e.to_string()))
    }
}

/// Builds the backend with the real sources.
pub fn build(enabled: Enabled, services: Services) -> Backend {
    let logs = LogBroadcaster::new();
    let helper = locate_helper();
    let github_consent = Consent::default();
    let mut sources: Vec<Arc<dyn UpdateSource>> = Vec::new();
    match helper.clone().filter(|_| enabled.apt) {
        Some(helper) => {
            tracing::info!(helper = %helper.display(), "using privileged helper");
            let runner = SystemRunner::new(logs.clone(), "APT");
            sources.push(Arc::new(AptSource::new(Box::new(runner), helper)));
        }
        None if !enabled.apt => tracing::info!("APT source disabled in settings"),
        None => {
            // Without the helper, APT updates could be listed but never installed.
            // Listing them anyway would promise something the app cannot do.
            tracing::warn!("{HELPER_NAME} not found; APT source disabled");
            logs.emit(LogEvent::output(
                None,
                "APT updates are unavailable: the update helper was not found.",
            ));
        }
    }
    if enabled.flatpak {
        let runner = SystemRunner::new(logs.clone(), "Flatpak");
        sources.push(Arc::new(FlatpakSource::new(Box::new(runner))));
    }
    let mut linker = None;
    if let (true, Some(db)) = (enabled.github, services.db) {
        match HttpFetcher::new() {
            Ok(fetcher) => {
                let fetcher = Arc::new(fetcher);
                let store: Arc<dyn TrackedStore> = Arc::new(DbTrackedStore(db));
                // The linker and the update checker share the fetcher and the ETag cache folder.
                linker = Some(Arc::new(Linker::new(
                    fetcher.clone(),
                    services.github_cache_dir.clone().map(DiskCache::new),
                    store.clone(),
                    Box::new(SystemRunner::new(logs.clone(), "GitHub")),
                )));
                let runner = SystemRunner::new(logs.clone(), "GitHub");
                let mut source = GithubSource::new(
                    fetcher,
                    services.github_cache_dir.map(DiskCache::new),
                    store,
                    Box::new(runner),
                );
                match (HttpDownloader::new(), services.downloads_dir) {
                    (Ok(downloader), Some(downloads_dir)) => {
                        source = source.with_install(InstallConfig {
                            downloader: Arc::new(downloader),
                            downloads_dir,
                            helper: helper.clone(),
                            consent: github_consent.clone(),
                        });
                    }
                    (Err(e), _) => tracing::warn!("GitHub installs disabled: {e}"),
                    (_, None) => tracing::warn!("GitHub installs disabled: no download folder"),
                }
                sources.push(Arc::new(source));
            }
            Err(e) => tracing::warn!("GitHub source disabled: {e}"),
        }
    }
    Backend {
        orchestrator: Arc::new(Orchestrator::new(sources)),
        logs,
        github_consent,
        linker,
    }
}

/// Builds the backend with the deterministic fake source (UI development).
#[cfg(feature = "fake")]
pub fn build_fake() -> Backend {
    Backend {
        orchestrator: Arc::new(Orchestrator::new(vec![Arc::new(
            package_manager_core::fake::FakeSource,
        )])),
        logs: LogBroadcaster::new(),
        github_consent: Consent::default(),
        linker: None,
    }
}

/// Display names and icons come from `.desktop` files and icon folders.
pub struct Enricher {
    flatpak_dirs: Vec<PathBuf>,
    resolver: IconResolver,
}

impl Enricher {
    pub fn system_default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute());
        Self {
            flatpak_dirs: flatpak_desktop_dirs(home.as_deref()),
            resolver: IconResolver::system_default(home.as_deref()),
        }
    }

    pub fn new(flatpak_dirs: Vec<PathBuf>, resolver: IconResolver) -> Self {
        Self {
            flatpak_dirs,
            resolver,
        }
    }

    /// Replaces package names with the app's real name and fills in its icon. Apps without
    /// a usable desktop entry are returned unchanged (they keep the letter-tile icon).
    /// Reads files, so call it off the UI thread.
    pub fn enrich(&self, apps: Vec<InstalledApp>) -> Vec<InstalledApp> {
        apps.into_iter().map(|app| self.enrich_one(app)).collect()
    }

    fn enrich_one(&self, mut app: InstalledApp) -> InstalledApp {
        let desktop = app.desktop_file.clone().or_else(|| match &app.source {
            AppSource::Flatpak { app_id, .. } => find_desktop_file(app_id, &self.flatpak_dirs),
            _ => None,
        });
        let Some(path) = desktop else { return app };
        let Some(entry) = read_desktop(&path).filter(|e| !e.hidden) else {
            return app;
        };
        app.icon = entry
            .icon
            .as_deref()
            .and_then(|i| self.resolver.resolve(i))
            .map_or_else(|| IconRef::initial_for(&entry.name), IconRef::File);
        app.name = entry.name;
        app.desktop_file = Some(path);
        app
    }
}

/// Types the GUI needs for "Add from GitHub" (the app never depends on `sources-*` directly).
pub use package_manager_sources_github::{
    AssetChoice, ChecksumKind, ChecksumState, Consent as GithubConsent, Linker as GithubLinker,
    RepoPreview, NOT_INSTALLED,
};

#[cfg(test)]
mod tests {
    use super::*;

    use package_manager_core::{Provenance, UpdateKind};
    use std::fs;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("um-enrich-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("mkdir");
        d
    }

    fn app(id: &str, name: &str, source: AppSource, desktop: Option<PathBuf>) -> InstalledApp {
        InstalledApp {
            id: id.into(),
            name: name.into(),
            version: "1".into(),
            source,
            provenance: Provenance::unknown(),
            icon: IconRef::initial_for(name),
            desktop_file: desktop,
            homepage: None,
            installed_at: None,
            is_system: false,
        }
    }

    #[test]
    fn apt_app_gets_display_name_and_icon_from_its_desktop_file() {
        let root = dir("apt");
        fs::write(
            root.join("firefox.desktop"),
            "[Desktop Entry]\nType=Application\nName=Firefox Web Browser\nIcon=firefox\n",
        )
        .expect("desktop");
        let icons = root.join("icons");
        fs::create_dir_all(icons.join("hicolor/48x48/apps")).expect("mk");
        fs::write(icons.join("hicolor/48x48/apps/firefox.png"), b"x").expect("icon");
        let e = Enricher::new(vec![], IconResolver::new(vec![icons.clone()]));
        let out = e.enrich(vec![app(
            "apt:firefox",
            "firefox",
            AppSource::Apt {
                package_name: "firefox".into(),
            },
            Some(root.join("firefox.desktop")),
        )]);
        assert_eq!(out[0].name, "Firefox Web Browser");
        assert_eq!(
            out[0].icon,
            IconRef::File(icons.join("hicolor/48x48/apps/firefox.png"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn flatpak_app_is_found_by_app_id_and_missing_icon_falls_back_to_a_letter() {
        let root = dir("flatpak");
        fs::write(
            root.join("org.gimp.GIMP.desktop"),
            "[Desktop Entry]\nType=Application\nName=GNU Image Manipulation Program\nIcon=org.gimp.GIMP\n",
        )
        .expect("desktop");
        let e = Enricher::new(vec![root.clone()], IconResolver::new(vec![]));
        let src = AppSource::Flatpak {
            app_id: "org.gimp.GIMP".into(),
            remote: "flathub".into(),
        };
        let out = e.enrich(vec![app("flatpak:org.gimp.GIMP", "GIMP", src, None)]);
        assert_eq!(out[0].name, "GNU Image Manipulation Program");
        assert_eq!(out[0].icon, IconRef::Initial('G'));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hidden_or_missing_desktop_entries_leave_the_app_unchanged() {
        let root = dir("hidden");
        fs::write(
            root.join("x.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden Thing\nNoDisplay=true\n",
        )
        .expect("desktop");
        let e = Enricher::new(vec![], IconResolver::new(vec![]));
        let hidden = app(
            "apt:x",
            "x",
            AppSource::Apt {
                package_name: "x".into(),
            },
            Some(root.join("x.desktop")),
        );
        let gone = app(
            "apt:y",
            "y",
            AppSource::Apt {
                package_name: "y".into(),
            },
            Some(root.join("nope.desktop")),
        );
        let none = app(
            "apt:z",
            "z",
            AppSource::Apt {
                package_name: "z".into(),
            },
            None,
        );
        let out = e.enrich(vec![hidden.clone(), gone.clone(), none.clone()]);
        assert_eq!(out, vec![hidden, gone, none]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn enrichment_does_not_change_ids_or_update_kinds() {
        let a = app(
            "apt:x",
            "x",
            AppSource::Apt {
                package_name: "x".into(),
            },
            None,
        );
        let out = Enricher::new(vec![], IconResolver::new(vec![])).enrich(vec![a.clone()]);
        assert_eq!(out[0].id, a.id);
        assert_eq!(out[0].source.kind(), UpdateKind::Apt);
    }

    #[test]
    fn relative_and_missing_helper_paths_are_skipped() {
        assert_eq!(
            pick_helper(
                true,
                [
                    Some(PathBuf::from("helper")),
                    None,
                    Some(PathBuf::from("/nonexistent/x"))
                ]
            ),
            None
        );
    }

    #[test]
    fn first_existing_absolute_helper_wins() {
        let exe = std::env::current_exe().expect("exe");
        let picked = pick_helper(
            true,
            [Some(PathBuf::from("/nonexistent/x")), Some(exe.clone())],
        );
        assert_eq!(picked, Some(exe));
    }

    #[test]
    fn release_builds_refuse_a_helper_the_user_could_have_replaced() {
        // The test executable is owned by the current user: fine for `cargo run`, never for
        // a file that pkexec will run as root.
        let exe = std::env::current_exe().expect("exe");
        assert_eq!(pick_helper(false, [Some(exe.clone())]), None);
        assert_eq!(pick_helper(true, [Some(exe.clone())]), Some(exe));
        // A root-owned program in a root-owned folder is accepted.
        assert_eq!(
            pick_helper(false, [Some(PathBuf::from("/usr/bin/env"))]),
            Some(PathBuf::from("/usr/bin/env"))
        );
    }

    #[test]
    fn only_root_owned_and_non_shared_writable_files_are_trusted() {
        assert!(is_root_only_writable(0, 0o755));
        assert!(is_root_only_writable(0, 0o4755));
        assert!(!is_root_only_writable(1000, 0o755), "owned by a user");
        assert!(!is_root_only_writable(0, 0o775), "group-writable");
        assert!(!is_root_only_writable(0, 0o757), "world-writable");
        assert!(!is_root_only_writable(0, 0o666));
    }

    #[test]
    fn the_polkit_policy_matches_the_installed_helper_and_never_caches_authorization() {
        let raw = include_str!("../../../data/polkit/org.packagemanager.helper.policy");
        // Judge only the real settings, not the explanatory comment.
        let (start, end) = (raw.find("<!--"), raw.find("-->"));
        let policy = match (start, end) {
            (Some(a), Some(b)) => format!("{}{}", &raw[..a], &raw[b + 3..]),
            _ => raw.to_owned(),
        };
        assert!(
            policy.contains(&format!(">{INSTALLED_HELPER}<")),
            "the policy must name exactly the path the app runs through pkexec"
        );
        assert!(policy.contains("<allow_active>auth_admin</allow_active>"));
        assert!(
            !policy.contains("auth_admin_keep"),
            "a kept authorization can be reused silently"
        );
        assert!(
            policy.contains("<allow_any>no</allow_any>")
                && policy.contains("<allow_inactive>no</allow_inactive>")
        );
    }
}
