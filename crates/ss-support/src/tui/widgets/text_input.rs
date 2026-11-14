//! A single-line text field that edits by grapheme and scrolls sideways to
//! keep the cursor in view.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A single-line text field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    value: String,
    /// Byte offset of the cursor; always on a character boundary.
    cursor: usize,
}

impl TextInput {
    /// A field holding `initial` with the cursor after it.
    pub fn new(initial: &str) -> Self {
        TextInput {
            value: initial.to_owned(),
            cursor: initial.len(),
        }
    }

    /// The text typed so far.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replaces the text and moves the cursor after it.
    pub fn set(&mut self, value: &str) {
        value.clone_into(&mut self.value);
        self.cursor = self.value.len();
    }

    /// Applies an editing key. Returns false for keys the field does not
    /// use, so the caller can give them another meaning.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let chord = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.value.clear();
                self.cursor = 0;
            }
            KeyCode::Char(c) if !chord && !c.is_control() => {
                self.value.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                let start = self.previous_boundary();
                self.value.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Delete => {
                let end = self.next_boundary();
                self.value.replace_range(self.cursor..end, "");
            }
            KeyCode::Left => self.cursor = self.previous_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.len(),
            _ => return false,
        }
        true
    }

    /// Inserts pasted text at the cursor as one line: line breaks and other
    /// control characters are left out.
    pub fn paste(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            self.value.insert(self.cursor, c);
            self.cursor += c.len_utf8();
        }
    }

    /// How many terminal columns the text before the cursor occupies.
    #[cfg(test)]
    pub(crate) fn cursor_col(&self) -> u16 {
        u16::try_from(columns(self.before_cursor())).unwrap_or(u16::MAX)
    }

    /// Draws the field on the first row of `area`, showing the part of the
    /// text around the cursor, and places the terminal cursor when `focused`.
    pub fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let before = columns(self.before_cursor());
        // The cursor takes a column of its own; whole graphemes are hidden
        // on the left until it has one.
        let mut to_hide = before.saturating_sub(width - 1);
        let mut hidden = 0;
        let mut shown = 0;
        let mut visible = String::new();
        for grapheme in drawable(&self.value) {
            let cols = grapheme.width();
            if to_hide > 0 {
                to_hide = to_hide.saturating_sub(cols);
                hidden += cols;
            } else if shown + cols <= width {
                visible.push_str(grapheme);
                shown += cols;
            } else {
                break;
            }
        }
        frame.render_widget(Line::from(visible), Rect { height: 1, ..area });
        if focused {
            let col = before.saturating_sub(hidden).min(width - 1);
            let x = area
                .x
                .saturating_add(u16::try_from(col).unwrap_or(u16::MAX));
            frame.set_cursor_position((x, area.y));
        }
    }

    fn before_cursor(&self) -> &str {
        self.value.get(..self.cursor).unwrap_or(&self.value)
    }

    /// The start of the grapheme that ends at or before the cursor.
    fn previous_boundary(&self) -> usize {
        self.before_cursor()
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(start, _)| start)
    }

    /// The end of the grapheme that starts at the cursor.
    fn next_boundary(&self) -> usize {
        self.value
            .get(self.cursor..)
            .and_then(|rest| rest.graphemes(true).next())
            .map_or(self.cursor, |grapheme| self.cursor + grapheme.len())
    }
}

/// The graphemes of `text` a terminal cell can hold: control characters are
/// left out, as they are when the text is drawn.
fn drawable(text: &str) -> impl Iterator<Item = &str> {
    text.graphemes(true)
        .filter(|grapheme| !grapheme.contains(char::is_control))
}

