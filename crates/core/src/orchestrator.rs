//! Scan -> Deduplicate -> Check -> Plan -> Apply across all sources.
//! A failing source never hides the results of the others.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::domain::{AppSource, InstalledApp, UpdateCandidate, UpdateKind};
use crate::source::{ApplyReport, ProgressSender, UpdateSource};

/// Results plus per-source errors (`(source kind, friendly message)`).
#[derive(Debug, Default)]
pub struct Collected<T> {
    pub items: Vec<T>,
    pub errors: Vec<(String, String)>,
}

/// Candidates in the order they must be applied: APT -> Flatpak -> GitHub.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpdatePlan {
    pub steps: Vec<UpdateCandidate>,
}

pub struct Orchestrator {
    sources: Vec<Arc<dyn UpdateSource>>,
}

fn dedup_key(app: &InstalledApp) -> String {
    app.name
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Higher wins: GitHub > ManualDeb > APT > Flatpak.
fn priority(source: &AppSource) -> u8 {
    match source {
        AppSource::Github { .. } => 3,
        AppSource::ManualDeb { .. } => 2,
        AppSource::Apt { .. } => 1,
        AppSource::Flatpak { .. } => 0,
    }
}

/// Merges apps with the same normalized name into one canonical entry; the others'
/// provenance moves to `also_available_from`. Matching by name can over-merge unrelated
/// apps that share a name, so callers should surface `also_available_from` in the UI.
pub fn deduplicate(apps: Vec<InstalledApp>) -> Vec<InstalledApp> {
    let mut groups: HashMap<String, Vec<InstalledApp>> = HashMap::new();
    let mut order = Vec::new();
    for app in apps {
        let key = dedup_key(&app);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(app);
    }
    order
        .into_iter()
        .filter_map(|key| {
            let mut group = groups.remove(&key)?;
            group.sort_by_key(|a| std::cmp::Reverse(priority(&a.source)));
            let mut it = group.into_iter();
            let mut canonical = it.next()?;
            for other in it {
                // The canonical entry may lack data only another source knows (the GitHub
                // entry has no desktop file; the APT one does).
                if canonical.desktop_file.is_none() {
                    canonical.desktop_file = other.desktop_file;
                }
                if canonical.homepage.is_none() {
                    canonical.homepage = other.homepage;
                }
                let mut p = other.provenance;
                p.also_available_from.clear();
                canonical.provenance.also_available_from.push(p);
            }
            Some(canonical)
        })
        .collect()
}

pub fn plan(mut candidates: Vec<UpdateCandidate>) -> UpdatePlan {
    candidates.sort_by_key(|c| c.source_kind); // stable: keeps order within a kind
    UpdatePlan { steps: candidates }
}

impl Orchestrator {
    pub fn new(sources: Vec<Arc<dyn UpdateSource>>) -> Self {
        Self { sources }
    }

    /// Scans every source in parallel, then deduplicates.
    pub async fn scan_all(&self) -> Collected<InstalledApp> {
        let mut set = JoinSet::new();
        for s in &self.sources {
            let s = Arc::clone(s);
            set.spawn(async move { (s.kind(), s.scan().await) });
        }
        let mut out = Collected {
            items: Vec::new(),
            errors: Vec::new(),
        };
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((_, Ok(apps))) => out.items.extend(apps),
                Ok((kind, Err(e))) => out.errors.push((kind.to_owned(), e.to_string())),
                Err(e) => out
                    .errors
                    .push(("unknown".to_owned(), format!("A check crashed: {e}"))),
            }
        }
        out.items = deduplicate(out.items);
        out
    }

    pub async fn check_all(&self, apps: &[InstalledApp]) -> Collected<UpdateCandidate> {
        let mut set = JoinSet::new();
        for s in &self.sources {
            let s = Arc::clone(s);
            let apps = apps.to_vec();
            set.spawn(async move { (s.kind(), s.check_updates(&apps).await) });
        }
        let mut out = Collected {
            items: Vec::new(),
            errors: Vec::new(),
        };
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((_, Ok(c))) => out.items.extend(c),
                Ok((kind, Err(e))) => out.errors.push((kind.to_owned(), e.to_string())),
                Err(e) => out
                    .errors
                    .push(("unknown".to_owned(), format!("A check crashed: {e}"))),
            }
        }
        out
    }

    /// Applies the plan one source at a time, in plan order (avoids dpkg lock conflicts).
    pub async fn apply(
        &self,
        plan: &UpdatePlan,
        progress: ProgressSender,
    ) -> Collected<ApplyReport> {
        let mut out = Collected {
            items: Vec::new(),
            errors: Vec::new(),
        };
        let mut i = 0;
        while i < plan.steps.len() {
            let kind: UpdateKind = plan.steps[i].source_kind;
            let end = i + plan.steps[i..]
                .iter()
                .take_while(|c| c.source_kind == kind)
                .count();
            let batch = &plan.steps[i..end];
            match self.sources.iter().find(|s| s.kind() == kind.source_name()) {
                Some(s) => match s.apply(batch, progress.clone()).await {
                    Ok(r) => out.items.push(r),
                    Err(e) => out.errors.push((s.kind().to_owned(), e.to_string())),
                },
                None => out.errors.push((
                    kind.source_name().to_owned(),
                    format!("No source is available to update {} apps.", kind.badge()),
                )),
            }
            i = end;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::domain::*;
    use crate::source::SourceError;

    fn app(id: &str, name: &str, source: AppSource) -> InstalledApp {
        InstalledApp {
            id: id.into(),
            name: name.into(),
            version: "1.0".into(),
            source,
            provenance: Provenance {
                display_name: id.into(),
                ..Provenance::unknown()
            },
            icon: IconRef::initial_for(name),
            desktop_file: None,
            homepage: None,
            installed_at: None,
            is_system: false,
        }
    }

    fn cand(a: &InstalledApp) -> UpdateCandidate {
        UpdateCandidate {
            app: a.clone(),
            latest_version: "2.0".into(),
            changelog_url: None,
            download_url: None,
            size_bytes: None,
            is_security: false,
            source_kind: a.source.kind(),
        }
    }

    fn apt(name: &str) -> InstalledApp {
        app(
            &format!("apt:{name}"),
            name,
            AppSource::Apt {
                package_name: name.into(),
            },
        )
    }
    fn flat(name: &str) -> InstalledApp {
        app(
            &format!("flatpak:{name}"),
            name,
            AppSource::Flatpak {
                app_id: name.into(),
                remote: "flathub".into(),
            },
        )
    }
    fn gh(name: &str) -> InstalledApp {
        app(
            &format!("github:o/{name}"),
            name,
            AppSource::Github {
                repo: GithubRepo {
                    owner: "o".into(),
                    repo: name.into(),
                },
                asset_pattern: "*.deb".into(),
            },
        )
    }

    struct Fixed {
        kind: &'static str,
        apps: Vec<InstalledApp>,
        fail: bool,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl UpdateSource for Fixed {
        fn kind(&self) -> &'static str {
            self.kind
        }
        async fn scan(&self) -> Result<Vec<InstalledApp>, SourceError> {
            if self.fail {
                Err(SourceError::Offline)
            } else {
                Ok(self.apps.clone())
            }
        }
        async fn check_updates(
            &self,
            apps: &[InstalledApp],
        ) -> Result<Vec<UpdateCandidate>, SourceError> {
            Ok(apps
                .iter()
                .filter(|a| a.source.kind().source_name() == self.kind)
                .map(cand)
                .collect())
        }
        async fn apply(
            &self,
            c: &[UpdateCandidate],
            _p: ProgressSender,
        ) -> Result<ApplyReport, SourceError> {
            self.log.lock().expect("lock").push(self.kind);
            Ok(ApplyReport {
                succeeded: c.iter().map(|c| c.app.id.clone()).collect(),
                failed: vec![],
            })
        }
    }

    #[test]
    fn dedup_prefers_github_and_merges_provenance() {
        let merged = deduplicate(vec![flat("Hydra"), apt("hydra"), gh("Hydra")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "github:o/Hydra");
        assert_eq!(merged[0].provenance.also_available_from.len(), 2);
    }

    #[test]
    fn dedup_keeps_the_desktop_file_of_a_merged_entry() {
        let mut manual = apt("hydra");
        manual.desktop_file = Some("/usr/share/applications/hydra.desktop".into());
        let merged = deduplicate(vec![gh("hydra"), manual]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "github:o/hydra");
        assert!(merged[0].desktop_file.is_some());
    }

    #[test]
    fn dedup_keeps_unrelated_apps() {
        assert_eq!(deduplicate(vec![apt("vlc"), apt("gimp")]).len(), 2);
    }

    #[test]
    fn plan_orders_apt_then_flatpak_then_github() {
        let p = plan(vec![
            cand(&gh("a")),
            cand(&flat("b")),
            cand(&apt("c")),
            cand(&apt("d")),
        ]);
        let kinds: Vec<_> = p.steps.iter().map(|c| c.source_kind).collect();
        assert_eq!(
            kinds,
            [
                UpdateKind::Apt,
                UpdateKind::Apt,
                UpdateKind::Flatpak,
                UpdateKind::Github
            ]
        );
    }

    #[tokio::test]
    async fn one_failing_source_does_not_hide_others() {
        let log = Arc::default();
        let o = Orchestrator::new(vec![
            Arc::new(Fixed {
                kind: "apt",
                apps: vec![apt("vlc")],
                fail: false,
                log: Arc::clone(&log),
            }),
            Arc::new(Fixed {
                kind: "flatpak",
                apps: vec![],
                fail: true,
                log,
            }),
        ]);
        let r = o.scan_all().await;
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0].0, "flatpak");
    }

    #[tokio::test]
    async fn apply_runs_sources_in_plan_order() {
        let log: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::default();
        let mk = |kind, apps| -> Arc<dyn UpdateSource> {
            Arc::new(Fixed {
                kind,
                apps,
                fail: false,
                log: Arc::clone(&log),
            })
        };
        // Registered in the "wrong" order on purpose.
        let o = Orchestrator::new(vec![
            mk("github", vec![]),
            mk("flatpak", vec![]),
            mk("apt", vec![]),
        ]);
        let p = plan(vec![cand(&gh("a")), cand(&flat("b")), cand(&apt("c"))]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = o.apply(&p, tx).await;
        assert!(r.errors.is_empty());
        assert_eq!(*log.lock().expect("lock"), ["apt", "flatpak", "github"]);
    }

    #[tokio::test]
    async fn apply_without_matching_source_reports_error() {
        let o = Orchestrator::new(vec![]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = o.apply(&plan(vec![cand(&apt("x"))]), tx).await;
        assert_eq!(r.errors.len(), 1);
    }
}
