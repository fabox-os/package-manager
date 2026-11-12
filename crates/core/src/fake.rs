//! Deterministic fake source for UI development and tests (feature `fake`).

use async_trait::async_trait;

use crate::domain::*;
use crate::source::*;

pub struct FakeSource;

fn app(id: &str, name: &str, version: &str, source: AppSource, prov: Provenance) -> InstalledApp {
    InstalledApp {
        id: id.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
        source,
        provenance: prov,
        icon: IconRef::initial_for(name),
        desktop_file: None,
        homepage: None,
        installed_at: None,
        is_system: false,
    }
}

fn prov(kind: ProvenanceKind, name: &str, trust: TrustLevel, sig: SignatureStatus) -> Provenance {
    Provenance {
        kind,
        display_name: name.to_owned(),
        detail_url: None,
        trust,
        verified_at: None,
        signature: sig,
        also_available_from: Vec::new(),
    }
}

impl FakeSource {
    pub fn apps() -> Vec<InstalledApp> {
        let apt = |repo: &str, comp: &str, trust| {
            prov(
                ProvenanceKind::AptRepo {
                    repo: repo.to_owned(),
                    component: comp.to_owned(),
                    origin: repo.to_owned(),
                },
                &format!("{repo} {comp}"),
                trust,
                SignatureStatus::Verified,
            )
        };
        vec![
            app(
                "apt:firefox",
                "Firefox",
                "131.0",
                AppSource::Apt {
                    package_name: "firefox".into(),
                },
                apt("Ubuntu", "noble/main", TrustLevel::Official),
            ),
            app(
                "apt:vlc",
                "VLC media player",
                "3.0.20",
                AppSource::Apt {
                    package_name: "vlc".into(),
                },
                apt("Ubuntu", "noble/universe", TrustLevel::Official),
            ),
            app(
                "flatpak:org.gimp.GIMP",
                "GIMP",
                "2.10.36",
                AppSource::Flatpak {
                    app_id: "org.gimp.GIMP".into(),
                    remote: "flathub".into(),
                },
                prov(
                    ProvenanceKind::FlatpakRemote {
                        remote: "flathub".into(),
                        remote_url: "https://dl.flathub.org/repo/".into(),
                    },
                    "Flathub",
                    TrustLevel::Official,
                    SignatureStatus::Verified,
                ),
            ),
            app(
                "github:hydralauncher/hydra",
                "Hydra",
                "2.0.0",
                AppSource::Github {
                    repo: GithubRepo {
                        owner: "hydralauncher".into(),
                        repo: "hydra".into(),
                    },
                    asset_pattern: "*.deb".into(),
                },
                prov(
                    ProvenanceKind::Github {
                        owner: "hydralauncher".into(),
                        repo: "hydra".into(),
                        release_url: "https://github.com/hydralauncher/hydra/releases".into(),
                    },
                    "github.com/hydralauncher/hydra",
                    TrustLevel::Verified,
                    SignatureStatus::Verified,
                ),
            ),
            app(
                "manual:mystery-tool",
                "Mystery Tool",
                "0.9.1",
                AppSource::ManualDeb {
                    package_name: "mystery-tool".into(),
                    linked_repo: None,
                },
                prov(
                    ProvenanceKind::ManualDeb {
                        reason: "No APT origin found".into(),
                    },
                    "Unknown",
                    TrustLevel::Unknown,
                    SignatureStatus::NotChecked,
                ),
            ),
        ]
    }

    fn latest(id: &str) -> Option<&'static str> {
        match id {
            "apt:firefox" => Some("132.0"),
            "flatpak:org.gimp.GIMP" => Some("2.10.38"),
            "github:hydralauncher/hydra" => Some("2.1.0"),
            _ => None, // VLC and Mystery Tool are up to date
        }
    }
}

#[async_trait]
impl UpdateSource for FakeSource {
    fn kind(&self) -> &'static str {
        "fake"
    }

    async fn scan(&self) -> Result<Vec<InstalledApp>, SourceError> {
        Ok(Self::apps())
    }

    async fn check_updates(
        &self,
        apps: &[InstalledApp],
    ) -> Result<Vec<UpdateCandidate>, SourceError> {
        Ok(apps
            .iter()
            .filter_map(|a| {
                let latest = Self::latest(&a.id)?;
                crate::version::is_newer(&a.version, latest).then(|| UpdateCandidate {
                    app: a.clone(),
                    latest_version: latest.to_owned(),
                    changelog_url: None,
                    download_url: None,
                    size_bytes: Some(42_000_000),
                    is_security: a.id == "apt:firefox",
                    source_kind: a.source.kind(),
                })
            })
            .collect())
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
            let _ = progress.send(ProgressEvent::Finished { app_id: id.clone() });
            report.succeeded.push(id);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_source_reports_three_updates() {
        let s = FakeSource;
        let apps = s.scan().await.expect("scan");
        assert_eq!(apps.len(), 5);
        let updates = s.check_updates(&apps).await.expect("check");
        assert_eq!(updates.len(), 3);
    }

    #[tokio::test]
    async fn every_fake_app_has_provenance_and_unknown_is_not_disguised() {
        let apps = FakeSource::apps();
        let manual = apps
            .iter()
            .find(|a| a.id == "manual:mystery-tool")
            .expect("manual");
        assert_eq!(manual.provenance.trust, TrustLevel::Unknown);
    }

    #[tokio::test]
    async fn apply_streams_progress() {
        let s = FakeSource;
        let apps = s.scan().await.expect("scan");
        let updates = s.check_updates(&apps).await.expect("check");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let report = s.apply(&updates, tx).await.expect("apply");
        assert_eq!(report.succeeded.len(), 3);
        assert!(matches!(
            rx.recv().await,
            Some(ProgressEvent::Started { .. })
        ));
    }
}