/// How many terminal columns `text` occupies when drawn.
fn columns(text: &str) -> usize {
    drawable(text).map(UnicodeWidthStr::width).sum()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::tui::testing::{ch, ctrl, key, render_with, screen_text, typed};

    fn field(initial: &str, keys: &[KeyEvent]) -> TextInput {
        let mut input = TextInput::new(initial);
        for &k in keys {
            input.handle(k);
        }
        input
    }

    fn drawn(input: &TextInput, width: u16) -> String {
        render_with(width, 1, |frame| input.render(frame, frame.area(), true))
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let input = field("ac", &[key(KeyCode::Left), ch('b')]);

        assert_eq!(input.value(), "abc");
        assert_eq!(input.cursor_col(), 2);
    }

    #[test]
    fn typing_into_an_empty_field_builds_the_value() {
        let input = field("", &typed("s3://b/k"));

        assert_eq!(input.value(), "s3://b/k");
    }

    #[test]
    fn shifted_characters_are_inserted() {
        let mut input = TextInput::new("");

        let used = input.handle(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));

        assert!(used);
        assert_eq!(input.value(), "A");
    }

    #[test]
    fn backspace_and_delete_remove_one_grapheme() {
        // A thumbs-up with a skin tone is two code points and one grapheme.
        let input = field("a👍🏽b", &[key(KeyCode::Left), key(KeyCode::Backspace)]);
        assert_eq!(input.value(), "ab");

        let input = field(
            "a👍🏽b",
            &[
                key(KeyCode::Home),
                key(KeyCode::Right),
                key(KeyCode::Delete),
            ],
        );
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn backspace_at_the_start_and_delete_at_the_end_change_nothing() {
        let input = field(
            "ab",
            &[
                key(KeyCode::Delete),
                key(KeyCode::Home),
                key(KeyCode::Backspace),
            ],
        );

        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn left_right_home_and_end_move_the_cursor() {
        let mut input = TextInput::new("abcd");
        assert_eq!(input.cursor_col(), 4);

        input.handle(key(KeyCode::Left));
        input.handle(key(KeyCode::Left));
        assert_eq!(input.cursor_col(), 2);

        input.handle(key(KeyCode::Right));
        assert_eq!(input.cursor_col(), 3);

        input.handle(key(KeyCode::Home));
        assert_eq!(input.cursor_col(), 0);
        assert!(input.handle(key(KeyCode::Left)), "consumed at the edge");
        assert_eq!(input.cursor_col(), 0);

        input.handle(key(KeyCode::End));
        assert!(input.handle(key(KeyCode::Right)), "consumed at the edge");
        assert_eq!(input.cursor_col(), 4);
    }

    #[test]
    fn wide_characters_count_as_two_columns() {
        let mut input = TextInput::new("日本");
        assert_eq!(input.cursor_col(), 4);

        input.handle(key(KeyCode::Left));
        assert_eq!(input.cursor_col(), 2);
    }

    #[test]
    fn a_combining_mark_is_deleted_with_its_base() {
        let input = field("e\u{301}", &[key(KeyCode::Backspace)]);

        assert_eq!(input.value(), "");
        assert_eq!(input.cursor_col(), 0);
    }

    #[test]
    fn ctrl_u_clears_the_line() {
        let mut input = TextInput::new("some text");

        assert!(input.handle(ctrl('u')));

        assert_eq!(input.value(), "");
        assert_eq!(input.cursor_col(), 0);
    }

    #[test]
    fn control_and_alt_chords_are_not_inserted() {
        let mut input = TextInput::new("x");

        assert!(!input.handle(ctrl('s')));
        assert!(!input.handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)));
        assert!(!input.handle(key(KeyCode::Enter)));
        assert!(!input.handle(key(KeyCode::Tab)));
        assert!(!input.handle(key(KeyCode::Esc)));

        assert_eq!(input.value(), "x");
    }

    #[test]
    fn set_replaces_the_text_and_moves_the_cursor_to_the_end() {
        let mut input = field("abc", &[key(KeyCode::Home)]);

        input.set("日本");

        assert_eq!(input.value(), "日本");
        assert_eq!(input.cursor_col(), 4);
    }

    #[test]
    fn a_short_value_is_drawn_from_the_start() {
        assert_eq!(drawn(&TextInput::new("abc"), 10), "abc\n");
    }

    #[test]
    fn a_long_value_scrolls_to_keep_the_cursor_visible() {
        let input = TextInput::new("0123456789abcdefghijklmnopqrst");

        // Nine columns of text and one for the cursor after it.
        assert_eq!(drawn(&input, 10), "lmnopqrst\n");
    }

    #[test]
    fn moving_back_into_a_long_value_scrolls_it_back() {
        let input = field("0123456789abcdefghijklmnopqrst", &[key(KeyCode::Home)]);

        assert_eq!(drawn(&input, 10), "0123456789\n");
    }

    #[test]
    fn scrolling_never_splits_a_wide_character() {
        let input = TextInput::new("日本語日本語");

        // Five columns: two whole characters and the cursor.
        assert_eq!(drawn(&input, 5), "本語\n");
    }

    #[test]
    fn control_characters_in_the_value_are_not_drawn() {
        let input = TextInput::new("a\x1b[2Jb\tc");

        assert_eq!(drawn(&input, 20), "a[2Jbc\n");
    }

    #[test]
    fn the_terminal_cursor_sits_after_the_text_when_focused() {
        let input = TextInput::new("日本");
        let mut term = Terminal::new(TestBackend::new(10, 2)).unwrap();

        term.draw(|frame| input.render(frame, Rect::new(3, 1, 7, 1), true))
            .unwrap();

        term.backend_mut().assert_cursor_position((7, 1));
        assert_eq!(screen_text(&term), "\n   日本\n");
    }

    #[test]
    fn an_unfocused_field_does_not_place_the_cursor() {
        let input = TextInput::new("abc");
        let mut term = Terminal::new(TestBackend::new(10, 1)).unwrap();
        term.backend_mut().set_cursor_position((0, 0)).unwrap();

        term.draw(|frame| input.render(frame, frame.area(), false))
            .unwrap();

        term.backend_mut().assert_cursor_position((0, 0));
    }

    #[test]
    fn a_zero_width_area_renders_nothing() {
        let input = TextInput::new("abc");

        render_with(4, 1, |frame| {
            input.render(frame, Rect::new(0, 0, 0, 1), true);
            input.render(frame, Rect::new(0, 0, 4, 0), true);
        });
    }

    #[test]
    fn pasted_text_is_inserted_at_the_cursor_without_line_breaks_or_controls() {
        let mut input = field("ab", &[key(KeyCode::Left)]);

        input.paste("x\r\ny\x1b[2J\tz");

        assert_eq!(input.value(), "axy[2Jzb");
        assert_eq!(input.cursor_col(), 7);
    }
}
