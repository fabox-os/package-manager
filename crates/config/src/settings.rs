use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Could not save settings to {path}: {source}")]
    Save { path: String, source: io::Error },
    #[error("Could not encode settings: {0}")]
    Encode(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub show_system_packages: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub show_terminal: bool,
    /// Open the terminal panel when an update starts, even if it is hidden.
    pub terminal_auto_show: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            show_terminal: true,
            terminal_auto_show: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub apt: bool,
    pub flatpak: bool,
    pub github: bool,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            apt: true,
            flatpak: true,
            github: true,
        }
    }
}

/// Every field has a default, so an empty, partial or older file always loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub ui: Ui,
    pub sources: Sources,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// Set when the file could not be used; shown to the user, never fatal.
    pub warning: Option<String>,
}

impl Config {
    /// Loads settings. A missing file gives the defaults silently. An unreadable or invalid
    /// file gives the defaults plus a warning, and an invalid file is moved aside to
    /// `config.toml.broken` so that saving later never destroys the user's text.
    pub fn load(path: &Path) -> LoadedConfig {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return LoadedConfig {
                    config: Self::default(),
                    warning: None,
                };
            }
            Err(e) => {
                return LoadedConfig {
                    config: Self::default(),
                    warning: Some(format!(
                        "Could not read {}: {e}. Using default settings.",
                        path.display()
                    )),
                };
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(config) => LoadedConfig {
                config,
                warning: None,
            },
            Err(e) => {
                let mut backup = path.to_path_buf().into_os_string();
                backup.push(".broken");
                let kept = match fs::rename(path, &backup) {
                    Ok(()) => format!(" Your file was kept as {}.", Path::new(&backup).display()),
                    Err(_) => String::new(),
                };
                tracing::warn!("invalid config: {e}");
                LoadedConfig {
                    config: Self::default(),
                    warning: Some(format!(
                        "Settings file is invalid ({}). Using default settings.{kept}",
                        e.message()
                    )),
                }
            }
        }
    }

    /// Writes atomically (temp file + rename), so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let text = toml::to_string_pretty(self)?;
        let fail = |source| ConfigError::Save {
            path: path.display().to_string(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(fail)?;
        }
        let mut tmp = path.to_path_buf().into_os_string();
        tmp.push(".tmp");
        fs::write(&tmp, text).map_err(fail)?;
        fs::rename(&tmp, path).map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("um-config-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn missing_file_gives_defaults_without_warning() {
        let l = Config::load(&temp_dir("missing").join("config.toml"));
        assert_eq!(l.config, Config::default());
        assert!(l.warning.is_none());
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let c = Config::default();
        assert!(c.ui.show_terminal && c.ui.terminal_auto_show);
        assert!(!c.general.show_system_packages);
        assert!(c.sources.apt && c.sources.flatpak && c.sources.github);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested/config.toml");
        let mut c = Config::default();
        c.ui.show_terminal = false;
        c.general.show_system_packages = true;
        c.sources.flatpak = false;
        c.save(&path).expect("save");
        let l = Config::load(&path);
        assert_eq!(l.config, c);
        assert!(l.warning.is_none());
        assert!(!path.with_extension("toml.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_file_fills_in_defaults_and_ignores_unknown_keys() {
        let dir = temp_dir("partial");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[ui]\nshow_terminal = false\n\n[future]\nthing = 1\n",
        )
        .expect("write");
        let l = Config::load(&path);
        assert!(!l.config.ui.show_terminal);
        assert!(l.config.ui.terminal_auto_show);
        assert!(l.warning.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_file_falls_back_and_is_kept_aside() {
        let dir = temp_dir("invalid");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("config.toml");
        fs::write(&path, "[ui\nshow_terminal = ???").expect("write");
        let l = Config::load(&path);
        assert_eq!(l.config, Config::default());
        assert!(l.warning.as_deref().is_some_and(|w| w.contains("invalid")));
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(dir.join("config.toml.broken")).expect("kept"),
            "[ui\nshow_terminal = ???"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_value_type_is_an_invalid_file_not_a_panic() {
        let dir = temp_dir("badtype");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("config.toml");
        fs::write(&path, "[ui]\nshow_terminal = \"yes\"\n").expect("write");
        assert!(Config::load(&path).warning.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_path_warns() {
        // A directory where the file should be.
        let dir = temp_dir("isdir");
        fs::create_dir_all(dir.join("config.toml")).expect("dir");
        let l = Config::load(&dir.join("config.toml"));
        assert!(l.warning.is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
