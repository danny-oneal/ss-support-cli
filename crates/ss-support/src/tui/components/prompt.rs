//! A one-line question answered by typing, such as the name of a preset.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use crate::tui::widgets::text_input::TextInput;

/// What a key did to a [`Prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptEvent {
    /// Still open.
    Pending,
    /// Answered with this text, trimmed and not empty.
    Submit(String),
    /// Dismissed without an answer.
    Cancel,
}

/// A labelled one-line text prompt.
#[derive(Debug, Clone)]
pub struct Prompt {
    label: String,
    input: TextInput,
}

impl Prompt {
    /// An empty prompt shown as `<label>: `.
    pub fn new(label: &str) -> Self {
        Prompt {
            label: format!("{label}: "),
            input: TextInput::default(),
        }
    }

    /// Applies a key: enter submits a non-empty answer, esc cancels, and
    /// everything else edits the text.
    pub fn handle(&mut self, key: KeyEvent) -> PromptEvent {
        match key.code {
            KeyCode::Esc => PromptEvent::Cancel,
            KeyCode::Enter => match self.input.value().trim() {
                "" => PromptEvent::Pending,
                answer => PromptEvent::Submit(answer.to_owned()),
            },
            _ => {
                self.input.handle(key);
                PromptEvent::Pending
            }
        }
    }

    /// Adds pasted text to the answer. A paste never submits.
    pub fn paste(&mut self, text: &str) {
        self.input.paste(text);
    }

    /// Draws the prompt on the first row of `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let label_width = u16::try_from(self.label.width()).unwrap_or(u16::MAX);
        let [label, field] =
            Layout::horizontal([Constraint::Length(label_width), Constraint::Min(0)]).areas(Rect {
                height: area.height.min(1),
                ..area
            });
        frame.render_widget(Line::from(self.label.as_str()), label);
        self.input.render(frame, field, true);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyCode;

    use super::*;
    use crate::tui::testing::{key, render_with, typed};

    fn after(keys: &[KeyEvent]) -> (Prompt, PromptEvent) {
        let mut prompt = Prompt::new("preset name");
        let mut last = PromptEvent::Pending;
        for &k in keys {
            last = prompt.handle(k);
        }
        (prompt, last)
    }

    #[test]
    fn typing_then_enter_submits_the_trimmed_text() {
        let mut keys = typed("  nightly ");
        keys.push(key(KeyCode::Enter));

        assert_eq!(after(&keys).1, PromptEvent::Submit("nightly".to_owned()));
    }

    #[test]
    fn typing_keeps_the_prompt_open() {
        assert_eq!(after(&typed("ni")).1, PromptEvent::Pending);
    }

    #[test]
    fn enter_on_an_empty_prompt_stays_open() {
        assert_eq!(after(&[key(KeyCode::Enter)]).1, PromptEvent::Pending);
        let mut keys = typed("   ");
        keys.push(key(KeyCode::Enter));
        assert_eq!(after(&keys).1, PromptEvent::Pending);
    }

    #[test]
    fn esc_cancels() {
        let mut keys = typed("nightly");
        keys.push(key(KeyCode::Esc));

        assert_eq!(after(&keys).1, PromptEvent::Cancel);
    }

    #[test]
    fn the_label_and_the_text_are_drawn_on_one_row() {
        let (prompt, _) = after(&typed("nightly"));

        let text = render_with(30, 1, |frame| prompt.render(frame, frame.area()));

        assert_eq!(text, "preset name: nightly\n");
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let (prompt, _) = after(&typed("nightly"));

        render_with(3, 1, |frame| prompt.render(frame, frame.area()));
        render_with(3, 1, |frame| prompt.render(frame, Rect::new(0, 1, 3, 0)));
    }

    #[test]
    fn a_pasted_answer_is_text_and_does_not_submit() {
        let mut prompt = Prompt::new("preset name");

        prompt.paste("nightly\n");

        let text = render_with(30, 1, |frame| prompt.render(frame, frame.area()));
        assert_eq!(text, "preset name: nightly\n");
        assert_eq!(
            prompt.handle(key(KeyCode::Enter)),
            PromptEvent::Submit("nightly".to_owned())
        );
    }
}
