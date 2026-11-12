//! iced GUI shell. All data comes through `package-manager-core` traits; the app never
//! talks to apt, flatpak or GitHub directly.

mod link_panel;
#[cfg(feature = "screenshots")]
mod shots;
mod strings;
mod style;
mod terminal;

use iced::widget::{
    button, checkbox, column, container, horizontal_space, pick_list, row, scrollable, text,
    text_input,
};
use iced::{Element, Length, Task, Theme};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use iced::futures::SinkExt;
use package_manager_config::{Config, Database, NewHistory, Paths};
use package_manager_core::log::{spawn_file_logger, LogBroadcaster, LogEvent, LogFile};
use package_manager_core::orchestrator::{plan, Orchestrator};
use package_manager_core::ProgressEvent;
use package_manager_runtime::Backend;
use tokio::sync::broadcast;

use link_panel::{LinkMsg, LinkPanel};
use package_manager_core::{InstalledApp, TrustLevel, UpdateCandidate, UpdateKind};
use terminal::Terminal;

fn backend(config: &Config, db: Option<&Database>, paths: Option<&Paths>) -> Backend {
    #[cfg(feature = "fake")]
    if std::env::args().any(|a| a == "--fake") {
        return package_manager_runtime::build_fake();
    }
    package_manager_runtime::build(
        package_manager_runtime::Enabled {
            apt: config.sources.apt,
            flatpak: config.sources.flatpak,
            github: config.sources.github,
        },
        package_manager_runtime::Services {
            db: db.cloned(),
            github_cache_dir: paths.map(|p| p.github_cache_dir.clone()),
            downloads_dir: paths.map(|p| p.downloads_dir.clone()),
        },
    )
}

type SharedReceiver = Arc<Mutex<Option<broadcast::Receiver<LogEvent>>>>;

/// Everything loaded from disk before the window opens.
struct Startup {
    config: Config,
    paths: Option<Paths>,
    db: Option<Database>,
    /// Problems to show the user (bad settings file, unusable database, ...).
    notices: Vec<String>,
}

fn load_startup() -> Startup {
    let paths = Paths::discover();
    let mut notices = Vec::new();
    let config = match &paths {
        Some(p) => {
            let loaded = Config::load(&p.config_file);
            notices.extend(loaded.warning);
            loaded.config
        }
        None => {
            notices.push(
                "Could not find your home folder: settings and history will not be saved."
                    .to_owned(),
            );
            Config::default()
        }
    };
    let db = paths
        .as_ref()
        .and_then(|p| match Database::open(&p.database_file) {
            Ok(db) => Some(db),
            Err(e) => {
                tracing::warn!("{e}");
                notices.push(format!("{e} Update history will not be recorded."));
                None
            }
        });
    Startup {
        config,
        paths,
        db,
        notices,
    }
}

/// The window icon (the same artwork the installer puts in the system icon theme).
const APP_ICON: &[u8] = include_bytes!("../../../data/icons/package-manager-256.png");

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let startup = load_startup();
    let backend = backend(&startup.config, startup.db.as_ref(), startup.paths.as_ref());
    // Subscribe before the first scan so the panel does not miss the opening lines.
    let log_rx: SharedReceiver = Arc::new(Mutex::new(Some(backend.logs.subscribe())));
    if let Some(p) = &startup.paths {
        spawn_file_logger(
            backend.logs.subscribe(),
            LogFile::new(p.last_run_log.clone()),
        );
    }
    let mut window = iced::window::Settings::default();
    // A broken icon must never stop the app from starting.
    window.icon = iced::window::icon::from_file_data(APP_ICON, None).ok();
    // Wayland and X11 docks find the icon through this id: it must equal the name of the
    // installed `package-manager.desktop`.
    window.platform_specific.application_id = "org.packagemanager.PackageManager".to_owned();
    iced::application(strings::APP_TITLE, App::update, App::view)
        .window(window)
        .subscription(App::subscription)
        .theme(|_| Theme::Dark)
        .run_with(move || App::new(backend, log_rx, startup))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Kind(UpdateKind),
}

