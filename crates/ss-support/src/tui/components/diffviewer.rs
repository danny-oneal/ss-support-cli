//! An interactive view of a semantic diff: step through changes, switch
//! between a unified and a side-by-side layout, collapse to one line per
//! change, and filter by path.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ss_core::diff::{Change, Delta, Kind};
use ss_core::jsonutil;

use crate::tui::text::clean;
use crate::tui::theme;
use crate::tui::widgets::text_input::TextInput;

/// How many changes a page key moves by.
const PAGE: usize = 10;

/// How the changes are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Each change under its path, old value above new.
    Unified,
    /// One row per change: path, old value, new value.
    Split,
}

/// The state of one diff on screen.
pub struct DiffViewer {
    left: String,
    right: String,
    delta: Delta,
    mode: Mode,
    /// One line per change instead of one block per change.
    collapsed: bool,
    /// Shows only the changes whose path contains this text.
    filter: TextInput,
    /// Keys are going to the filter.
    filtering: bool,
    /// Index of the selected change among the shown ones.
    selected: usize,
}

impl DiffViewer {
    /// A viewer for `delta` between the sides labelled `left` and `right`.
    pub fn new(left: String, right: String, delta: Delta) -> Self {
        DiffViewer {
            left,
            right,
            delta,
            mode: Mode::Unified,
            collapsed: false,
            filter: TextInput::default(),
            filtering: false,
            selected: 0,
        }
    }

