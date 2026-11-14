//! A bordered panel that shows one message, wrapped to fit: an error, or
//! how a run ended.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::tui::theme;

/// Draws `message` in an error panel filling `area`.
pub fn render_error(frame: &mut Frame, area: Rect, message: &str) {
    render_notice(frame, area, "error", message, theme::error());
}

/// Draws `message` in a panel titled `title`, bordered in `style`, filling
/// `area`.
pub fn render_notice(frame: &mut Frame, area: Rect, title: &str, message: &str, style: Style) {
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(style);
    let text = Paragraph::new(message)
        .wrap(Wrap { trim: false })
        .block(block);
    frame.render_widget(text, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render_with;

    #[test]
    fn the_message_is_wrapped_inside_a_titled_border() {
        let text = render_with(24, 5, |frame| {
            render_error(frame, frame.area(), "S3 is not configured for dev");
        });
        assert_eq!(
            text,
            "┌ error ───────────────┐\n\
             │S3 is not configured  │\n\
             │for dev               │\n\
             │                      │\n\
             └──────────────────────┘\n"
        );
    }

    #[test]
    fn an_area_too_small_for_the_border_draws_without_panicking() {
        render_with(1, 1, |frame| render_error(frame, frame.area(), "boom"));
        render_with(0, 0, |frame| render_error(frame, frame.area(), "boom"));
    }
}