impl Filter {
    const ALL: [Filter; 5] = [
        Filter::All,
        Filter::Kind(UpdateKind::Apt),
        Filter::Kind(UpdateKind::Flatpak),
        Filter::Kind(UpdateKind::Github),
        Filter::Kind(UpdateKind::ManualDeb),
    ];
}

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Filter::All => f.write_str("All"),
            Filter::Kind(k) => f.write_str(k.badge()),
        }
    }
}

enum Status {
    Loading,
    Ready,
    Updating,
    Error(String),
}

struct App {
    backend: Arc<Orchestrator>,
    logs: LogBroadcaster,
    log_rx: SharedReceiver,
    terminal: Terminal,
    link: LinkPanel,
    #[cfg(feature = "screenshots")]
    shots: Option<shots::Shots>,
    /// `None` when the GitHub source is off, so "Add from GitHub" is hidden.
    linker: Option<Arc<package_manager_runtime::GithubLinker>>,
    github_consent: package_manager_runtime::GithubConsent,
    /// Plain (non-error) messages, e.g. "linked X".
    info_notices: Vec<String>,
    config: Config,
    paths: Option<Paths>,
    db: Option<Database>,
    /// Settings/database problems; stay visible until the app restarts.
    startup_notices: Vec<String>,
    status: Status,
    apps: Vec<InstalledApp>,
    updates: Vec<UpdateCandidate>,
    /// Problems from the last check (e.g. one source unavailable).
    load_notices: Vec<String>,
    /// Problems from the last update run.
    apply_notices: Vec<String>,
    search: String,
    filter: Filter,
    show_system: bool,
}

/// Rendering thousands of rows is slow; past this the user is asked to search.
const MAX_ROWS: usize = 200;

#[derive(Debug, Clone)]
struct Loaded {
    apps: Vec<InstalledApp>,
    updates: Vec<UpdateCandidate>,
    errors: Vec<String>,
}

#[derive(Debug, Clone)]
struct Applied {
    succeeded: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone)]
enum Message {
    Loaded(Loaded),
    SearchChanged(String),
    FilterChanged(Filter),
    ShowSystemToggled(bool),
    CheckRequested,
    UpdateAllRequested,
    UpdateOneRequested(String),
    Applied(Applied),
    LogBatch(Vec<LogEvent>),
    TerminalToggled(bool),
    TerminalCopy,
    TerminalClear,
    TerminalExpand,
    TerminalJump,
    TerminalScrolled(f32),
    ConfigSaved(Result<(), String>),
    Link(LinkMsg),
    #[cfg(feature = "screenshots")]
    Shot(shots::ShotMsg),
}

fn source_label(kind: &str) -> &str {
    match kind {
        "apt" => "APT",
        "flatpak" => "Flatpak",
        "github" => "GitHub",
        other => other,
    }
}

fn label_errors(errors: Vec<(String, String)>) -> Vec<String> {
    errors
        .into_iter()
        .map(|(k, m)| format!("{}: {m}", source_label(&k)))
        .collect()
}

async fn load(backend: Arc<Orchestrator>) -> Loaded {
    let mut scanned = backend.scan_all().await;
    // Real names and icons, before the check so update candidates carry them too.
    // Reads thousands of small files, so it runs off the async threads.
    let apps = std::mem::take(&mut scanned.items);
    scanned.items = match tokio::task::spawn_blocking({
        let apps = apps.clone();
        move || package_manager_runtime::Enricher::system_default().enrich(apps)
    })
    .await
    {
        Ok(enriched) => enriched,
        Err(e) => {
            tracing::warn!("could not load app names and icons: {e}");
            apps
        }
    };
    let checked = backend.check_all(&scanned.items).await;
    let mut errors = label_errors(scanned.errors);
    errors.extend(label_errors(checked.errors));
    Loaded {
        apps: scanned.items,
        updates: checked.items,
        errors,
    }
}

