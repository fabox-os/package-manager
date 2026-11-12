//! Pure parsers for `dpkg-query`, `apt-mark`, `apt list` and `apt-cache policy` output.

use std::collections::HashMap;
use std::path::PathBuf;

use package_manager_core::{Provenance, ProvenanceKind, SignatureStatus, TrustLevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    pub name: String,
    pub version: String,
}

/// Parses `dpkg-query -W -f='${Package}\t${Version}\t${db:Status-Abbrev}\n'`.
/// Only fully installed packages (`ii`) are returned.
pub fn parse_dpkg_query(output: &str) -> Vec<InstalledPackage> {
    output
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let (name, version, status) = (f.next()?, f.next()?, f.next()?);
            (status.starts_with("ii") && !name.is_empty()).then(|| InstalledPackage {
                name: name.to_owned(),
                version: version.to_owned(),
            })
        })
        .collect()
}

pub fn parse_manual_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgradable {
    pub name: String,
    pub new_version: String,
    pub old_version: Option<String>,
    pub is_security: bool,
}

/// Parses `apt list --upgradable`.
/// Line: `name/suite1,suite2 new-version arch [upgradable from: old-version]`.
pub fn parse_upgradable(output: &str) -> Vec<Upgradable> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (head, new_version) = (parts.next()?, parts.next()?);
            let (name, suites) = head.split_once('/')?;
            let old_version = line
                .split_once("[upgradable from: ")
                .and_then(|(_, rest)| rest.strip_suffix(']'))
                .map(str::to_owned);
            Some(Upgradable {
                name: name.to_owned(),
                new_version: new_version.to_owned(),
                old_version,
                is_security: suites.split(',').any(|s| s.ends_with("-security")),
            })
        })
        .collect()
}

/// Parses `dpkg-query -S '/usr/share/applications/*.desktop'` into package -> desktop file.
/// Line: `pkg[:arch][, pkg2...]: /path/to/file.desktop`. When a package owns several
/// desktop files (e.g. settings panels) the one named after the package wins, otherwise the
/// first by path, so the result is deterministic.
pub fn parse_desktop_files(output: &str) -> HashMap<String, PathBuf> {
    let mut all: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for line in output.lines() {
        let Some((pkgs, path)) = line.split_once(": /") else {
            continue;
        };
        let path = PathBuf::from(format!("/{path}"));
        if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
            continue;
        }
        for pkg in pkgs.split(", ") {
            // Drop a multi-arch suffix: `libfoo:amd64` -> `libfoo`.
            let name = pkg.split(':').next().unwrap_or(pkg).trim();
            if !name.is_empty() {
                all.entry(name.to_owned()).or_default().push(path.clone());
            }
        }
    }
    all.into_iter()
        .filter_map(|(pkg, mut files)| {
            files.sort();
            let named_after_pkg = files
                .iter()
                .find(|f| f.file_stem().and_then(|s| s.to_str()) == Some(pkg.as_str()))
                .cloned();
            named_after_pkg
                .or_else(|| files.into_iter().next())
                .map(|f| (pkg, f))
        })
        .collect()
}

