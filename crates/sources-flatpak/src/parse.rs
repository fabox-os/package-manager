//! Pure parsers for tab-separated `flatpak` output.

use std::collections::HashMap;

use package_manager_core::{Provenance, ProvenanceKind, SignatureStatus, TrustLevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatpakApp {
    pub app_id: String,
    pub name: String,
    pub version: String,
    pub remote: String,
    pub installation: String,
}

/// `flatpak list --app --columns=application,name,version,origin,installation`
pub fn parse_list(output: &str) -> Vec<FlatpakApp> {
    output
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 || f[0].is_empty() {
                return None;
            }
            Some(FlatpakApp {
                app_id: f[0].to_owned(),
                name: if f[1].is_empty() {
                    f[0].to_owned()
                } else {
                    f[1].to_owned()
                },
                version: f[2].to_owned(),
                remote: f[3].to_owned(),
                installation: f[4].to_owned(),
            })
        })
        .collect()
}

/// `flatpak remotes --columns=name,url` -> name => url
pub fn parse_remotes(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|l| {
            let (name, url) = l.split_once('\t')?;
            (!name.is_empty()).then(|| (name.to_owned(), url.trim().to_owned()))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatpakUpdate {
    pub app_id: String,
    pub version: String,
    pub remote: String,
}

/// `flatpak remote-ls --updates --app --columns=application,version,origin`
pub fn parse_updates(output: &str) -> Vec<FlatpakUpdate> {
    output
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 3 && !f[0].is_empty()).then(|| FlatpakUpdate {
                app_id: f[0].to_owned(),
                version: f[1].to_owned(),
                remote: f[2].to_owned(),
            })
        })
        .collect()
}

/// Flathub is Official; any other remote is Community. An unknown remote URL is
/// never guessed: the origin line says so.
pub fn provenance_for(remote: &str, remote_url: Option<&str>) -> Provenance {
    let is_flathub = remote_url
        .and_then(|u| u.strip_prefix("https://"))
        .map(|u| u.split('/').next().unwrap_or("").ends_with("flathub.org"))
        .unwrap_or(false);
    Provenance {
        kind: ProvenanceKind::FlatpakRemote {
            remote: remote.to_owned(),
            remote_url: remote_url.unwrap_or_default().to_owned(),
        },
        display_name: if is_flathub {
            "Flathub".to_owned()
        } else {
            remote.to_owned()
        },
        detail_url: remote_url
            .filter(|u| u.starts_with("https://"))
            .map(str::to_owned),
        trust: if is_flathub {
            TrustLevel::Official
        } else {
            TrustLevel::Community
        },
        verified_at: None,
        signature: SignatureStatus::NotChecked,
        also_available_from: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parses_and_falls_back_to_id_for_name() {
        let apps = parse_list(include_str!("../tests/fixtures/flatpak-list.txt"));
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[1].installation, "user");
        assert_eq!(apps[1].version, "");
    }

    #[test]
    fn flathub_official_other_remote_community() {
        let r = parse_remotes(include_str!("../tests/fixtures/flatpak-remotes.txt"));
        let a = provenance_for("flathub", r.get("flathub").map(String::as_str));
        let b = provenance_for("myremote", r.get("myremote").map(String::as_str));
        assert_eq!(a.trust, TrustLevel::Official);
        assert_eq!(b.trust, TrustLevel::Community);
        assert_eq!(b.display_name, "myremote");
    }

    #[test]
    fn spoofed_flathub_name_is_not_trusted() {
        // A remote *named* flathub but pointing elsewhere must not be Official.
        let p = provenance_for("flathub", Some("https://evil.example/flathub.org/"));
        assert_eq!(p.trust, TrustLevel::Community);
        let p = provenance_for("flathub", None);
        assert_eq!(p.trust, TrustLevel::Community);
    }

    #[test]
    fn updates_parse() {
        let u = parse_updates(include_str!("../tests/fixtures/flatpak-updates.txt"));
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].version, "2.10.38");
    }

    #[test]
    fn garbage_does_not_panic() {
        assert!(parse_list("\t\t\t\t\n").is_empty());
        assert!(parse_updates("x\n").is_empty());
        assert!(parse_remotes("\t\n").is_empty());
    }
}