async fn apply(
    backend: Arc<Orchestrator>,
    logs: LogBroadcaster,
    db: Option<Database>,
    candidates: Vec<UpdateCandidate>,
) -> Applied {
    // id -> (display name, "APT • Ubuntu noble/main"): every update line names its origin.
    let info: HashMap<String, (String, String)> = candidates
        .iter()
        .map(|c| {
            (
                c.app.id.clone(),
                (
                    c.app.name.clone(),
                    format!(
                        "{} • {}",
                        c.app.source.kind().badge(),
                        c.app.provenance.display_name
                    ),
                ),
            )
        })
        .collect();
    let history_src = candidates.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let progress_logs = logs.clone();
    let drain = async move {
        while let Some(event) = rx.recv().await {
            let (id, status, msg) = match event {
                ProgressEvent::Started { app_id } => (app_id, false, "Updating…".to_owned()),
                ProgressEvent::Finished { app_id } => (app_id, true, "Updated".to_owned()),
                ProgressEvent::Failed { app_id, reason } => {
                    (app_id, true, format!("Failed: {reason}"))
                }
                ProgressEvent::Percent { .. } => continue,
            };
            let (name, origin) = info
                .get(&id)
                .cloned()
                .unwrap_or((id.clone(), String::new()));
            let prefix = (!origin.is_empty()).then_some(origin);
            let line = format!("{name}: {msg}");
            progress_logs.emit(if status {
                LogEvent::status(prefix, line)
            } else {
                LogEvent::output(prefix, line)
            });
        }
    };
    let update_plan = plan(candidates);
    let (result, ()) = tokio::join!(backend.apply(&update_plan, tx), drain);

    let mut out = Applied {
        succeeded: Vec::new(),
        errors: label_errors(result.errors),
    };
    for report in result.items {
        out.succeeded.extend(report.succeeded);
        out.errors.extend(
            report
                .failed
                .into_iter()
                .map(|(id, why)| format!("{id}: {why}")),
        );
    }
    record_history(db, &history_src, &out).await;
    if out.errors.is_empty() {
        logs.emit(LogEvent::status(None, "Done"));
    } else {
        for e in &out.errors {
            logs.emit(LogEvent::status(None, format!("Failed: {e}")));
        }
    }
    out
}

/// Writes one history row per attempted update, including where it came from.
async fn record_history(db: Option<Database>, attempted: &[UpdateCandidate], outcome: &Applied) {
    let Some(db) = db else { return };
    let rows: Vec<NewHistory> = attempted
        .iter()
        .map(|c| {
            let success = outcome.succeeded.contains(&c.app.id);
            NewHistory {
                app_id: c.app.id.clone(),
                app_name: c.app.name.clone(),
                from_version: c.app.version.clone(),
                to_version: c.latest_version.clone(),
                source: c.app.source.kind().badge().to_owned(),
                provenance: c.app.provenance.display_name.clone(),
                success,
                message: (!success).then(|| {
                    outcome
                        .errors
                        .iter()
                        .find(|e| e.contains(&c.app.id))
                        .cloned()
                        .unwrap_or_else(|| "Not completed".to_owned())
                }),
            }
        })
        .collect();
    let result =
        tokio::task::spawn_blocking(move || rows.iter().try_for_each(|r| db.add_history(r))).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("could not record history: {e}"),
        Err(e) => tracing::warn!("history task failed: {e}"),
    }
}

impl App {
    fn new(backend: Backend, log_rx: SharedReceiver, startup: Startup) -> (Self, Task<Message>) {
        let Startup {
            config,
            paths,
            db,
            notices,
        } = startup;
        let Backend {
            orchestrator,
            logs,
            github_consent,
            linker,
        } = backend;
        logs.emit(LogEvent::run_start());
        let task = Task::perform(load(Arc::clone(&orchestrator)), Message::Loaded);
        (
            Self {
                backend: orchestrator,
                logs,
                log_rx,
                terminal: Terminal::new(config.ui.show_terminal),
                link: LinkPanel::new(),
                #[cfg(feature = "screenshots")]
                shots: shots::from_args(),
                linker,
                github_consent,
                info_notices: Vec::new(),
                show_system: config.general.show_system_packages,
                config,
                paths,
                db,
                startup_notices: notices,
                status: Status::Loading,
                apps: Vec::new(),
                updates: Vec::new(),
                load_notices: Vec::new(),
                apply_notices: Vec::new(),
                search: String::new(),
                filter: Filter::All,
            },
            task,
        )
    }

    fn reload(&mut self) -> Task<Message> {
        self.status = Status::Loading;
        Task::perform(load(Arc::clone(&self.backend)), Message::Loaded)
    }

    #[cfg(not(feature = "screenshots"))]
    fn update(&mut self, message: Message) -> Task<Message> {
        self.update_core(message)
    }

