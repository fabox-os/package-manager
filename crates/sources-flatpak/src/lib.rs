//! Flatpak update source.

mod parse;

use async_trait::async_trait;
use package_manager_core::exec::CommandRunner;
use package_manager_core::version::is_newer;
use package_manager_core::{
    AppSource, ApplyReport, IconRef, InstalledApp, ProgressEvent, ProgressSender, SourceError,
    UpdateCandidate, UpdateKind, UpdateSource,
};

pub struct FlatpakSource {
    runner: Box<dyn CommandRunner>,
}

impl FlatpakSource {
    pub fn new(runner: Box<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    async fn stdout(&self, args: &[&str]) -> Result<String, SourceError> {
        self.exec(args, false).await
    }

    /// Like `stdout`, but streams every line to the log (for commands that change the system).
    async fn stdout_logged(&self, args: &[&str]) -> Result<String, SourceError> {
        self.exec(args, true).await
    }

    async fn exec(&self, args: &[&str], logged: bool) -> Result<String, SourceError> {
        let out = if logged {
            self.runner.run_logged("flatpak", args).await?
        } else {
            self.runner.run("flatpak", args).await?
        };
        if out.success {
            Ok(out.stdout)
        } else {
            Err(SourceError::Other(format!(
                "flatpak failed: {}",
                out.stderr.trim()
            )))
        }
    }
}

#[async_trait]
impl UpdateSource for FlatpakSource {
    fn kind(&self) -> &'static str {
        "flatpak"
    }

    async fn scan(&self) -> Result<Vec<InstalledApp>, SourceError> {
        let list = parse::parse_list(
            &self
                .stdout(&[
                    "list",
                    "--app",
                    "--columns=application,name,version,origin,installation",
                ])
                .await?,
        );
        let remotes = match self.stdout(&["remotes", "--columns=name,url"]).await {
            Ok(out) => parse::parse_remotes(&out),
            Err(e) => {
                tracing::warn!("flatpak remotes failed: {e}");
                Default::default()
            }
        };
        Ok(list
            .into_iter()
            .map(|a| InstalledApp {
                id: format!("flatpak:{}", a.app_id),
                icon: IconRef::initial_for(&a.name),
                provenance: parse::provenance_for(
                    &a.remote,
                    remotes.get(&a.remote).map(String::as_str),
                ),
                name: a.name,
                version: a.version,
                source: AppSource::Flatpak {
                    app_id: a.app_id,
                    remote: a.remote,
                },
                desktop_file: None,
                homepage: None,
                installed_at: None,
                is_system: false,
            })
            .collect())
    }

    async fn check_updates(
        &self,
        apps: &[InstalledApp],
    ) -> Result<Vec<UpdateCandidate>, SourceError> {
        let updates = parse::parse_updates(
            &self
                .stdout(&[
                    "remote-ls",
                    "--updates",
                    "--app",
                    "--columns=application,version,origin",
                ])
                .await?,
        );
        Ok(apps
            .iter()
            .filter_map(|app| {
                let AppSource::Flatpak { app_id, remote } = &app.source else {
                    return None;
                };
                let u = updates
                    .iter()
                    .find(|u| &u.app_id == app_id && &u.remote == remote)?;
                // Flatpak often omits versions; a listed update is then taken at face value.
                let newer = app.version.is_empty()
                    || u.version.is_empty()
                    || is_newer(&app.version, &u.version);
                newer.then(|| UpdateCandidate {
                    app: app.clone(),
                    latest_version: if u.version.is_empty() {
                        "latest".to_owned()
                    } else {
                        u.version.clone()
                    },
                    changelog_url: None,
                    download_url: None,
                    size_bytes: None,
                    is_security: false,
                    source_kind: UpdateKind::Flatpak,
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
            let AppSource::Flatpak { app_id, .. } = &c.app.source else {
                continue;
            };
            let id = c.app.id.clone();
            // A closed receiver only means nobody is listening anymore.
            let _ = progress.send(ProgressEvent::Started { app_id: id.clone() });
            // Flatpak elevates through polkit itself for system installs; no root helper needed.
            match self
                .stdout_logged(&["update", "-y", "--noninteractive", app_id])
                .await
            {
                Ok(_) => {
                    let _ = progress.send(ProgressEvent::Finished { app_id: id.clone() });
                    report.succeeded.push(id);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use package_manager_core::exec::scripted::ScriptedRunner;
    use package_manager_core::TrustLevel;

    fn runner() -> ScriptedRunner {
        ScriptedRunner::new()
            .with(
                "flatpak list --app --columns=application,name,version,origin,installation",
                include_str!("../tests/fixtures/flatpak-list.txt"),
            )
            .with(
                "flatpak remotes --columns=name,url",
                include_str!("../tests/fixtures/flatpak-remotes.txt"),
            )
            .with(
                "flatpak remote-ls --updates --app --columns=application,version,origin",
                include_str!("../tests/fixtures/flatpak-updates.txt"),
            )
            .with("flatpak update -y --noninteractive org.gimp.GIMP", "")
    }

    #[tokio::test]
    async fn scan_sets_provenance() {
        let apps = FlatpakSource::new(Box::new(runner()))
            .scan()
            .await
            .expect("scan");
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].provenance.trust, TrustLevel::Official);
        assert_eq!(apps[1].provenance.trust, TrustLevel::Community);
    }

    #[tokio::test]
    async fn check_returns_listed_updates() {
        let src = FlatpakSource::new(Box::new(runner()));
        let apps = src.scan().await.expect("scan");
        let updates = src.check_updates(&apps).await.expect("check");
        assert_eq!(updates.len(), 2);
        // Tool has no installed version: still reported, as listed by the remote.
        assert_eq!(updates[1].latest_version, "1.1");
    }

    #[tokio::test]
    async fn apply_reports_success_and_failure_per_app() {
        let src = FlatpakSource::new(Box::new(runner()));
        let apps = src.scan().await.expect("scan");
        let updates = src.check_updates(&apps).await.expect("check");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        // Tool's update command is not scripted -> fails, GIMP succeeds.
        let report = src.apply(&updates, tx).await.expect("apply");
        assert_eq!(report.succeeded, vec!["flatpak:org.gimp.GIMP"]);
        assert_eq!(report.failed.len(), 1);
    }

    #[tokio::test]
    async fn missing_flatpak_is_reported_as_unavailable_error() {
        let r = ScriptedRunner::new();
        let err = FlatpakSource::new(Box::new(r))
            .scan()
            .await
            .expect_err("no flatpak");
        assert!(matches!(
            err,
            SourceError::Other(_) | SourceError::Unavailable(_)
        ));
    }
}
