//! "Add from GitHub": paste a repository (or its releases link), see which `.deb` will be
//! used and where it comes from, then install it or just link it for future updates.

use std::sync::Arc;

use iced::widget::{
    button, checkbox, column, container, horizontal_space, pick_list, row, text, text_input,
};
use iced::{Element, Length, Task};
use package_manager_core::{is_valid_package_name, TrustLevel};
use package_manager_runtime::{ChecksumKind, ChecksumState, RepoPreview};

use crate::{strings, style, App, Message};

#[derive(Debug, Clone)]
pub enum Stage {
    Input,
    Searching,
    Failed(String),
    Preview(Arc<RepoPreview>),
    /// Linking / installing is under way.
    Working,
}

#[derive(Debug, Clone)]
pub enum LinkMsg {
    Open,
    Close,
    InputChanged(String),
    Find,
    Found(Result<(Arc<RepoPreview>, Option<String>), String>),
    ChoicePicked(String),
    PackageChanged(String),
    InstalledChecked(String, Option<String>),
    AcceptNoChecksum(bool),
    AcceptReplace(bool),
    LinkOnly,
    LinkedOnly(Result<String, String>),
    InstallNow,
    LinkedForInstall(Result<Box<package_manager_core::UpdateCandidate>, String>),
}

pub struct LinkPanel {
    pub open: bool,
    input: String,
    stage: Stage,
    choice: usize,
    package: String,
    /// Installed version of `package`, if any.
    installed: Option<String>,
    accept_no_checksum: bool,
    /// The user agreed to replace an already-installed package with this download.
    accept_replace: bool,
}

impl LinkPanel {
    pub fn new() -> Self {
        Self {
            open: false,
            input: String::new(),
            stage: Stage::Input,
            choice: 0,
            package: String::new(),
            installed: None,
            accept_no_checksum: false,
            accept_replace: false,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn preview(&self) -> Option<&Arc<RepoPreview>> {
        match &self.stage {
            Stage::Preview(p) => Some(p),
            _ => None,
        }
    }

    fn chosen_checksum(&self) -> Option<ChecksumState> {
        self.preview()
            .and_then(|p| p.choices.get(self.choice))
            .map(|c| c.checksum)
    }

    /// Installing needs a valid package name and, for a release with no published
    /// checksum, the user's explicit confirmation.
    pub fn can_install(&self) -> bool {
        is_valid_package_name(self.package.trim())
            && (!self.needs_replace_confirmation() || self.accept_replace)
            && match self.chosen_checksum() {
                Some(ChecksumState::Missing) => self.accept_no_checksum,
                Some(_) => true,
                None => false,
            }
    }

    /// A repository the user typed in must not silently take over a package that is already
    /// installed (it may be a distribution package named after the downloaded file). Known
    /// presets, which exist to update an installed app, are exempt.
    fn needs_replace_confirmation(&self) -> bool {
        self.installed.is_some() && self.preview().is_some_and(|p| p.preset_name.is_none())
    }

    fn can_link(&self) -> bool {
        self.preview().is_some() && is_valid_package_name(self.package.trim())
    }

    pub fn view(&self) -> Element<'_, Message> {
        let busy = matches!(self.stage, Stage::Searching | Stage::Working);
        let finder = row![
            text_input(strings::LINK_PLACEHOLDER, &self.input)
                .on_input(|s| Message::Link(LinkMsg::InputChanged(s)))
                .on_submit(Message::Link(LinkMsg::Find))
                .width(Length::Fill),
            button(strings::FIND_RELEASE).on_press_maybe(
                (!busy && !self.input.trim().is_empty()).then_some(Message::Link(LinkMsg::Find))
            ),
        ]
        .spacing(12);

        let body: Option<Element<'_, Message>> = match &self.stage {
            Stage::Input => None,
            Stage::Searching => Some(text(strings::LINK_SEARCHING).size(14).into()),
            Stage::Working => Some(text(strings::UPDATING).size(14).into()),
            Stage::Failed(e) => Some(text(e.clone()).size(14).style(text::danger).into()),
            Stage::Preview(p) => Some(self.preview_view(p)),
        };

        container(
            column![
                row![
                    text(strings::ADD_FROM_GITHUB).size(18),
                    horizontal_space(),
                    button(strings::CLOSE)
                        .style(button::secondary)
                        .on_press(Message::Link(LinkMsg::Close)),
                ],
                finder,
            ]
            .push_maybe(body)
            .spacing(12),
        )
        .padding(14)
        .width(Length::Fill)
        .style(style::card)
        .into()
    }