    /// The current layout.
    #[cfg(test)]
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// Applies a key. Returns false for keys the viewer does not use.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => self.clear_filter(),
                _ => {
                    if self.filter.handle(key) {
                        self.selected = 0;
                    }
                }
            }
            return true;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let last = self.shown().len().saturating_sub(1);
        match key.code {
            KeyCode::Char('n') | KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::Char('p') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::PageDown => self.selected = (self.selected + PAGE).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(PAGE),
            KeyCode::Char('g') => self.selected = 0,
            KeyCode::Char('s') => {
                self.mode = match self.mode {
                    Mode::Unified => Mode::Split,
                    Mode::Split => Mode::Unified,
                };
            }
            KeyCode::Char('c') => self.collapsed = !self.collapsed,
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value().is_empty() => self.clear_filter(),
            _ => return false,
        }
        true
    }

    /// Adds pasted text to the path filter while it is open; otherwise the
    /// paste is dropped.
    pub fn paste(&mut self, text: &str) {
        if self.filtering {
            self.filter.paste(text);
            self.selected = 0;
        }
    }

    /// Draws the diff into `area`, keeping the selected change in view.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let shown = self.shown();
        let filtered = self.filtering || !self.filter.value().is_empty();
        let mut header = format!(
            "diff {} → {}   ({})",
            self.left, self.right, self.delta.stats
        );
        if filtered {
            header.push_str(&format!(
                "   {} of {} shown",
                shown.len(),
                self.delta.changes.len()
            ));
        }
        let [header_row, filter_row, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(filtered)),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(Line::styled(clean(&header), theme::heading()), header_row);
        if filtered {
            let [marker, field] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(filter_row);
            frame.render_widget(Line::from("/ "), marker);
            self.filter.render(frame, field, self.filtering);
        }

        if self.delta.changes.is_empty() {
            frame.render_widget(Line::styled("identical", theme::dim()), body);
        } else if shown.is_empty() {
            let notice = format!("no change matches \"{}\"", self.filter.value());
            frame.render_widget(Line::styled(clean(&notice), theme::dim()), body);
        } else {
            match self.mode {
                Mode::Unified => self.render_unified(frame, body, &shown),
                Mode::Split => self.render_split(frame, body, &shown),
            }
        }
    }

    fn render_unified(&self, frame: &mut Frame, area: Rect, shown: &[&Change]) {
        let mut lines = Vec::new();
        // The lines of the selected change: first line and how many.
        let mut anchor = (0, 0);
        for (index, change) in shown.iter().enumerate() {
            let first = lines.len();
            let style = kind_style(change.kind);
            let marker = self.marker(index);
            if self.collapsed {
                lines.push(Line::styled(format!("{marker}{}", one_line(change)), style));
            } else {
                let note = match change.kind {
                    Kind::TypeChanged => " (type changed)",
                    Kind::Added | Kind::Removed | Kind::Changed => "",
                };
                lines.push(Line::styled(
                    format!(
                        "{marker}{} {}{note}",
                        glyph(change.kind),
                        clean(&change.path.dotted())
                    ),
                    style,
                ));
                if let Some(old) = &change.old {
                    lines.push(Line::styled(
                        format!("      - {}", value_text(old)),
                        theme::removed(),
                    ));
                }
                if let Some(new) = &change.new {
                    lines.push(Line::styled(
                        format!("      + {}", value_text(new)),
                        theme::added(),
                    ));
                }
            }
            if index == self.selected {
                anchor = (first, lines.len() - first);
            }
        }
        let top = scroll_to(anchor, area.height);
        frame.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    fn render_split(&self, frame: &mut Frame, area: Rect, shown: &[&Change]) {
        let [titles, rows] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        let columns = [
            Constraint::Percentage(40),
            Constraint::Percentage(30),
            Constraint::Percentage(30),
        ];
        let title_cells: [Rect; 3] = Layout::horizontal(columns).areas(titles);
        let row_cells: [Rect; 3] = Layout::horizontal(columns).areas(rows);
        let top = scroll_to((self.selected, 1), rows.height);

        let paths = shown.iter().enumerate().map(|(index, change)| {
            Line::styled(
                format!(
                    "{}{} {}",
                    self.marker(index),
                    glyph(change.kind),
                    clean(&change.path.dotted())
                ),
                kind_style(change.kind),
            )
        });
        let side = |pick: fn(&Change) -> Option<&serde_json::Value>, style: Style| {
            shown
                .iter()
                .map(|change| Line::styled(pick(change).map(value_text).unwrap_or_default(), style))
                .collect::<Vec<_>>()
        };
        let cells = [
            ("  change".to_owned(), paths.collect::<Vec<_>>()),
            (
                clean(&self.left),
                side(|change| change.old.as_ref(), theme::removed()),
            ),
            (
                clean(&self.right),
                side(|change| change.new.as_ref(), theme::added()),
            ),
        ];
        for (column, (title, lines)) in cells.into_iter().enumerate() {
            let (Some(&title_cell), Some(&row_cell)) =
                (title_cells.get(column), row_cells.get(column))
            else {
                continue;
            };
            // Every column after the first is set off by a rule on its left.
            let borders = if column == 0 {
                Borders::NONE
            } else {
                Borders::LEFT
            };
            frame.render_widget(
                Paragraph::new(Line::styled(title, theme::dim()))
                    .block(Block::new().borders(borders)),
                title_cell,
            );
            frame.render_widget(
                Paragraph::new(lines)
                    .scroll((top, 0))
                    .block(Block::new().borders(borders)),
                row_cell,
            );
        }
    }

    /// The changes the filter lets through, in order.
    fn shown(&self) -> Vec<&Change> {
        let wanted = self.filter.value().to_lowercase();
        self.delta
            .changes
            .iter()
            .filter(|change| change.path.dotted().to_lowercase().contains(&wanted))
            .collect()
    }

    fn clear_filter(&mut self) {
        self.filtering = false;
        self.filter.set("");
        self.selected = 0;
    }

    fn marker(&self, index: usize) -> &'static str {
        if index == self.selected { "> " } else { "  " }
    }
}

/// How far to scroll so the lines `(first, count)` end inside `height` rows.
fn scroll_to((first, count): (usize, usize), height: u16) -> u16 {
    let top = (first + count).saturating_sub(usize::from(height));
    // The block's first line wins when the block is taller than the area.
    u16::try_from(top.min(first)).unwrap_or(u16::MAX)
}

fn glyph(kind: Kind) -> char {
    match kind {
        Kind::Added => '+',
        Kind::Removed => '-',
        Kind::Changed => '~',
        Kind::TypeChanged => '!',
    }
}

fn kind_style(kind: Kind) -> Style {
    match kind {
        Kind::Added => theme::added(),
        Kind::Removed => theme::removed(),
        Kind::Changed => theme::changed(),
        Kind::TypeChanged => theme::error(),
    }
}

fn value_text(value: &serde_json::Value) -> String {
    clean(&jsonutil::compact(value))
}