    /// Development builds with `--features screenshots` also drive the screenshot sequence.
    #[cfg(feature = "screenshots")]
    fn update(&mut self, message: Message) -> Task<Message> {
        if let Message::Shot(m) = message {
            return self.shots_update(m);
        }
        let event = shots::observe(&message);
        let task = self.update_core(message);
        match event {
            Some(e) => Task::batch([task, self.shots_event(e)]),
            None => task,
        }
    }

    fn update_core(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(l) => {
                for e in &l.errors {
                    tracing::warn!("{e}");
                }
                self.status = if l.apps.is_empty() && !l.errors.is_empty() {
                    Status::Error(l.errors.join("\n"))
                } else {
                    Status::Ready
                };
                self.apps = l.apps;
                self.updates = l.updates;
                self.load_notices = l.errors;
                Task::none()
            }
            Message::SearchChanged(s) => {
                self.search = s;
                Task::none()
            }
            Message::FilterChanged(f) => {
                self.filter = f;
                Task::none()
            }
            Message::ShowSystemToggled(on) => {
                self.show_system = on;
                self.config.general.show_system_packages = on;
                self.save_config()
            }
            Message::CheckRequested => {
                self.info_notices.clear();
                self.logs.emit(LogEvent::run_start());
                self.apply_notices.clear();
                self.reload()
            }
            Message::UpdateAllRequested => self.start_apply(self.updates.clone()),
            Message::UpdateOneRequested(id) => {
                let one = self
                    .updates
                    .iter()
                    .filter(|u| u.app.id == id)
                    .cloned()
                    .collect();
                self.start_apply(one)
            }
            Message::Applied(done) => {
                self.apply_notices = done.errors;
                // Verify: re-scan so the list shows what is really installed now.
                self.reload()
            }
            Message::LogBatch(events) => self.terminal.push(events),
            Message::TerminalToggled(on) => {
                self.terminal.visible = on;
                self.config.ui.show_terminal = on;
                let save = self.save_config();
                if on {
                    // A re-shown panel starts at the newest line.
                    Task::batch([save, self.terminal.jump_to_bottom()])
                } else {
                    save
                }
            }
            Message::Link(m) => self.link_update(m),
            #[cfg(feature = "screenshots")]
            Message::Shot(_) => Task::none(), // handled in `update`
            Message::ConfigSaved(Ok(())) => Task::none(),
            Message::ConfigSaved(Err(e)) => {
                tracing::warn!("{e}");
                if !self.startup_notices.contains(&e) {
                    self.startup_notices.push(e);
                }
                Task::none()
            }
            Message::TerminalCopy => iced::clipboard::write(self.terminal.text().to_owned()),
            Message::TerminalClear => {
                self.terminal.clear();
                Task::none()
            }
            Message::TerminalExpand => {
                self.terminal.toggle_expanded();
                Task::none()
            }
            Message::TerminalJump => self.terminal.jump_to_bottom(),
            Message::TerminalScrolled(y) => {
                self.terminal.scrolled(y);
                Task::none()
            }
        }
    }

    /// Streams log events from the broadcaster into the UI, batched so a burst of output
    /// (an `apt` run) produces a few updates, not thousands.
    fn subscription(&self) -> iced::Subscription<Message> {
        let logs = self.logs.clone();
        let shared = Arc::clone(&self.log_rx);
        iced::Subscription::run_with_id(
            "log-stream",
            iced::stream::channel(100, move |mut output| async move {
                let taken = shared.lock().ok().and_then(|mut g| g.take());
                let mut rx = taken.unwrap_or_else(|| logs.subscribe());
                loop {
                    match rx.recv().await {
                        Ok(first) => {
                            let mut batch = vec![first];
                            while batch.len() < 500 {
                                match rx.try_recv() {
                                    Ok(e) => batch.push(e),
                                    Err(_) => break,
                                }
                            }
                            if output.send(Message::LogBatch(batch)).await.is_err() {
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }),
        )
    }

    /// Saves settings off the UI thread. A failure is shown, never fatal.
    fn save_config(&self) -> Task<Message> {
        let Some(paths) = &self.paths else {
            return Task::none();
        };
        let (path, config) = (paths.config_file.clone(), self.config.clone());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || config.save(&path))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()))
            },
            Message::ConfigSaved,
        )
    }

    fn start_apply(&mut self, candidates: Vec<UpdateCandidate>) -> Task<Message> {
        if candidates.is_empty() {
            return Task::none();
        }
        self.status = Status::Updating;
        self.logs.emit(LogEvent::run_start());
        if self.config.ui.terminal_auto_show {
            self.terminal.visible = true;
        }
        Task::perform(
            apply(
                Arc::clone(&self.backend),
                self.logs.clone(),
                self.db.clone(),
                candidates,
            ),
            Message::Applied,
        )
    }

    /// Rows to show (capped) and how many matched before the cap.
    /// Apps with a pending update come first, then alphabetical.
    fn visible(&self) -> (Vec<&InstalledApp>, usize) {
        let q = self.search.to_lowercase();
        let has_update = |a: &InstalledApp| self.updates.iter().any(|u| u.app.id == a.id);
        let mut rows: Vec<&InstalledApp> = self
            .apps
            .iter()
            .filter(|a| self.show_system || !a.is_system)
            .filter(|a| match self.filter {
                Filter::All => true,
                Filter::Kind(k) => a.source.kind() == k,
            })
            .filter(|a| q.is_empty() || a.name.to_lowercase().contains(&q))
            .collect();
        rows.sort_by_cached_key(|a| (!has_update(a), a.name.to_lowercase()));
        let total = rows.len();
        rows.truncate(MAX_ROWS);
        (rows, total)
    }

    /// Pending updates for system packages that the list hides.
    fn hidden_system_updates(&self) -> usize {
        if self.show_system {
            return 0;
        }
        self.updates.iter().filter(|u| u.app.is_system).count()
    }

    fn view(&self) -> Element<'_, Message> {
        let busy = matches!(self.status, Status::Loading | Status::Updating);
        let update_all = {
            let label = if matches!(self.status, Status::Updating) {
                strings::UPDATING
            } else {
                strings::UPDATE_ALL
            };
            let b = button(label);
            if busy || self.updates.is_empty() {
                b
            } else {
                b.on_press(Message::UpdateAllRequested)
            }
        };
        let header = row![
            text(strings::APP_TITLE).size(24),
            horizontal_space(),
            button(strings::CHECK)
                .style(button::secondary)
                .on_press_maybe((!busy).then_some(Message::CheckRequested)),
            update_all,
        ]
        .spacing(12);
        let header = if self.linker.is_some() {
            // Inserted before "Update All" would reorder the row; append instead.
            header.push(
                button(strings::ADD_FROM_GITHUB)
                    .style(button::secondary)
                    .on_press_maybe((!busy).then_some(Message::Link(LinkMsg::Open))),
            )
        } else {
            header
        };

        let filters = row![
            text_input(strings::SEARCH_PLACEHOLDER, &self.search)
                .on_input(Message::SearchChanged)
                .width(Length::Fill),
            pick_list(Filter::ALL, Some(self.filter), Message::FilterChanged),
            checkbox(strings::SHOW_SYSTEM, self.show_system).on_toggle(Message::ShowSystemToggled),
        ]
        .spacing(12);

        let body: Element<'_, Message> = match &self.status {
            Status::Loading => centered(text(strings::LOADING)),
            Status::Error(e) => centered(
                column![
                    text(e.clone()),
                    button(strings::RETRY).on_press(Message::CheckRequested)
                ]
                .spacing(12),
            ),
            Status::Ready | Status::Updating => {
                let (visible, total) = self.visible();
                if visible.is_empty() {
                    centered(text(if self.apps.is_empty() || self.search.is_empty() {
                        strings::UP_TO_DATE
                    } else {
                        strings::NO_MATCHES
                    }))
                } else {
                    let mut list = column(visible.into_iter().map(|a| self.app_row(a, busy)))
                        .spacing(8)
                        .padding([0, 12]);
                    if total > MAX_ROWS {
                        list = list.push(
                            text(format!(
                                "Showing {MAX_ROWS} of {total}. {}",
                                strings::ROW_LIMIT_NOTE
                            ))
                            .size(13),
                        );
                    }
                    scrollable(list).height(Length::Fill).into()
                }
            }
        };

        let hidden = self.hidden_system_updates();
        let system_note = (hidden > 0).then(|| {
            text(format!(
                "{hidden} system package update{} included in Update All.",
                if hidden == 1 { " is" } else { "s are" }
            ))
            .size(13)
        });
        let notices = column(
            self.startup_notices
                .iter()
                .chain(&self.apply_notices)
                .chain(&self.load_notices)
                .map(|n| text(n.clone()).size(13).style(text::danger).into()),
        )
        .spacing(4)
        .push_maybe(system_note);
        let info = column(
            self.info_notices
                .iter()
                .map(|n| text(n.clone()).size(13).style(style::good).into()),
        )
        .spacing(4);

        let footer = row![checkbox(strings::SHOW_TERMINAL, self.terminal.visible)
            .on_toggle(Message::TerminalToggled)];
        // One element for the middle area in both modes, so the terminal keeps its place in
        // the widget tree. (A shifted position makes iced reset its scroll state.)
        let middle: Element<'_, Message> = if self.link.open && self.linker.is_some() {
            // The panel takes the place of the list while it is open, so everything fits.
            // Scrolls, so the Install / Link buttons stay reachable in a short window.
            scrollable(column![self.link.view(), info].spacing(16))
                .height(Length::Fill)
                .into()
        } else {
            column![filters, info, notices, body]
                .spacing(16)
                .height(Length::Fill)
                .into()
        };
        let mut page = column![header, middle].spacing(16);
        if self.terminal.visible {
            page = page.push(self.terminal.view());
        }
        page = page.push(footer);

        container(page).padding(20).into()
    }

    fn app_row<'a>(&'a self, app: &'a InstalledApp, busy: bool) -> Element<'a, Message> {
        let candidate = self.updates.iter().find(|u| u.app.id == app.id);
        let version_line = match candidate {
            Some(c) => format!("{} -> {}", app.version, c.latest_version),
            None => app.version.clone(),
        };
        let trust = text(app.provenance.trust.label())
            .size(12)
            .style(match app.provenance.trust {
                TrustLevel::Official | TrustLevel::Verified => style::good,
                TrustLevel::Community => text::primary,
                TrustLevel::Unknown => text::danger,
            });
        let action: Element<'_, Message> = match candidate {
            Some(_) => button(strings::UPDATE)
                .on_press_maybe((!busy).then(|| Message::UpdateOneRequested(app.id.clone())))
                .into(),
            None => horizontal_space().width(0).into(),
        };
        container(
            row![
                icon_view(&app.icon),
                column![
                    text(app.name.clone()).size(16),
                    text(version_line).size(13),
                    row![
                        text(format!("[{}]", app.source.kind().badge())).size(12),
                        trust,
                        text(format!("from {}", app.provenance.display_name)).size(12),
                    ]
                    .spacing(8),
                ]
                .spacing(2),
                horizontal_space(),
                action,
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center),
        )
        .padding(12)
        .width(Length::Fill)
        .style(style::card)
        .into()
    }
}