/// Parses multi-package `apt-cache policy` output and returns the provenance of each package.
///
/// Origin of the installed version if a repo still carries it. If none does (the archive
/// moved on) but a repo offers a *different* candidate version, that repo is the origin APT
/// will update from. Only a package no repo knows about, or one newer than every repo,
/// gets the explicit manual-.deb / Unknown provenance.
pub fn parse_policy(output: &str) -> HashMap<String, Provenance> {
    #[derive(Default)]
    struct Block {
        name: String,
        installed: Option<String>,
        candidate: Option<String>,
        current: Option<String>,
        // version -> first repo source (url, suite/component)
        sources: HashMap<String, (String, String)>,
    }

    fn finish(block: Block, result: &mut HashMap<String, Provenance>) {
        let Some(installed) = block.installed.as_ref().filter(|_| !block.name.is_empty()) else {
            return;
        };
        let origin = block.sources.get(installed).cloned().or_else(|| {
            let cand = block.candidate.as_ref().filter(|c| *c != installed)?;
            block.sources.get(cand).cloned()
        });
        result.insert(block.name, classify(origin));
    }

    let mut result = HashMap::new();
    let mut block: Option<Block> = None;

    for line in output.lines() {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if indent == 0 && t.ends_with(':') {
            if let Some(done) = block.take() {
                finish(done, &mut result);
            }
            block = Some(Block {
                name: t
                    .trim_end_matches(':')
                    .split(':')
                    .next()
                    .unwrap_or(t)
                    .to_owned(),
                ..Block::default()
            });
            continue;
        }
        let Some(b) = block.as_mut() else { continue };
        if let Some(v) = t.strip_prefix("Installed:") {
            let v = v.trim();
            b.installed = (!v.is_empty() && v != "(none)").then(|| v.to_owned());
        } else if let Some(v) = t.strip_prefix("Candidate:") {
            let v = v.trim();
            b.candidate = (!v.is_empty() && v != "(none)").then(|| v.to_owned());
        } else if indent <= 5 && t.contains(char::is_numeric) && !t.ends_with(':') {
            // A version-table entry: `[***] <version> <priority>`.
            b.current = t
                .trim_start_matches("***")
                .split_whitespace()
                .next()
                .map(str::to_owned);
        } else if indent >= 8 {
            let mut it = t.split_whitespace();
            let (_prio, url, suite) = (it.next(), it.next(), it.next());
            if let (Some(v), Some(url), Some(suite)) = (b.current.clone(), url, suite) {
                if url.starts_with("http://") || url.starts_with("https://") {
                    b.sources
                        .entry(v)
                        .or_insert_with(|| (url.to_owned(), suite.to_owned()));
                }
            }
        }
    }
    if let Some(done) = block {
        finish(done, &mut result);
    }
    result
}

fn host_and_path(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split_once('/').unwrap_or((rest, ""))
}

