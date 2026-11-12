//! Settings (`config.toml`), file locations and the SQLite database.
//! No UI and no update-source code lives here.

mod db;
mod settings;

pub use db::{Database, DbError, HistoryEntry, NewHistory, TrackedRecord};
pub use settings::{Config, ConfigError, General, LoadedConfig, Sources, Ui};

use std::path::PathBuf;

/// Where everything lives: `~/.config`, `~/.local/share` and `~/.cache` (XDG aware).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub database_file: PathBuf,
    pub last_run_log: PathBuf,
    /// ETag-validated GitHub release responses (`~/.cache/package-manager/github-cache`).
    pub github_cache_dir: PathBuf,
    /// `~/.cache/package-manager/downloads`. Deliberately fixed (not `XDG_CACHE_HOME`): the
    /// privileged helper only installs files from exactly this folder of the calling user.
    pub downloads_dir: PathBuf,
}

/// The folder name used before the project was renamed.
const LEGACY_NAME: &str = "update-manager";

/// Moves `old` to `new` when only `old` exists. Failure leaves both untouched: the app then
/// simply starts with fresh settings.
fn migrate_legacy_dir(old: &std::path::Path, new: &std::path::Path) {
    if !old.is_dir() || new.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::rename(old, new) {
        tracing::warn!("could not move {} to {}: {e}", old.display(), new.display());
    }
}

impl Paths {
    /// `None` only when the home directory cannot be determined.
    pub fn discover() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("", "", "package-manager")?;
        let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
        // Earlier versions were called `update-manager`; keep the user's settings and links.
        if let Some(old) = directories::ProjectDirs::from("", "", LEGACY_NAME) {
            migrate_legacy_dir(old.config_dir(), dirs.config_dir());
            migrate_legacy_dir(old.data_dir(), dirs.data_dir());
            migrate_legacy_dir(old.cache_dir(), dirs.cache_dir());
        }
        Some(Self::in_dirs(
            dirs.config_dir(),
            dirs.data_dir(),
            dirs.cache_dir(),
            &home,
        ))
    }

    pub fn in_dirs(
        config_dir: &std::path::Path,
        data_dir: &std::path::Path,
        cache_dir: &std::path::Path,
        home: &std::path::Path,
    ) -> Self {
        Self {
            config_file: config_dir.join("config.toml"),
            database_file: config_dir.join("tracked.db"),
            last_run_log: data_dir.join("logs").join("last-run.log"),
            github_cache_dir: cache_dir.join("github-cache"),
            downloads_dir: home.join(".cache/package-manager/downloads"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_folders_are_moved_once_and_never_overwrite_new_ones() {
        let base = std::env::temp_dir().join(format!("pm-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (old, new) = (base.join("old"), base.join("new"));
        std::fs::create_dir_all(&old).expect("mk");
        std::fs::write(old.join("config.toml"), "x").expect("write");
        migrate_legacy_dir(&old, &new);
        assert!(new.join("config.toml").is_file() && !old.exists());
        // A second old folder must not replace what already exists.
        std::fs::create_dir_all(&old).expect("mk");
        std::fs::write(old.join("config.toml"), "other").expect("write");
        migrate_legacy_dir(&old, &new);
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).expect("read"),
            "x"
        );
        assert!(old.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn layout_matches_the_architecture_doc() {
        let p = Paths::in_dirs(
            "/c/package-manager".as_ref(),
            "/d/package-manager".as_ref(),
            "/x/package-manager".as_ref(),
            "/home/u".as_ref(),
        );
        assert_eq!(
            p.config_file,
            PathBuf::from("/c/package-manager/config.toml")
        );
        assert_eq!(
            p.database_file,
            PathBuf::from("/c/package-manager/tracked.db")
        );
        assert_eq!(
            p.last_run_log,
            PathBuf::from("/d/package-manager/logs/last-run.log")
        );
        assert_eq!(
            p.github_cache_dir,
            PathBuf::from("/x/package-manager/github-cache")
        );
        assert_eq!(
            p.downloads_dir,
            PathBuf::from("/home/u/.cache/package-manager/downloads")
        );
    }
}