/// A change on one line, worded as in the plain-text output.
fn one_line(change: &Change) -> String {
    let path = clean(&change.path.dotted());
    let old = change.old.as_ref().map(value_text).unwrap_or_default();
    let new = change.new.as_ref().map(value_text).unwrap_or_default();
    match change.kind {
        Kind::Added => format!("+ {path} = {new}"),
        Kind::Removed => format!("- {path} = {old}"),
        Kind::Changed => format!("~ {path}: {old} -> {new}"),
        Kind::TypeChanged => format!("! {path}: {old} -> {new} (type changed)"),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyCode;
    use ss_core::diff::{self, Opts};
    use ss_core::jsonutil;

    use super::*;
    use crate::tui::testing::{ch, key, render_with, typed};

    const LEFT: &str = r#"{
        "name": "orders",
        "limit": 100,
        "tags": ["a", "b"],
        "legacy": true,
        "rules": [{"id": "r1", "limit": 1}]
    }"#;
    const RIGHT: &str = r#"{
        "name": "orders",
        "limit": 250,
        "tags": ["a"],
        "added": "x",
        "rules": [{"id": "r1", "limit": "1"}]
    }"#;

    fn delta(left: &str, right: &str) -> Delta {
        let left = jsonutil::parse(left.as_bytes()).unwrap();
        let right = jsonutil::parse(right.as_bytes()).unwrap();
        diff::json(&left, &right, &Opts::default())
    }

    fn viewer() -> DiffViewer {
        DiffViewer::new("V1".to_owned(), "current".to_owned(), delta(LEFT, RIGHT))
    }

    fn press(viewer: &mut DiffViewer, keys: &[KeyEvent]) {
        for &k in keys {
            viewer.handle(k);
        }
    }

    fn drawn(viewer: &DiffViewer, width: u16, height: u16) -> String {
        render_with(width, height, |frame| viewer.render(frame, frame.area()))
    }

    /// The path the selection marker is on.
    fn selected(viewer: &DiffViewer) -> String {
        drawn(viewer, 80, 40)
            .lines()
            .find_map(|line| line.strip_prefix("> "))
            .map(|rest| {
                rest.split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned()
            })
            .unwrap_or_default()
    }

    #[test]
    fn the_diff_viewer_lists_changes_with_kind_markers_and_stats() {
        insta::assert_snapshot!(drawn(&viewer(), 60, 20));
    }

    #[test]
    fn s_toggles_between_unified_and_side_by_side() {
        let mut viewer = viewer();
        assert_eq!(viewer.mode(), Mode::Unified);

        assert!(viewer.handle(ch('s')));
        assert_eq!(viewer.mode(), Mode::Split);
        insta::assert_snapshot!(drawn(&viewer, 72, 10));

        viewer.handle(ch('s'));
        assert_eq!(viewer.mode(), Mode::Unified);
    }

    #[test]
    fn n_and_p_move_between_changes_and_stop_at_the_ends() {
        let mut viewer = viewer();
        assert_eq!(selected(&viewer), "added");

        press(&mut viewer, &[ch('p')]);
        assert_eq!(selected(&viewer), "added", "already on the first change");

        press(&mut viewer, &[ch('n')]);
        assert_eq!(selected(&viewer), "legacy");

        press(&mut viewer, &typed("nnnnnnnn"));
        assert_eq!(selected(&viewer), "tags[1]", "stops on the last change");

        press(&mut viewer, &[ch('p')]);
        assert_eq!(selected(&viewer), "rules[0].limit");
    }

    #[test]
    fn c_collapses_to_one_line_per_change() {
        let mut viewer = viewer();

        assert!(viewer.handle(ch('c')));

        assert_eq!(
            drawn(&viewer, 60, 8),
            "diff V1 → current   (+1 −2 ~1 !1)\n\
             > + added = \"x\"\n\
             \x20 - legacy = true\n\
             \x20 ~ limit: 100 -> 250\n\
             \x20 ! rules[0].limit: 1 -> \"1\" (type changed)\n\
             \x20 - tags[1] = \"b\"\n\
             \n\
             \n"
        );
    }

    #[test]
    fn slash_filters_changes_by_path() {
        let mut viewer = viewer();
        press(&mut viewer, &[ch('c'), ch('/')]);
        press(&mut viewer, &typed("LIMIT"));

        let text = drawn(&viewer, 60, 6);

        assert_eq!(
            text,
            "diff V1 → current   (+1 −2 ~1 !1)   2 of 5 shown\n\
             / LIMIT\n\
             > ~ limit: 100 -> 250\n\
             \x20 ! rules[0].limit: 1 -> \"1\" (type changed)\n\
             \n\
             \n"
        );
    }

    #[test]
    fn letters_typed_into_the_filter_are_not_commands() {
        let mut viewer = viewer();
        press(&mut viewer, &[ch('/')]);

        press(&mut viewer, &typed("sc"));

        assert_eq!(viewer.mode(), Mode::Unified, "s did not switch the layout");
        assert!(drawn(&viewer, 60, 6).contains("/ sc"));
    }

    #[test]
    fn enter_keeps_the_filter_and_returns_to_the_keys() {
        let mut viewer = viewer();
        press(&mut viewer, &[ch('c'), ch('/')]);
        press(&mut viewer, &typed("limit"));

        press(&mut viewer, &[key(KeyCode::Enter), ch('n')]);

        assert_eq!(selected(&viewer), "rules[0].limit:");
        assert!(drawn(&viewer, 60, 6).contains("2 of 5 shown"));
    }

    #[test]
    fn esc_clears_the_filter_before_it_is_passed_on() {
        let mut viewer = viewer();
        press(&mut viewer, &[ch('/')]);
        press(&mut viewer, &typed("limit"));

        assert!(viewer.handle(key(KeyCode::Esc)), "clears the filter");
        assert!(!drawn(&viewer, 60, 6).contains("shown"));

        assert!(!viewer.handle(key(KeyCode::Esc)), "nothing left to clear");
    }

    #[test]
    fn a_filter_with_no_match_says_so() {
        let mut viewer = viewer();
        press(&mut viewer, &[ch('/')]);
        press(&mut viewer, &typed("zzz"));

        let text = drawn(&viewer, 60, 6);

        assert!(text.contains("no change matches"), "{text}");
    }

    #[test]
    fn g_jumps_to_the_top_and_page_keys_scroll_a_page() {
        let left: String = format!(
            "{{{}}}",
            (0..40)
                .map(|i| format!("\"k{i:02}\": {i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut viewer = DiffViewer::new("a".to_owned(), "b".to_owned(), delta(&left, "{}"));
        viewer.handle(ch('c'));

        viewer.handle(key(KeyCode::PageDown));
        assert_eq!(selected(&viewer), "k10");

        viewer.handle(key(KeyCode::PageDown));
        viewer.handle(key(KeyCode::PageUp));
        assert_eq!(selected(&viewer), "k10");

        viewer.handle(ch('g'));
        assert_eq!(selected(&viewer), "k00");

        viewer.handle(key(KeyCode::PageUp));
        assert_eq!(selected(&viewer), "k00", "stops at the top");
    }

    #[test]
    fn the_selected_change_stays_in_view_in_a_long_diff() {
        let left: String = format!(
            "{{{}}}",
            (0..40)
                .map(|i| format!("\"k{i:02}\": {i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut viewer = DiffViewer::new("a".to_owned(), "b".to_owned(), delta(&left, "{}"));
        for _ in 0..3 {
            viewer.handle(key(KeyCode::PageDown));
        }

        let text = drawn(&viewer, 40, 8);

        assert!(text.contains("> - k30"), "{text}");
    }

    #[test]
    fn an_empty_delta_shows_identical() {
        let viewer = DiffViewer::new("a".to_owned(), "b".to_owned(), delta("{}", "{}"));

        let text = drawn(&viewer, 40, 4);

        assert!(text.contains("identical"), "{text}");
    }

    #[test]
    fn keys_the_viewer_does_not_use_are_passed_on() {
        let mut viewer = viewer();

        assert!(!viewer.handle(ch('e')));
        assert!(!viewer.handle(key(KeyCode::Tab)));
        assert!(!viewer.handle(key(KeyCode::Esc)));
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let mut viewer = viewer();
        viewer.handle(ch('/'));
        viewer.handle(ch('k'));
        viewer.handle(key(KeyCode::Enter));
        for mode in 0..2 {
            render_with(2, 1, |frame| viewer.render(frame, frame.area()));
            render_with(2, 1, |frame| viewer.render(frame, Rect::new(0, 1, 2, 0)));
            let _ = mode;
            viewer.handle(ch('s'));
        }
    }

    #[test]
    fn a_paste_goes_into_the_filter_only_while_it_is_open() {
        let mut viewer = viewer();
        viewer.paste("limit\n");
        assert!(
            !drawn(&viewer, 60, 6).contains("shown"),
            "no filter is open"
        );

        press(&mut viewer, &[ch('c'), ch('/')]);
        viewer.paste("limit\n");

        let text = drawn(&viewer, 60, 6);
        assert!(text.contains("/ limit"), "{text}");
        assert!(text.contains("2 of 5 shown"), "{text}");
    }
}
