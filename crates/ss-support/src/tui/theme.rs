//! The styles every screen draws with, so the look is decided in one place.

use ratatui::style::{Color, Modifier, Style};

/// The title bar.
pub fn title() -> Style {
    Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
}

/// The key hints in the bottom line.
pub fn hints() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// The status line.
pub fn status() -> Style {
    Style::new().fg(Color::Yellow)
}

/// Errors and failed steps.
pub fn error() -> Style {
    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
}

/// The selected row or focused field.
pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

/// Secondary text such as help and headers.
pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// Group headings.
pub fn heading() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Values present only on the right side of a diff, and finished steps.
pub fn added() -> Style {
    Style::new().fg(Color::Green)
}

/// Values present only on the left side of a diff.
pub fn removed() -> Style {
    Style::new().fg(Color::Red)
}

/// Values that differ between the two sides of a diff, and running steps.
pub fn changed() -> Style {
    Style::new().fg(Color::Yellow)
}
