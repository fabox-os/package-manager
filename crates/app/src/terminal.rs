//! Read-only log viewer styled as a terminal. It only renders what the `LogBroadcaster`
//! delivers; there is no input and no shell.

use std::collections::VecDeque;

use iced::widget::{button, column, container, horizontal_space, row, scrollable, text};
use iced::{Element, Font, Length, Task};
use package_manager_core::log::{LogEvent, LogKind, MAX_LINES};

use crate::{strings, Message};

const HEIGHT_NORMAL: f32 = 180.0;
const HEIGHT_EXPANDED: f32 = 320.0;
/// Within this fraction of the end, the view counts as "at the bottom".
const BOTTOM_EPSILON: f32 = 0.01;

fn scroll_id() -> scrollable::Id {
    scrollable::Id::new("terminal")
}

pub struct Terminal {
    pub visible: bool,
    expanded: bool,
    lines: VecDeque<String>,
    /// `lines` joined, kept in sync so `view` does not rebuild it every frame.
    text: String,
    /// Auto-scroll while the user has not scrolled up.
    stick_to_bottom: bool,
}

impl Terminal {
    pub fn new(visible: bool) -> Self {
        Self {
            visible,
            expanded: false,
            lines: VecDeque::new(),
            text: String::new(),
            stick_to_bottom: true,
        }
    }

    /// Appends events (dropping the oldest past the cap) and scrolls if following.
    pub fn push(&mut self, events: Vec<LogEvent>) -> Task<Message> {
        let mut trimmed = false;
        let mut added = false;
        for e in events.into_iter().filter(|e| e.kind != LogKind::RunStart) {
            if !self.text.is_empty() {
                self.text.push('\n');
            }
            let line = e.render();
            self.text.push_str(&line);
            self.lines.push_back(line);
            added = true;
            while self.lines.len() > MAX_LINES {
                self.lines.pop_front();
                trimmed = true;
            }
        }
        if trimmed {
            self.text = self
                .lines
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n");
        }
        if added && self.stick_to_bottom {
            self.snap_to_bottom()
        } else {
            Task::none()
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.text.clear();
        self.stick_to_bottom = true;
    }

    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn scrolled(&mut self, relative_y: f32) {
        self.stick_to_bottom = relative_y >= 1.0 - BOTTOM_EPSILON;
    }

    pub fn jump_to_bottom(&mut self) -> Task<Message> {
        self.stick_to_bottom = true;
        self.snap_to_bottom()
    }

    fn snap_to_bottom(&self) -> Task<Message> {
        scrollable::snap_to(scroll_id(), scrollable::RelativeOffset::END)
    }

    pub fn view(&self) -> Element<'_, Message> {
        let mut header = row![
            button(strings::COPY).on_press(Message::TerminalCopy),
            button(strings::CLEAR).on_press(Message::TerminalClear),
            button(if self.expanded {
                strings::SHRINK
            } else {
                strings::EXPAND
            })
            .on_press(Message::TerminalExpand),
            horizontal_space(),
        ]
        .spacing(8);
        if !self.stick_to_bottom {
            header = header.push(button(strings::JUMP_TO_BOTTOM).on_press(Message::TerminalJump));
        }

        let body: Element<'_, Message> = if self.text.is_empty() {
            text(strings::NO_OUTPUT)
                .size(13)
                .font(Font::MONOSPACE)
                .into()
        } else {
            scrollable(
                text(&self.text)
                    .size(13)
                    .font(Font::MONOSPACE)
                    .width(Length::Fill),
            )
            .id(scroll_id())
            .on_scroll(|v| Message::TerminalScrolled(v.relative_offset().y))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
        };

        container(column![header, body].spacing(8))
            .padding(10)
            .width(Length::Fill)
            .height(Length::Fixed(if self.expanded {
                HEIGHT_EXPANDED
            } else {
                HEIGHT_NORMAL
            }))
            .style(container::dark)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str) -> LogEvent {
        LogEvent::output(None, s)
    }

    #[test]
    fn keeps_at_most_max_lines_and_drops_the_oldest() {
        let mut t = Terminal::new(true);
        let _ = t.push(
            (0..MAX_LINES + 50)
                .map(|i| ev(&format!("line {i}")))
                .collect(),
        );
        assert_eq!(t.lines.len(), MAX_LINES);
        assert!(t.text().starts_with("line 50\n"));
        assert!(t.text().ends_with(&format!("line {}", MAX_LINES + 49)));
    }

    #[test]
    fn run_start_is_not_displayed_and_clear_empties() {
        let mut t = Terminal::new(true);
        let _ = t.push(vec![LogEvent::run_start(), ev("hello")]);
        assert_eq!(t.text(), "hello");
        t.clear();
        assert_eq!(t.text(), "");
    }

    #[test]
    fn scrolling_up_stops_auto_follow_until_back_at_bottom() {
        let mut t = Terminal::new(true);
        t.scrolled(0.4);
        assert!(!t.stick_to_bottom);
        t.scrolled(1.0);
        assert!(t.stick_to_bottom);
    }
}
