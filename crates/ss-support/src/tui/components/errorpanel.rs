//! A bordered panel that shows one error message, wrapped to fit.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::tui::theme;

/// Draws `message` in an error panel filling `area`.
pub fn render_error(frame: &mut Frame, area: Rect, message: &str) {
    let block = Block::bordered()
        .title(" error ")
        .border_style(theme::error());
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