fn classify(origin: Option<(String, String)>) -> Provenance {
    let Some((url, suite)) = origin else {
        return Provenance {
            kind: ProvenanceKind::ManualDeb {
                reason: "No APT origin found".to_owned(),
            },
            display_name: "Unknown — manual .deb, no repo found".to_owned(),
            detail_url: None,
            trust: TrustLevel::Unknown,
            verified_at: None,
            signature: SignatureStatus::NotChecked,
            also_available_from: Vec::new(),
        };
    };
    let (host, path) = host_and_path(&url);
    let (repo, trust, display) = if host.ends_with("ubuntu.com") {
        (
            "Ubuntu".to_owned(),
            TrustLevel::Official,
            format!("Ubuntu {suite}"),
        )
    } else if host.ends_with("debian.org") {
        (
            "Debian".to_owned(),
            TrustLevel::Official,
            format!("Debian {suite}"),
        )
    } else if host.starts_with("ppa.launchpad") {
        let ppa: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).take(2).collect();
        let name = format!("PPA {}", ppa.join("/"));
        (name.clone(), TrustLevel::Community, name)
    } else {
        (
            host.to_owned(),
            TrustLevel::Community,
            format!("{host} {suite}"),
        )
    };
    Provenance {
        kind: ProvenanceKind::AptRepo {
            repo: repo.clone(),
            component: suite,
            origin: repo,
        },
        display_name: display,
        detail_url: Some(url),
        trust,
        verified_at: None,
        // APT verifies repo signatures itself; this app has not checked them.
        signature: SignatureStatus::NotChecked,
        also_available_from: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DPKG: &str = include_str!("../tests/fixtures/dpkg-query.txt");
    const MANUAL: &str = include_str!("../tests/fixtures/apt-mark-showmanual.txt");
    const UPGRADABLE: &str = include_str!("../tests/fixtures/apt-list-upgradable.txt");
    const POLICY: &str = include_str!("../tests/fixtures/apt-cache-policy.txt");

    #[test]
    fn dpkg_skips_removed_but_configured_packages() {
        let pkgs = parse_dpkg_query(DPKG);
        assert_eq!(pkgs.len(), 5);
        assert!(pkgs.iter().all(|p| p.name != "somelib"));
        assert_eq!(pkgs[0].version, "131.0+build1-0ubuntu0.24.04.1");
    }

    #[test]
    fn manual_list_parses() {
        assert_eq!(parse_manual_list(MANUAL).len(), 4);
    }

    #[test]
    fn upgradable_detects_security_and_old_version() {
        let u = parse_upgradable(UPGRADABLE);
        assert_eq!(u.len(), 2);
        assert!(u[0].is_security);
        assert!(!u[1].is_security);
        assert_eq!(u[1].old_version.as_deref(), Some("2.0"));
    }

    #[test]
    fn policy_official_repo_is_official_for_installed_version() {
        let p = parse_policy(POLICY);
        let ff = &p["firefox"];
        // The installed version came from noble/main, not the newer noble-updates entry.
        assert_eq!(ff.trust, TrustLevel::Official);
        assert_eq!(ff.display_name, "Ubuntu noble/main");
        assert_eq!(p["vlc"].display_name, "Ubuntu noble/universe");
    }

    #[test]
    fn policy_ppa_is_community() {
        let p = parse_policy(POLICY);
        assert_eq!(p["hydra"].trust, TrustLevel::Community);
        assert_eq!(p["hydra"].display_name, "PPA someone/tools");
    }

    #[test]
    fn policy_without_repo_is_unknown_not_faked() {
        let p = parse_policy(POLICY);
        assert_eq!(p["mypkg"].trust, TrustLevel::Unknown);
        assert!(matches!(p["mypkg"].kind, ProvenanceKind::ManualDeb { .. }));
        assert!(p["mypkg"].detail_url.is_none());
    }

    #[test]
    fn superseded_installed_version_uses_candidate_repo() {
        let out = "oldpkg:\n  Installed: 0.1\n  Candidate: 0.2\n  Version table:\n     0.2 500\n        500 http://archive.ubuntu.com/ubuntu noble-updates/main amd64 Packages\n *** 0.1 100\n        100 /var/lib/dpkg/status\n";
        let p = parse_policy(out);
        assert_eq!(p["oldpkg"].trust, TrustLevel::Official);
        assert_eq!(p["oldpkg"].display_name, "Ubuntu noble-updates/main");
    }

    #[test]
    fn locally_installed_newer_than_repo_stays_unknown() {
        // Candidate == installed and no repo carries it: a hand-installed .deb.
        let out = "hydra:\n  Installed: 3.0\n  Candidate: 3.0\n  Version table:\n *** 3.0 100\n        100 /var/lib/dpkg/status\n     2.0 500\n        500 http://archive.ubuntu.com/ubuntu noble/main amd64 Packages\n";
        let p = parse_policy(out);
        assert_eq!(p["hydra"].trust, TrustLevel::Unknown);
    }

    #[test]
    fn desktop_files_prefer_the_one_named_after_the_package() {
        let m = parse_desktop_files(include_str!("../tests/fixtures/dpkg-search-desktop.txt"));
        assert_eq!(
            m["firefox"],
            PathBuf::from("/usr/share/applications/firefox.desktop")
        );
        assert_eq!(
            m["gnome-control-center"],
            PathBuf::from("/usr/share/applications/gnome-control-center.desktop")
        );
    }

    #[test]
    fn desktop_files_handle_multiarch_and_shared_files() {
        let m = parse_desktop_files(include_str!("../tests/fixtures/dpkg-search-desktop.txt"));
        let viewer = PathBuf::from("/usr/share/applications/foo-viewer.desktop");
        assert_eq!(m["libfoo"], viewer);
        assert_eq!(m["libfoo-data"], viewer);
    }

    #[test]
    fn desktop_files_ignore_junk() {
        assert!(parse_desktop_files(
            "dpkg-query: no path found matching pattern x
: /x.desktop
pkg: /etc/passwd
"
        )
        .is_empty());
    }

    #[test]
    fn garbage_input_does_not_panic() {
        assert!(parse_dpkg_query("\u{0}\t\t").is_empty());
        assert!(parse_upgradable("///\n/ \n").is_empty());
        assert!(parse_policy(":\n  Installed:\n").is_empty());
    }
}
