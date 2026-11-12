//! Development-only: saves PNG screenshots of the running window so the UI can be reviewed
//! without a person at the keyboard. Compiled only with `--features screenshots`.
//!
//! `package-manager-app --screenshot-dir DIR [--demo-repo owner/repo] [--prefix NAME]`
//! waits for the list, saves `NAME-list.png`, then (with `--demo-repo`) opens "Add from
//! GitHub", looks the repo up, saves `NAME-panel.png`, and exits.

use std::path::PathBuf;
use std::time::Duration;

use iced::window::Screenshot;
use iced::Task;

use crate::link_panel::LinkMsg;
use crate::{App, Message};

pub struct Shots {
    dir: PathBuf,
    prefix: String,
    repo: Option<String>,
    step: u8,
}

#[derive(Debug, Clone)]
pub enum ShotMsg {
    Take(String),
    Captured(String, Screenshot),
    Saved(String),
}

/// What just happened in the app that a screenshot may follow.
pub enum Event {
    Loaded,
    Found,
}

pub fn from_args() -> Option<Shots> {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    Some(Shots {
        dir: PathBuf::from(value("--screenshot-dir")?),
        prefix: value("--prefix").unwrap_or_else(|| "shot".to_owned()),
        repo: value("--demo-repo"),
        step: 0,
    })
}

pub fn observe(message: &Message) -> Option<Event> {
    match message {
        Message::Loaded(_) => Some(Event::Loaded),
        Message::Link(LinkMsg::Found(_)) => Some(Event::Found),
        _ => None,
    }
}

fn after_delay(name: &str) -> Task<Message> {
    let name = name.to_owned();
    Task::future(async move {
        // Icons and the first frame need a moment to appear.
        tokio::time::sleep(Duration::from_millis(1800)).await;
        Message::Shot(ShotMsg::Take(name))
    })
}

impl App {
    pub(crate) fn shots_event(&mut self, event: Event) -> Task<Message> {
        let Some(shots) = self.shots.as_mut() else {
            return Task::none();
        };
        match (event, shots.step) {
            (Event::Loaded, 0) => {
                shots.step = 1;
                after_delay("list")
            }
            (Event::Found, 2) => {
                shots.step = 3;
                after_delay("panel")
            }
            _ => Task::none(),
        }
    }

    pub(crate) fn shots_update(&mut self, msg: ShotMsg) -> Task<Message> {
        match msg {
            ShotMsg::Take(name) => iced::window::get_latest()
                .then(|id| match id {
                    Some(id) => iced::window::screenshot(id),
                    None => Task::none(),
                })
                .map(move |shot| Message::Shot(ShotMsg::Captured(name.clone(), shot))),
            ShotMsg::Captured(name, shot) => {
                let Some(shots) = self.shots.as_ref() else {
                    return Task::none();
                };
                let path = shots.dir.join(format!("{}-{name}.png", shots.prefix));
                Task::perform(
                    async move {
                        let (w, h) = (shot.size.width, shot.size.height);
                        let saved = tokio::task::spawn_blocking(move || {
                            std::fs::create_dir_all(
                                path.parent().unwrap_or(std::path::Path::new(".")),
                            )
                            .ok();
                            image::save_buffer(&path, &shot.bytes, w, h, image::ColorType::Rgba8)
                        })
                        .await;
                        match saved {
                            Ok(Ok(())) => eprintln!("saved screenshot {name} ({w}x{h})"),
                            other => eprintln!("could not save screenshot {name}: {other:?}"),
                        }
                        name
                    },
                    |name| Message::Shot(ShotMsg::Saved(name)),
                )
            }
            ShotMsg::Saved(name) => {
                let Some(shots) = self.shots.as_mut() else {
                    return Task::none();
                };
                match (name.as_str(), shots.repo.clone()) {
                    ("list", Some(repo)) => {
                        shots.step = 2;
                        Task::done(Message::Link(LinkMsg::Open))
                            .chain(Task::done(Message::Link(LinkMsg::InputChanged(repo))))
                            .chain(Task::done(Message::Link(LinkMsg::Find)))
                    }
                    _ => iced::exit(),
                }
            }
        }
    }
}
