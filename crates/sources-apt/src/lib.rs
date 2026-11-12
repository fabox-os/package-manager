//! APT / dpkg update source.

mod parse;

use std::path::PathBuf;

use async_trait::async_trait;
use package_manager_core::exec::{CommandRunner, PKEXEC};
use package_manager_core::version::is_newer;
use package_manager_core::{
    AppSource, ApplyReport, IconRef, InstalledApp, ProgressEvent, ProgressSender, SourceError,
    UpdateCandidate, UpdateKind, UpdateSource,
};

pub struct AptSource {
    runner: Box<dyn CommandRunner>,
    /// Absolute path of `package-manager-helper`, run through `pkexec`.
    helper: PathBuf,
}

/// pkexec: 126 = dialog dismissed or not authorized, 127 = authentication failed.
const PKEXEC_AUTH_CODES: [i32; 2] = [126, 127];

impl AptSource {
    pub fn new(runner: Box<dyn CommandRunner>, helper: PathBuf) -> Self {
        Self { runner, helper }
    }

    /// Runs the helper with a validated command through `pkexec`.
    async fn run_helper(
        &self,
        args: &[&str],
    ) -> Result<package_manager_core::exec::CommandOutput, SourceError> {
        let helper = self
            .helper
            .to_str()
            .filter(|_| self.helper.is_absolute())
            .ok_or_else(|| SourceError::Other("The update helper path is invalid.".to_owned()))?;
        let mut full = vec![helper];
        full.extend_from_slice(args);
        // Changes the system: stream every line to the log.
        let out = self.runner.run_logged(PKEXEC, &full).await?;
        match out.code {
            Some(c) if PKEXEC_AUTH_CODES.contains(&c) => Err(SourceError::Cancelled),
            _ if out.stderr.contains("Could not get lock") || out.stderr.contains("is locked") => {
                Err(SourceError::Locked)
            }
            _ => Ok(out),
        }
    }

    /// Refreshes the package index (`apt-get update`) through the helper.
    /// Asks for administrator authentication, so callers should do it on explicit request.
    pub async fn refresh_index(&self) -> Result<(), SourceError> {
        let out = self.run_helper(&["apt-update"]).await?;
        if out.success {
            Ok(())
        } else {
            Err(SourceError::Other(format!(
                "Could not refresh the package list: {}",
                out.stderr.trim()
            )))
        }
    }

    async fn stdout(&self, program: &str, args: &[&str]) -> Result<String, SourceError> {
        let out = self.runner.run(program, args).await?;
        if out.success {
            Ok(out.stdout)
        } else if out.stderr.contains("Could not get lock") || out.stderr.contains("is locked") {
            Err(SourceError::Locked)
        } else {
            Err(SourceError::Other(format!(
                "{program} failed: {}",
                out.stderr.trim()
            )))
        }
    }
}