    fn preview_view<'a>(&'a self, p: &'a RepoPreview) -> Element<'a, Message> {
        let Some(choice) = p.choices.get(self.choice) else {
            return text("").into();
        };
        let provenance = p.provenance(self.choice);
        let trust = match provenance.trust {
            TrustLevel::Verified => "Verified",
            TrustLevel::Official => "Official",
            TrustLevel::Community => "Community (a repository you linked)",
            TrustLevel::Unknown => "Unknown",
        };
        let names: Vec<String> = p.choices.iter().map(|c| c.name.clone()).collect();
        let file: Element<'_, Message> = if names.len() > 1 {
            pick_list(names, Some(choice.name.clone()), |n| {
                Message::Link(LinkMsg::ChoicePicked(n))
            })
            .into()
        } else {
            text(choice.name.clone()).size(14).into()
        };
        let checksum: Element<'_, Message> = match choice.checksum {
            ChecksumState::Published(kind) => text(format!(
                "{} ({})",
                strings::CHECKSUM_OK,
                match kind {
                    ChecksumKind::ElectronYml => "latest-linux.yml",
                    ChecksumKind::SumsFile => "checksums file",
                    ChecksumKind::Sidecar => "checksum file",
                }
            ))
            .size(13)
            .style(style::good)
            .into(),
            ChecksumState::Missing => column![
                text(strings::NO_CHECKSUM_WARNING)
                    .size(13)
                    .style(text::danger),
                checkbox(strings::NO_CHECKSUM_CONFIRM, self.accept_no_checksum)
                    .on_toggle(|on| Message::Link(LinkMsg::AcceptNoChecksum(on))),
            ]
            .spacing(4)
            .into(),
            ChecksumState::Unknown => column![].into(),
        };
        let installed = match &self.installed {
            Some(v) => format!(
                "Installed now: {v}. Installing replaces it with {}.",
                p.version
            ),
            None => "Not installed yet.".to_owned(),
        };
        let replace: Option<Element<'_, Message>> = self.needs_replace_confirmation().then(|| {
            column![
                text(strings::REPLACE_WARNING).size(13).style(text::danger),
                checkbox(strings::REPLACE_CONFIRM, self.accept_replace)
                    .on_toggle(|on| Message::Link(LinkMsg::AcceptReplace(on))),
            ]
            .spacing(4)
            .into()
        });
        let mut details = column![
            text(format!("{}  ·  {}", p.repo.full_name(), p.tag)).size(16),
            row![
                text("File:").size(14),
                file,
                text(format!("({})", megabytes(choice.size))).size(13)
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            text(format!("Trust: {trust}")).size(13),
            checksum,
            row![
                text(strings::PACKAGE_NAME).size(14),
                text_input("package-name", &self.package)
                    .on_input(|s| Message::Link(LinkMsg::PackageChanged(s)))
                    .width(260),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            text(strings::PACKAGE_HINT).size(12),
            text(installed).size(13),
        ]
        .push_maybe(replace)
        .spacing(6);
        if let Some(name) = &p.preset_name {
            details = details.push(text(format!("Known app: {name}")).size(13));
        }
        let notes = tidy_notes(&p.notes);
        if !notes.is_empty() {
            details = details.push(text(notes).size(12));
        }

        let actions = row![
            button(text(format!("Install {}", p.version))).on_press_maybe(
                self.can_install()
                    .then_some(Message::Link(LinkMsg::InstallNow))
            ),
            button(strings::LINK_ONLY)
                .style(button::secondary)
                .on_press_maybe(self.can_link().then_some(Message::Link(LinkMsg::LinkOnly))),
            button(strings::CANCEL)
                .style(button::secondary)
                .on_press(Message::Link(LinkMsg::Close)),
            text(strings::LINK_ONLY_HINT).size(12),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);

        column![details, actions].spacing(12).into()
    }
}

