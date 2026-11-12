//! Shared widget styles, derived from the theme palette (no hard-coded colors).

use iced::widget::container;
use iced::{border, Theme};

/// A bordered card on the normal background. Text colors (success green, danger red) keep
/// their contrast here, which they lose on the lighter `rounded_box` fill.
pub fn card(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.color.into()),
        border: border::rounded(8)
            .color(palette.background.strong.color)
            .width(1),
        ..container::Style::default()
    }
}

/// "Good" text (Official / Verified / checksum published). The plain `text::success` is a dark
/// green that is hard to read on the dark background; the palette's strong shade is not.
pub fn good(theme: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(theme.extended_palette().success.strong.color),
    }
}
