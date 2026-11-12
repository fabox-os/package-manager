//! Known apps shipped with the program (`presets.json`). Presets only *suggest* a link:
//! nothing is tracked until the user accepts.

use std::sync::OnceLock;

use package_manager_core::GithubRepo;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Preset {
    pub name: String,
    pub package_name: String,
    pub repo: String,
    pub asset_pattern: String,
}

impl Preset {
    pub fn repo(&self) -> Option<GithubRepo> {
        GithubRepo::parse(&self.repo)
    }
}

static PRESETS: OnceLock<Vec<Preset>> = OnceLock::new();

pub fn presets() -> &'static [Preset] {
    PRESETS.get_or_init(
        || match serde_json::from_str(include_str!("../presets.json")) {
            Ok(list) => list,
            Err(e) => {
                tracing::error!("presets.json is invalid: {e}");
                Vec::new()
            }
        },
    )
}

pub fn preset_for_package(package_name: &str) -> Option<&'static Preset> {
    presets().iter().find(|p| p.package_name == package_name)
}

pub fn is_preset_repo(repo: &GithubRepo) -> bool {
    presets().iter().any(|p| p.repo().as_ref() == Some(repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_presets_parse_and_have_valid_repos() {
        assert!(!presets().is_empty());
        for p in presets() {
            assert!(p.repo().is_some(), "{} has an invalid repo", p.name);
            assert!(!p.package_name.is_empty() && !p.asset_pattern.is_empty());
        }
    }

    #[test]
    fn hydra_preset_uses_the_real_package_and_asset_names() {
        let p = preset_for_package("hydralauncher").expect("hydra");
        assert_eq!(p.repo, "hydralauncher/hydra");
        assert_eq!(p.asset_pattern, "hydralauncher_{version}_amd64.deb");
        assert!(is_preset_repo(&p.repo().expect("repo")));
        assert!(!is_preset_repo(
            &GithubRepo::parse("someone/hydra").expect("fork")
        ));
        assert!(preset_for_package("unknown-package").is_none());
    }

    #[test]
    fn hydra_preset_matches_the_real_release() {
        use crate::assets::select_asset;
        use crate::release::parse_release;
        let r =
            parse_release(include_bytes!("../tests/fixtures/hydra-release.json")).expect("fixture");
        let p = preset_for_package("hydralauncher").expect("hydra");
        assert!(select_asset(&r.assets, &p.asset_pattern, r.version(), "amd64").is_some());
    }
}