const ICON_SIZE: f32 = 36.0;

/// The app icon, or a letter tile when there is no usable image. Image decoding is cached
/// by the renderer after the first frame.
fn icon_view<'a>(icon: &package_manager_core::IconRef) -> Element<'a, Message> {
    use package_manager_core::IconRef;
    match icon {
        IconRef::File(path) => {
            let is_svg = path.extension().and_then(|e| e.to_str()) == Some("svg");
            if is_svg {
                iced::widget::svg(iced::widget::svg::Handle::from_path(path))
                    .width(ICON_SIZE)
                    .height(ICON_SIZE)
                    .into()
            } else {
                iced::widget::image(iced::widget::image::Handle::from_path(path))
                    .width(ICON_SIZE)
                    .height(ICON_SIZE)
                    .into()
            }
        }
        IconRef::Initial(c) => letter_tile(*c),
        // Theme names and URLs are resolved or downloaded elsewhere; never rendered directly.
        IconRef::Theme(_) | IconRef::Url(_) => letter_tile('?'),
    }
}

fn letter_tile<'a>(c: char) -> Element<'a, Message> {
    container(text(c.to_string()).size(20))
        .center_x(ICON_SIZE)
        .center_y(ICON_SIZE)
        .style(style::card)
        .into()
}

fn centered<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}