#[async_trait]
impl UpdateSource for AptSource {
    fn kind(&self) -> &'static str {
        "apt"
    }

    /// Lists every installed package. Auto-installed dependencies are flagged `is_system`.
    async fn scan(&self) -> Result<Vec<InstalledApp>, SourceError> {
        let installed = parse::parse_dpkg_query(
            &self
                .stdout(
                    "dpkg-query",
                    &["-W", "-f=${Package}\t${Version}\t${db:Status-Abbrev}\n"],
                )
                .await?,
        );
        let manual = parse::parse_manual_list(&self.stdout("apt-mark", &["showmanual"]).await?);
        let manual: std::collections::HashSet<&str> = manual.iter().map(String::as_str).collect();
        // Multi-arch installs list the same package once per architecture: keep one.
        let mut seen = std::collections::HashSet::new();
        let packages: Vec<_> = installed
            .into_iter()
            .filter(|p| seen.insert(p.name.clone()))
            .collect();
        if packages.is_empty() {
            return Ok(Vec::new());
        }

        let mut args = vec!["policy"];
        args.extend(packages.iter().map(|p| p.name.as_str()));
        // If policy fails, every app is honestly shown as Unknown rather than guessed.
        let policy = match self.stdout("apt-cache", &args).await {
            Ok(out) => parse::parse_policy(&out),
            Err(e) => {
                tracing::warn!("apt-cache policy failed: {e}");
                Default::default()
            }
        };

        // Desktop files give display names and icons. Optional: any failure just means
        // packages keep their package names.
        let desktop_files = match self
            .stdout("dpkg-query", &["-S", "/usr/share/applications/*.desktop"])
            .await
        {
            Ok(out) => parse::parse_desktop_files(&out),
            Err(e) => {
                tracing::debug!("no desktop files for APT packages: {e}");
                Default::default()
            }
        };

        Ok(packages
            .into_iter()
            .map(|p| {
                let provenance = policy
                    .get(&p.name)
                    .cloned()
                    .unwrap_or_else(package_manager_core::Provenance::unknown);
                let is_manual_deb = matches!(
                    provenance.kind,
                    package_manager_core::ProvenanceKind::ManualDeb { .. }
                );
                let (id, source) = if is_manual_deb {
                    (
                        format!("manual:{}", p.name),
                        AppSource::ManualDeb {
                            package_name: p.name.clone(),
                            linked_repo: None,
                        },
                    )
                } else {
                    (
                        format!("apt:{}", p.name),
                        AppSource::Apt {
                            package_name: p.name.clone(),
                        },
                    )
                };
                let desktop_file = desktop_files.get(&p.name).cloned();
                InstalledApp {
                    is_system: !manual.contains(p.name.as_str()),
                    id,
                    icon: IconRef::initial_for(&p.name),
                    name: p.name,
                    version: p.version,
                    source,
                    provenance,
                    desktop_file,
                    homepage: None,
                    installed_at: None,
                }
            })
            .collect())
    }

    async fn check_updates(
        &self,
        apps: &[InstalledApp],
    ) -> Result<Vec<UpdateCandidate>, SourceError> {
        let upgradable =
            parse::parse_upgradable(&self.stdout("apt", &["list", "--upgradable"]).await?);
        Ok(apps
            .iter()
            .filter_map(|app| {
                let AppSource::Apt { package_name } = &app.source else {
                    return None;
                };
                let u = upgradable.iter().find(|u| &u.name == package_name)?;
                is_newer(&app.version, &u.new_version).then(|| UpdateCandidate {
                    app: app.clone(),
                    latest_version: u.new_version.clone(),
                    changelog_url: None,
                    download_url: None,
                    size_bytes: None,
                    is_security: u.is_security,
                    source_kind: UpdateKind::Apt,
                })
            })
            .collect())
    }

    async fn apply(
        &self,
        candidates: &[UpdateCandidate],
        progress: ProgressSender,
    ) -> Result<ApplyReport, SourceError> {
        let targets: Vec<(&str, &str)> = candidates
            .iter()
            .filter_map(|c| match &c.app.source {
                AppSource::Apt { package_name } => Some((c.app.id.as_str(), package_name.as_str())),
                _ => None,
            })
            .collect();
        if targets.is_empty() {
            return Ok(ApplyReport::default());
        }
        // A closed receiver only means nobody is listening anymore.
        for (id, _) in &targets {
            let _ = progress.send(ProgressEvent::Started {
                app_id: (*id).to_owned(),
            });
        }
        let mut args = vec!["apt-upgrade"];
        args.extend(targets.iter().map(|(_, pkg)| *pkg));
        let out = self.run_helper(&args).await?;

        // APT upgrades the batch in one transaction: all succeed or none are reported done.
        let mut report = ApplyReport::default();
        for (id, _) in targets {
            if out.success {
                let _ = progress.send(ProgressEvent::Finished {
                    app_id: id.to_owned(),
                });
                report.succeeded.push(id.to_owned());
            } else {
                let reason = format!("APT could not finish the upgrade: {}", out.stderr.trim());
                let _ = progress.send(ProgressEvent::Failed {
                    app_id: id.to_owned(),
                    reason: reason.clone(),
                });
                report.failed.push((id.to_owned(), reason));
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use package_manager_core::exec::scripted::ScriptedRunner;

    const HELPER: &str = "/usr/libexec/package-manager-helper";

    fn runner() -> ScriptedRunner {
        let pkgs = "firefox vlc hydra mypkg libfoo";
        ScriptedRunner::new()
            .with(
                "dpkg-query -W -f=${Package}\t${Version}\t${db:Status-Abbrev}\n",
                include_str!("../tests/fixtures/dpkg-query.txt"),
            )
            .with(
                "apt-mark showmanual",
                include_str!("../tests/fixtures/apt-mark-showmanual.txt"),
            )
            .with(
                &format!("apt-cache policy {pkgs}"),
                include_str!("../tests/fixtures/apt-cache-policy.txt"),
            )
            .with(
                "apt list --upgradable",
                include_str!("../tests/fixtures/apt-list-upgradable.txt"),
            )
    }

    #[tokio::test]
    async fn scan_maps_sources_and_provenance() {
        let src = AptSource::new(Box::new(runner()), HELPER.into());
        let apps = src.scan().await.expect("scan");
        assert_eq!(apps.len(), 5);
        let ids: Vec<_> = apps.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"apt:firefox"));
        assert!(ids.contains(&"manual:mypkg"));
    }

    #[tokio::test]
    async fn auto_installed_packages_are_flagged_system() {
        let src = AptSource::new(Box::new(runner()), HELPER.into());
        let apps = src.scan().await.expect("scan");
        let flag = |id: &str| apps.iter().find(|a| a.id == id).map(|a| a.is_system);
        assert_eq!(flag("apt:libfoo"), Some(true));
        assert_eq!(flag("apt:firefox"), Some(false));
    }

    #[tokio::test]
    async fn scan_links_packages_to_their_desktop_files() {
        let r = runner().with(
            "dpkg-query -S /usr/share/applications/*.desktop",
            include_str!("../tests/fixtures/dpkg-search-desktop.txt"),
        );
        let apps = AptSource::new(Box::new(r), HELPER.into())
            .scan()
            .await
            .expect("scan");
        let ff = apps
            .iter()
            .find(|a| a.id == "apt:firefox")
            .expect("firefox");
        assert_eq!(
            ff.desktop_file.as_deref(),
            Some(std::path::Path::new(
                "/usr/share/applications/firefox.desktop"
            ))
        );
        let vlc = apps.iter().find(|a| a.id == "apt:vlc").expect("vlc");
        assert!(vlc.desktop_file.is_none());
    }

    #[tokio::test]
    async fn multiarch_duplicates_are_listed_once() {
        let r = ScriptedRunner::new()
            .with(
                "dpkg-query -W -f=${Package}\t${Version}\t${db:Status-Abbrev}\n",
                "libc6\t2.39\tii \nlibc6\t2.39\tii \n",
            )
            .with("apt-mark showmanual", "")
            .with("apt-cache policy libc6", "");
        let apps = AptSource::new(Box::new(r), HELPER.into())
            .scan()
            .await
            .expect("scan");
        assert_eq!(apps.len(), 1);
    }

    #[tokio::test]
    async fn check_finds_user_and_system_updates_with_security_flag() {
        let src = AptSource::new(Box::new(runner()), HELPER.into());
        let apps = src.scan().await.expect("scan");
        let updates = src.check_updates(&apps).await.expect("check");
        let ids: Vec<_> = updates.iter().map(|u| u.app.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"apt:firefox") && ids.contains(&"apt:libfoo"));
        let ff = updates
            .iter()
            .find(|u| u.app.id == "apt:firefox")
            .expect("firefox");
        assert!(ff.is_security);
    }

    #[tokio::test]
    async fn lock_error_is_friendly() {
        let r = ScriptedRunner::new().with_failure(
            "apt list --upgradable",
            "E: Could not get lock /var/lib/dpkg/lock",
        );
        let src = AptSource::new(Box::new(r), HELPER.into());
        let err = src.check_updates(&[]).await.expect_err("locked");
        assert!(matches!(err, SourceError::Locked));
    }

    #[tokio::test]
    async fn missing_apt_cache_degrades_to_unknown() {
        let r = ScriptedRunner::new()
            .with(
                "dpkg-query -W -f=${Package}\t${Version}\t${db:Status-Abbrev}\n",
                "tool\t1.0\tii \n",
            )
            .with("apt-mark showmanual", "tool\n");
        let apps = AptSource::new(Box::new(r), HELPER.into())
            .scan()
            .await
            .expect("scan");
        assert_eq!(
            apps[0].provenance.trust,
            package_manager_core::TrustLevel::Unknown
        );
    }

    async fn firefox_candidate() -> (AptSource, Vec<UpdateCandidate>) {
        let src = AptSource::new(Box::new(runner()), HELPER.into());
        let apps = src.scan().await.expect("scan");
        let updates: Vec<_> = src
            .check_updates(&apps)
            .await
            .expect("check")
            .into_iter()
            .filter(|u| u.app.id == "apt:firefox")
            .collect();
        (src, updates)
    }

    fn apply_source(r: ScriptedRunner) -> AptSource {
        AptSource::new(Box::new(r), HELPER.into())
    }

    #[tokio::test]
    async fn apply_calls_helper_through_pkexec_with_only_plan_packages() {
        let (_, updates) = firefox_candidate().await;
        let r = ScriptedRunner::new()
            .with(&format!("/usr/bin/pkexec {HELPER} apt-upgrade firefox"), "");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let report = apply_source(r).apply(&updates, tx).await.expect("apply");
        assert_eq!(report.succeeded, vec!["apt:firefox"]);
        assert!(matches!(
            rx.recv().await,
            Some(ProgressEvent::Started { .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ProgressEvent::Finished { .. })
        ));
    }

    #[tokio::test]
    async fn dismissed_auth_dialog_is_cancelled_not_failed() {
        let (_, updates) = firefox_candidate().await;
        let r = ScriptedRunner::new().with_exit(
            &format!("/usr/bin/pkexec {HELPER} apt-upgrade firefox"),
            126,
            "",
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = apply_source(r)
            .apply(&updates, tx)
            .await
            .expect_err("cancelled");
        assert!(matches!(err, SourceError::Cancelled));
    }

    #[tokio::test]
    async fn apt_lock_during_apply_is_friendly() {
        let (_, updates) = firefox_candidate().await;
        let r = ScriptedRunner::new().with_exit(
            &format!("/usr/bin/pkexec {HELPER} apt-upgrade firefox"),
            100,
            "E: Could not get lock /var/lib/dpkg/lock-frontend",
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = apply_source(r)
            .apply(&updates, tx)
            .await
            .expect_err("locked");
        assert!(matches!(err, SourceError::Locked));
    }

    #[tokio::test]
    async fn failed_upgrade_marks_packages_failed() {
        let (_, updates) = firefox_candidate().await;
        let r = ScriptedRunner::new().with_exit(
            &format!("/usr/bin/pkexec {HELPER} apt-upgrade firefox"),
            100,
            "E: broken",
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let report = apply_source(r).apply(&updates, tx).await.expect("report");
        assert!(report.succeeded.is_empty());
        assert_eq!(report.failed.len(), 1);
    }

    #[tokio::test]
    async fn relative_helper_path_is_refused() {
        let src = AptSource::new(Box::new(ScriptedRunner::new()), "helper".into());
        assert!(src.refresh_index().await.is_err());
    }

    #[tokio::test]
    async fn refresh_index_uses_helper() {
        let r = ScriptedRunner::new().with(&format!("/usr/bin/pkexec {HELPER} apt-update"), "");
        apply_source(r).refresh_index().await.expect("refresh");
    }
}