/// Release notes are Markdown; show them as one plain line: links reduced to their text,
/// markers and emoji removed (the UI font has no emoji), entries joined, cut short.
fn tidy_notes(notes: &str) -> String {
    let joined = notes
        .lines()
        .map(|l| strip_inline_markdown(l.trim().trim_start_matches(['#', '*', '-', '>']).trim()))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    if joined.chars().count() > 180 {
        joined.chars().take(180).collect::<String>() + "…"
    } else {
        joined
    }
}

/// `[text](url)` -> `text`; drops backticks, `**`, and characters the font cannot draw.
fn strip_inline_markdown(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match (after.find("]("), after.find(')')) {
            (Some(mid), Some(end)) if mid < end => {
                out.push_str(&after[..mid]);
                rest = &after[end + 1..];
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "")
        .chars()
        .filter(|&c| c != '`' && is_drawable(c))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// ASCII, Latin letters, and common punctuation. Emoji and symbols outside the UI font would
/// show as empty boxes.
fn is_drawable(c: char) -> bool {
    c.is_ascii() && !c.is_ascii_control()
        || ('\u{00A0}'..='\u{024F}').contains(&c)
        || "…·’‘“”–—".contains(c)
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

impl App {
    /// Handles the "Add from GitHub" messages. Everything slow runs in a task.
    pub(crate) fn link_update(&mut self, msg: LinkMsg) -> Task<Message> {
        let Some(linker) = self.linker.clone() else {
            return Task::none();
        };
        match msg {
            LinkMsg::Open => {
                self.link.open = true;
                Task::none()
            }
            LinkMsg::Close => {
                self.link.reset();
                Task::none()
            }
            LinkMsg::InputChanged(s) => {
                self.link.input = s;
                Task::none()
            }
            LinkMsg::Find => {
                if self.link.input.trim().is_empty() || matches!(self.link.stage, Stage::Searching)
                {
                    return Task::none();
                }
                self.link.stage = Stage::Searching;
                let input = self.link.input.clone();
                Task::perform(
                    async move {
                        let p = linker.preview(&input).await.map_err(|e| e.to_string())?;
                        let installed = linker.installed_version(&p.suggested_package).await;
                        Ok((Arc::new(p), installed))
                    },
                    |r| Message::Link(LinkMsg::Found(r)),
                )
            }
            LinkMsg::Found(Ok((preview, installed))) => {
                self.link.package = preview.suggested_package.clone();
                self.link.choice = 0;
                self.link.installed = installed;
                self.link.accept_replace = false;
                self.link.accept_no_checksum = false;
                self.link.stage = Stage::Preview(preview);
                Task::none()
            }
            LinkMsg::Found(Err(e)) => {
                self.link.stage = Stage::Failed(e);
                Task::none()
            }
            LinkMsg::ChoicePicked(name) => {
                let picked = self.link.preview().and_then(|p| {
                    let i = p.choices.iter().position(|c| c.name == name)?;
                    Some((i, p.choices[i].suggested_package.clone()))
                });
                let Some((i, package)) = picked else {
                    return Task::none();
                };
                self.link.choice = i;
                self.link.accept_no_checksum = false;
                // A different file can be a different package (e.g. `bat` vs `bat-musl`).
                self.link.package = package.clone();
                self.link.installed = None;
                self.link.accept_replace = false;
                Task::perform(
                    async move {
                        let v = linker.installed_version(&package).await;
                        (package, v)
                    },
                    |(n, v)| Message::Link(LinkMsg::InstalledChecked(n, v)),
                )
            }
            LinkMsg::PackageChanged(name) => {
                self.link.package = name.clone();
                self.link.installed = None;
                self.link.accept_replace = false;
                Task::perform(
                    async move {
                        let v = linker.installed_version(name.trim()).await;
                        (name, v)
                    },
                    |(n, v)| Message::Link(LinkMsg::InstalledChecked(n, v)),
                )
            }
            LinkMsg::InstalledChecked(name, version) => {
                // Ignore answers for a name the user has since edited.
                if name == self.link.package {
                    self.link.installed = version;
                    self.link.accept_replace = false;
                }
                Task::none()
            }
            LinkMsg::AcceptReplace(on) => {
                self.link.accept_replace = on;
                Task::none()
            }
            LinkMsg::AcceptNoChecksum(on) => {
                self.link.accept_no_checksum = on;
                Task::none()
            }
            LinkMsg::LinkOnly => {
                let (Some(preview), true) = (self.link.preview().cloned(), self.link.can_link())
                else {
                    return Task::none();
                };
                let (choice, package) = (self.link.choice, self.link.package.clone());
                self.link.stage = Stage::Working;
                Task::perform(
                    async move {
                        let t = linker
                            .link(&preview, choice, &package)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(format!(
                            "{} is linked to {}",
                            t.package_name,
                            t.repo.full_name()
                        ))
                    },
                    |r| Message::Link(LinkMsg::LinkedOnly(r)),
                )
            }
            LinkMsg::LinkedOnly(Ok(done)) => {
                self.link.reset();
                self.info_notices.push(format!(
                    "{done}. It shows up here once the package is installed, and updates come from that repository."
                ));
                self.reload()
            }
            LinkMsg::LinkedOnly(Err(e)) => {
                self.link.stage = Stage::Failed(e);
                Task::none()
            }
            LinkMsg::InstallNow => {
                let (Some(preview), true) = (self.link.preview().cloned(), self.link.can_install())
                else {
                    return Task::none();
                };
                let (choice, package) = (self.link.choice, self.link.package.clone());
                let missing = preview
                    .choices
                    .get(choice)
                    .is_some_and(|c| c.checksum == ChecksumState::Missing);
                let consent = self.github_consent.clone();
                self.link.stage = Stage::Working;
                Task::perform(
                    async move {
                        let tracked = linker
                            .link(&preview, choice, &package)
                            .await
                            .map_err(|e| e.to_string())?;
                        let candidate = linker
                            .install_candidate(&preview, choice, &tracked)
                            .ok_or_else(|| "That download is no longer available.".to_owned())?;
                        // The user ticked the box in the panel; it covers this one install.
                        if missing {
                            consent.allow(&candidate.app.id);
                        }
                        Ok(Box::new(candidate))
                    },
                    |r| Message::Link(LinkMsg::LinkedForInstall(r)),
                )
            }
            LinkMsg::LinkedForInstall(Ok(candidate)) => {
                self.link.reset();
                self.start_apply(vec![*candidate])
            }
            LinkMsg::LinkedForInstall(Err(e)) => {
                self.link.stage = Stage::Failed(e);
                Task::none()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use package_manager_runtime::{AssetChoice, RepoPreview};

    use super::*;

    fn preview(checksum: ChecksumState) -> Arc<RepoPreview> {
        Arc::new(RepoPreview {
            repo: package_manager_core::GithubRepo::parse("o/r").expect("repo"),
            tag: "v1.0".into(),
            version: "1.0".into(),
            release_url: "https://github.com/o/r/releases/tag/v1.0".into(),
            notes: String::new(),
            choices: vec![AssetChoice {
                name: "tool_1.0_amd64.deb".into(),
                size: 1_500_000,
                url: "https://github.com/o/r/releases/download/v1.0/tool_1.0_amd64.deb".into(),
                pattern: "tool_{version}_amd64.deb".into(),
                checksum,
                suggested_package: "tool".into(),
            }],
            preset_name: None,
            suggested_package: "tool".into(),
        })
    }

    fn panel(checksum: ChecksumState, package: &str) -> LinkPanel {
        let mut p = LinkPanel::new();
        p.stage = Stage::Preview(preview(checksum));
        p.package = package.into();
        p
    }

    #[test]
    fn installing_needs_a_preview_and_a_valid_package_name() {
        assert!(!LinkPanel::new().can_install());
        let published = ChecksumState::Published(ChecksumKind::SumsFile);
        assert!(panel(published, "tool").can_install());
        for bad in ["", " ", "-x", "Tool", "a b", "../x", "x"] {
            assert!(!panel(published, bad).can_install(), "{bad:?}");
            assert!(!panel(published, bad).can_link(), "{bad:?}");
        }
        assert!(
            panel(published, " tool ").can_install(),
            "surrounding spaces are trimmed"
        );
    }

    #[test]
    fn a_release_without_checksum_needs_the_explicit_tick() {
        let mut p = panel(ChecksumState::Missing, "tool");
        assert!(!p.can_install(), "blocked until the user confirms");
        assert!(
            p.can_link(),
            "linking alone installs nothing, so it is allowed"
        );
        p.accept_no_checksum = true;
        assert!(p.can_install());
    }

    #[test]
    fn closing_resets_everything_including_the_confirmation() {
        let mut p = panel(ChecksumState::Missing, "tool");
        p.accept_no_checksum = true;
        p.input = "o/r".into();
        p.open = true;
        p.reset();
        assert!(!p.open && !p.accept_no_checksum && p.input.is_empty() && p.package.is_empty());
        assert!(!p.can_install());
    }

    #[test]
    fn sizes_are_shown_in_megabytes() {
        assert_eq!(megabytes(178_442_846), "178.4 MB");
        assert_eq!(megabytes(0), "0.0 MB");
    }

    #[test]
    fn a_typed_in_repo_cannot_replace_an_installed_package_without_the_tick() {
        let published = ChecksumState::Published(ChecksumKind::SumsFile);
        let mut p = panel(published, "tool");
        assert!(p.can_install(), "nothing installed: no extra confirmation");
        p.installed = Some("2.46.0-4".into());
        assert!(!p.can_install(), "would replace an installed package");
        assert!(p.can_link(), "linking alone changes nothing on the system");
        p.accept_replace = true;
        assert!(p.can_install());
    }

    #[test]
    fn known_presets_may_update_an_installed_app_directly() {
        let published = ChecksumState::Published(ChecksumKind::ElectronYml);
        let mut p = panel(published, "hydralauncher");
        if let Stage::Preview(prev) = &mut p.stage {
            Arc::make_mut(prev).preset_name = Some("Hydra Launcher".into());
        }
        p.installed = Some("4.3.0".into());
        assert!(p.can_install());
    }

    #[test]
    fn release_notes_become_one_short_plain_line() {
        let md =
            "## What's Changed\n* fix(a): stop crash\n- feat(b): new thing\n\n**Full Changelog**";
        assert_eq!(
            tidy_notes(md),
            "What's Changed · fix(a): stop crash · feat(b): new thing · Full Changelog"
        );
        assert_eq!(tidy_notes(""), "");
        assert_eq!(tidy_notes("   \n\n"), "");
        let long = "x".repeat(500);
        assert_eq!(tidy_notes(&long).chars().count(), 181);
    }

    #[test]
    fn notes_lose_links_code_marks_and_emoji() {
        assert_eq!(
            tidy_notes("## What's Changed\n### \u{1F41B} Fixes\n* explain `--no-refresh` by @a in [#14815](https://github.com/cli/cli/pull/14815)"),
            "What's Changed · Fixes · explain --no-refresh by @a in #14815"
        );
        assert_eq!(
            strip_inline_markdown("a [broken(link) b"),
            "a [broken(link) b"
        );
        assert_eq!(
            strip_inline_markdown("caf\u{e9} \u{2192} ok"),
            "caf\u{e9} ok"
        );
    }
}
