use std::path::PathBuf;

use chrono::{DateTime, Utc};

/// A GitHub repository, `owner/repo`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubRepo {
    pub owner: String,
    pub repo: String,
}

impl GithubRepo {
    /// Parses `owner/repo`. Returns `None` for anything else (no panics on user input).
    pub fn parse(input: &str) -> Option<Self> {
        let (owner, repo) = input.trim().split_once('/')?;
        let valid = |s: &str| {
            // `.` and `..` are never real names and would be path traversal in cache file names.
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.len() <= 100
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        (valid(owner) && valid(repo)).then(|| Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppSource {
    Apt {
        package_name: String,
    },
    Flatpak {
        app_id: String,
        remote: String,
    },
    Github {
        repo: GithubRepo,
        asset_pattern: String,
    },
    ManualDeb {
        package_name: String,
        linked_repo: Option<GithubRepo>,
    },
}

impl AppSource {
    pub fn kind(&self) -> UpdateKind {
        match self {
            Self::Apt { .. } => UpdateKind::Apt,
            Self::Flatpak { .. } => UpdateKind::Flatpak,
            Self::Github { .. } => UpdateKind::Github,
            Self::ManualDeb { .. } => UpdateKind::ManualDeb,
        }
    }
}

/// Order matters: it is the plan order (APT -> Flatpak -> GitHub), see ARCHITECTURE.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UpdateKind {
    Apt,
    Flatpak,
    Github,
    ManualDeb,
}

impl UpdateKind {
    /// The `UpdateSource::kind()` name of the source that applies this kind of update.
    pub fn source_name(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Flatpak => "flatpak",
            Self::Github => "github",
            Self::ManualDeb => "manual",
        }
    }

    /// Badge text shown in the list row.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Apt => "APT",
            Self::Flatpak => "Flatpak",
            Self::Github => "GitHub",
            Self::ManualDeb => "Manual .deb",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    Official,
    Verified,
    Community,
    Unknown,
}

impl TrustLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Official => "Official",
            Self::Verified => "Verified",
            Self::Community => "Community",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureStatus {
    Verified,
    NotChecked,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceKind {
    AptRepo {
        repo: String,
        component: String,
        origin: String,
    },
    FlatpakRemote {
        remote: String,
        remote_url: String,
    },
    Github {
        owner: String,
        repo: String,
        release_url: String,
    },
    ManualDeb {
        reason: String,
    },
    Unknown,
}

/// The real origin of an app. Never guessed: when unproven it is [`Provenance::unknown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub kind: ProvenanceKind,
    pub display_name: String,
    pub detail_url: Option<String>,
    pub trust: TrustLevel,
    pub verified_at: Option<DateTime<Utc>>,
    pub signature: SignatureStatus,
    pub also_available_from: Vec<Provenance>,
}

impl Provenance {
    pub fn unknown() -> Self {
        Self {
            kind: ProvenanceKind::Unknown,
            display_name: "Unknown".to_owned(),
            detail_url: None,
            trust: TrustLevel::Unknown,
            verified_at: None,
            signature: SignatureStatus::NotChecked,
            also_available_from: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconRef {
    Theme(String),
    File(PathBuf),
    /// Only before download; never rendered directly.
    Url(String),
    Initial(char),
}

impl IconRef {
    /// Last-resort icon: first letter of the app name.
    pub fn initial_for(name: &str) -> Self {
        Self::Initial(
            name.chars()
                .find(|c| c.is_alphanumeric())
                .map(|c| c.to_ascii_uppercase())
                .unwrap_or('?'),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    /// Stable id, e.g. `apt:hydra`, `flatpak:org.gimp.GIMP`, `github:hydralauncher/hydra`.
    pub id: String,
    pub name: String,
    /// Installed version exactly as reported by the source.
    pub version: String,
    pub source: AppSource,
    pub provenance: Provenance,
    pub icon: IconRef,
    pub desktop_file: Option<PathBuf>,
    pub homepage: Option<String>,
    pub installed_at: Option<DateTime<Utc>>,
    /// Not something the user installed on purpose (e.g. an auto-installed library).
    /// Hidden from the list by default but still updated.
    pub is_system: bool,
}

/// Debian policy for package names: 2-128 chars, lowercase letters, digits and `+ - .`,
/// starting with a letter or digit. Used before a name is passed to any command.
pub fn is_valid_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && (2..=128).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
}

/// A package the user linked to a GitHub repository. Only linked repos are ever checked or
/// installed from; nothing is linked automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedApp {
    /// The installed Debian package this repo's releases update.
    pub package_name: String,
    pub repo: GithubRepo,
    /// e.g. `hydralauncher_{version}_amd64.deb`
    pub asset_pattern: String,
    /// Came from the shipped presets list (as opposed to a repo the user typed in).
    pub is_preset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCandidate {
    pub app: InstalledApp,
    pub latest_version: String,
    pub changelog_url: Option<String>,
    pub download_url: Option<String>,
    pub size_bytes: Option<u64>,
    pub is_security: bool,
    pub source_kind: UpdateKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_repo() {
        let r = GithubRepo::parse(" hydralauncher/hydra ").expect("valid");
        assert_eq!(r.full_name(), "hydralauncher/hydra");
    }

    #[test]
    fn package_name_rule_matches_debian_policy() {
        for ok in ["hydralauncher", "libstdc++6", "python3.12", "0ad"] {
            assert!(is_valid_package_name(ok), "{ok}");
        }
        for bad in ["", "a", "-x", "Hydra", "a b", "a/b", "../x", "a;b", "é1"] {
            assert!(!is_valid_package_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn rejects_dot_names_that_would_escape_a_cache_folder() {
        for bad in [
            "../x",
            "x/..",
            "./x",
            "x/.",
            "../..",
            "a/b/..",
            &format!("{}/x", "a".repeat(101)),
        ] {
            assert!(
                GithubRepo::parse(bad).is_none(),
                "{bad:?} should be rejected"
            );
        }
        assert!(GithubRepo::parse("a.b/c.d-e_f").is_some());
    }

    #[test]
    fn rejects_bad_repo_names() {
        for bad in [
            "",
            "hydra",
            "a/",
            "/b",
            "a/b/c",
            "a b/c",
            "a/b;rm",
            "http://x/y",
        ] {
            assert!(
                GithubRepo::parse(bad).is_none(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn initial_icon_falls_back() {
        assert_eq!(IconRef::initial_for("hydra"), IconRef::Initial('H'));
        assert_eq!(IconRef::initial_for("  "), IconRef::Initial('?'));
    }

    #[test]
    fn unknown_provenance_is_explicitly_unknown() {
        let p = Provenance::unknown();
        assert_eq!(p.trust, TrustLevel::Unknown);
        assert_eq!(p.kind, ProvenanceKind::Unknown);
    }

    #[test]
    fn plan_order_is_apt_flatpak_github() {
        assert!(UpdateKind::Apt < UpdateKind::Flatpak);
        assert!(UpdateKind::Flatpak < UpdateKind::Github);
    }
}
