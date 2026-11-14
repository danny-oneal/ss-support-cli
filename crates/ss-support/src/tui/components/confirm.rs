//! The confirmation asked before an operation's first mutating step. The
//! answer is always typed and ended with enter: `y` for a mutating
//! operation, the resource name for a destructive one. A single stray key
//! can never proceed.

use std::cell::Cell;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ss_core::guard::Class;
use ss_core::ops::ConfirmSpec;
use ss_core::render;

use crate::tui::screen::ConfirmRequest;
use crate::tui::text::{clean, wrap_columns};
use crate::tui::theme;
use crate::tui::widgets::text_input::TextInput;

/// What the typed answer amounts to when enter is pressed.
enum Verdict {
    Proceed,
    Decline,
    /// Nothing typed yet; the question stays open without comment.
    Unanswered,
    /// Typed, but not an answer; the question stays open and says so.
    Unclear,
}

/// One open confirmation question.
pub struct ConfirmModal {
    spec: ConfirmSpec,
    /// The question, until it has been answered.
    request: Option<ConfirmRequest>,
    /// What the operator has typed as the answer.
    typed: TextInput,
    /// The last attempt was not an answer.
    unclear: bool,
    /// The first preview row shown.
    top: usize,
    /// The furthest the preview could scroll when it was last drawn.
    last_top: Cell<usize>,
    /// How many preview rows were last drawn; a page scrolls by this.
    page: Cell<usize>,
}

impl ConfirmModal {
    /// A modal asking `request`.
    pub fn new(request: ConfirmRequest) -> Self {
        ConfirmModal {
            spec: request.spec.clone(),
            request: Some(request),
            typed: TextInput::default(),
            unclear: false,
            top: 0,
            last_top: Cell::new(0),
            page: Cell::new(1),
        }
    }

    /// Applies a key. Returns true once the question has been answered and
    /// the modal should close.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let page = self.page.get().max(1);
        let answer = match key.code {
            KeyCode::Esc => Some(false),
            KeyCode::Enter => match self.verdict() {
                Verdict::Proceed => Some(true),
                Verdict::Decline => Some(false),
                Verdict::Unanswered => None,
                Verdict::Unclear => {
                    self.unclear = true;
                    None
                }
            },
            KeyCode::Up => self.scroll_to(self.top.saturating_sub(1)),
            KeyCode::Down => self.scroll_to(self.top.saturating_add(1)),
            KeyCode::PageUp => self.scroll_to(self.top.saturating_sub(page)),
            KeyCode::PageDown => self.scroll_to(self.top.saturating_add(page)),
            _ => {
                if self.typed.handle(key) {
                    self.unclear = false;
                }
                None
            }
        };
        let Some(proceed) = answer else {
            return false;
        };
        if let Some(request) = self.request.take() {
            request.answer(proceed);
        }
        true
    }

    /// Answers no.
    pub fn decline(mut self) {
        if let Some(request) = self.request.take() {
            request.answer(false);
        }
    }

    /// Draws the question over `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(" confirm ")
            .border_style(theme::changed());
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        let width = usize::from(inner.width);

        // The question is never cut: it wraps, and the preview gives way.
        let question: Vec<String> = self
            .question()
            .iter()
            .flat_map(|line| wrap_columns(&clean(line), width))
            .collect();
        let prompt_rows = u16::try_from(question.len() + 2).unwrap_or(u16::MAX);
        let [heading, preview, prompt] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(prompt_rows.min(inner.height)),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    clean(&format!("{} ({})", self.spec.op_id, self.spec.class)),
                    theme::heading(),
                ),
                Line::from(clean(&format!("Step \"{}\" will change:", self.spec.step))),
            ]),
            heading,
        );
        self.render_preview(frame, preview, width);
        self.render_prompt(frame, prompt, question);
    }

    /// Draws the preview, wrapped to `width`, from the row scrolled to. When
    /// it does not fit, the last row says which rows are shown.
    fn render_preview(&self, frame: &mut Frame, area: Rect, width: usize) {
        let rows: Vec<String> = clean(&render::text(&self.spec.preview))
            .lines()
            .flat_map(|line| wrap_columns(line, width))
            .collect();
        let fits = rows.len() <= usize::from(area.height);
        let [body, marker] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(u16::from(!fits))])
                .areas(area);
        let visible = usize::from(body.height);
        let last_top = rows.len().saturating_sub(visible);
        let top = self.top.min(last_top);
        self.last_top.set(last_top);
        self.page.set(visible);

        let shown: Vec<Line> = rows
            .iter()
            .skip(top)
            .take(visible)
            .map(|row| Line::from(row.as_str()))
            .collect();
        frame.render_widget(Paragraph::new(shown), body);
        if !fits {
            let through = (top + visible).min(rows.len());
            frame.render_widget(
                Line::styled(
                    format!(
                        "lines {}–{through} of {} · ↑↓ pgup pgdn scroll",
                        top + 1,
                        rows.len()
                    ),
                    theme::changed(),
                ),
                marker,
            );
        }
    }

    fn render_prompt(&self, frame: &mut Frame, area: Rect, question: Vec<String>) {
        let question_rows = u16::try_from(question.len()).unwrap_or(u16::MAX);
        let [asked, entry, note] = Layout::vertical([
            Constraint::Length(question_rows),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let style = if self.destructive() {
            theme::error()
        } else {
            theme::heading()
        };
        frame.render_widget(
            Paragraph::new(
                question
                    .into_iter()
                    .map(|row| Line::styled(row, style))
                    .collect::<Vec<_>>(),
            ),
            asked,
        );
        let [marker, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(entry);
        frame.render_widget(Line::from("> "), marker);
        self.typed.render(frame, field, true);
        let note_line = match (self.unclear, self.destructive()) {
            (true, true) => Line::styled("name does not match", theme::error()),
            (true, false) => Line::styled("type y or n", theme::error()),
            (false, _) => Line::styled("enter answer · esc decline", theme::hints()),
        };
        frame.render_widget(note_line, note);
    }

    /// The lines of the question. A destructive operation shows the name to
    /// type on a line of its own, exactly as it must be typed.
    fn question(&self) -> Vec<String> {
        if self.destructive() {
            vec![
                "This is DESTRUCTIVE. Type this name, then enter:".to_owned(),
                self.spec.resource.clone(),
            ]
        } else {
            vec![format!(
                "Proceed with {}? Type y or n, then enter:",
                self.spec.op_id
            )]
        }
    }

    /// What pressing enter means for the text typed so far.
    fn verdict(&self) -> Verdict {
        let typed = self.typed.value();
        if self.destructive() {
            // The name must match exactly, and an empty name matches nothing.
            return if !self.spec.resource.is_empty() && typed == self.spec.resource {
                Verdict::Proceed
            } else {
                Verdict::Unclear
            };
        }
        match typed.trim().to_lowercase().as_str() {
            "" => Verdict::Unanswered,
            "y" | "yes" => Verdict::Proceed,
            "n" | "no" => Verdict::Decline,
            _ => Verdict::Unclear,
        }
    }

    /// Moves the preview to `top`, within what was last drawn. Never an
    /// answer.
    fn scroll_to(&mut self, top: usize) -> Option<bool> {
        self.top = top.min(self.last_top.get());
        None
    }

    fn destructive(&self) -> bool {
        self.spec.class == Class::Destructive
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyCode;
    use ss_core::guard::Class;
    use ss_core::ops::{ConfirmSpec, View};
    use tokio::sync::oneshot::{self, error::TryRecvError};

    use super::*;
    use crate::tui::testing::{ch, key, render_with, typed};

    fn spec(class: Class) -> ConfirmSpec {
        ConfirmSpec {
            op_id: "s3.restore-version".to_owned(),
            class,
            resource: "orders-dev".to_owned(),
            step: "copy the version".to_owned(),
            preview: vec![View::summary([
                ("Bucket", "config-dev"),
                ("Key", "orders-dev"),
            ])],
        }
    }

    fn modal(class: Class) -> (ConfirmModal, oneshot::Receiver<bool>) {
        let (request, answer) = ConfirmRequest::new(spec(class));
        (ConfirmModal::new(request), answer)
    }

    fn press(modal: &mut ConfirmModal, keys: &[KeyEvent]) -> bool {
        keys.iter().any(|&k| modal.handle(k))
    }

    fn drawn(modal: &ConfirmModal) -> String {
        render_with(60, 14, |frame| modal.render(frame, frame.area()))
    }

    fn enter() -> KeyEvent {
        key(KeyCode::Enter)
    }

    #[test]
    fn a_mutating_confirmation_needs_y_and_then_enter() {
        let (mut modal, mut answer) = modal(Class::Mutate);

        assert!(!modal.handle(ch('y')), "y alone does not answer");
        assert_eq!(answer.try_recv(), Err(TryRecvError::Empty));

        assert!(modal.handle(enter()));
        assert_eq!(answer.try_recv(), Ok(true));
    }

    #[test]
    fn yes_in_any_case_proceeds() {
        let (mut modal, mut answer) = modal(Class::Mutate);
        let mut keys = typed(" YES ");
        keys.push(enter());

        assert!(press(&mut modal, &keys));

        assert_eq!(answer.try_recv(), Ok(true));
    }

    #[test]
    fn n_and_enter_or_esc_declines_a_mutating_confirmation() {
        let (mut no, mut answer) = modal(Class::Mutate);
        assert!(!no.handle(ch('n')), "n alone does not answer");
        assert!(no.handle(enter()));
        assert_eq!(answer.try_recv(), Ok(false));

        let (mut esc, mut answer) = modal(Class::Mutate);
        assert!(esc.handle(key(KeyCode::Esc)));
        assert_eq!(answer.try_recv(), Ok(false));
    }

    #[test]
    fn enter_alone_leaves_a_mutating_confirmation_open() {
        let (mut modal, mut answer) = modal(Class::Mutate);

        assert!(!press(&mut modal, &[enter(), enter(), key(KeyCode::Tab)]));

        assert_eq!(answer.try_recv(), Err(TryRecvError::Empty));
        assert!(!drawn(&modal).contains("type y or n"));
    }

    #[test]
    fn text_that_merely_contains_a_y_is_not_a_yes() {
        let (mut modal, mut answer) = modal(Class::Mutate);
        let mut keys = typed("my-key");
        keys.push(enter());

        assert!(!press(&mut modal, &keys));

        assert_eq!(answer.try_recv(), Err(TryRecvError::Empty));
        assert!(drawn(&modal).contains("type y or n"), "{}", drawn(&modal));
    }

    fn long_preview(class: Class) -> (ConfirmModal, oneshot::Receiver<bool>) {
        let body: String = (1..=40).map(|i| format!("change {i:02}\n")).collect();
        let (request, answer) = ConfirmRequest::new(ConfirmSpec {
            preview: vec![View::Doc {
                title: String::new(),
                body,
                lang: String::new(),
            }],
            ..spec(class)
        });
        (ConfirmModal::new(request), answer)
    }

    #[test]
    fn a_preview_longer_than_the_modal_says_how_much_is_shown() {
        let (modal, _answer) = long_preview(Class::Mutate);

        let text = drawn(&modal);

        assert!(text.contains("change 01"), "{text}");
        assert!(!text.contains("change 40"), "{text}");
        assert!(text.contains("lines 1–6 of 40"), "{text}");
    }

    #[test]
    fn the_preview_scrolls_to_its_last_line_and_stops() {
        let (mut modal, mut answer) = long_preview(Class::Destructive);
        drawn(&modal);

        modal.handle(key(KeyCode::Down));
        assert!(drawn(&modal).contains("lines 2–"), "{}", drawn(&modal));

        for _ in 0..20 {
            modal.handle(key(KeyCode::PageDown));
            drawn(&modal);
        }
        let text = drawn(&modal);
        assert!(text.contains("change 40"), "{text}");
        assert!(text.contains("of 40"), "{text}");

        for _ in 0..20 {
            modal.handle(key(KeyCode::PageUp));
            drawn(&modal);
        }
        assert!(drawn(&modal).contains("lines 1–"));
        assert_eq!(
            answer.try_recv(),
            Err(TryRecvError::Empty),
            "scrolling answers nothing"
        );
    }

    #[test]
    fn a_preview_that_fits_shows_no_scroll_marker() {
        let (modal, _answer) = modal(Class::Mutate);

        assert!(!drawn(&modal).contains("lines 1–"));
    }

    #[test]
    fn a_preview_line_wider_than_the_modal_is_wrapped_not_cut() {
        let wide = format!("{}END", "x".repeat(70));
        let (request, _answer) = ConfirmRequest::new(ConfirmSpec {
            preview: vec![View::Doc {
                title: String::new(),
                body: wide,
                lang: String::new(),
            }],
            ..spec(Class::Mutate)
        });
        let modal = ConfirmModal::new(request);

        let text = drawn(&modal);

        assert!(text.contains("END"), "{text}");
    }

    #[test]
    fn a_resource_name_wider_than_the_modal_is_shown_in_full() {
        let name = format!("orders-{}-dev", "long".repeat(15));
        let (request, mut answer) = ConfirmRequest::new(ConfirmSpec {
            resource: name.clone(),
            ..spec(Class::Destructive)
        });
        let mut modal = ConfirmModal::new(request);

        let text = drawn(&modal);
        let joined: String = text
            .lines()
            .map(|line| line.trim_matches(|c| c == '│' || c == ' '))
            .collect();
        assert!(joined.contains(&name), "{text}");

        let mut keys = typed(&name);
        keys.push(enter());
        assert!(press(&mut modal, &keys));
        assert_eq!(answer.try_recv(), Ok(true));
    }

    #[test]
    fn a_destructive_confirmation_requires_the_exact_resource_name() {
        let (mut modal, mut answer) = modal(Class::Destructive);

        let mut keys = typed("orders");
        keys.push(key(KeyCode::Enter));
        assert!(!press(&mut modal, &keys));
        assert_eq!(answer.try_recv(), Err(TryRecvError::Empty));
        assert!(drawn(&modal).contains("name does not match"));

        let mut keys = typed("-dev");
        keys.push(key(KeyCode::Enter));
        assert!(press(&mut modal, &keys));
        assert_eq!(answer.try_recv(), Ok(true));
    }

    #[test]
    fn typing_again_clears_the_mismatch_notice() {
        let (mut modal, _answer) = modal(Class::Destructive);
        press(&mut modal, &[ch('x'), key(KeyCode::Enter)]);
        assert!(drawn(&modal).contains("name does not match"));

        modal.handle(key(KeyCode::Backspace));

        assert!(!drawn(&modal).contains("name does not match"));
    }

    #[test]
    fn y_does_not_confirm_a_destructive_operation() {
        let (mut modal, mut answer) = modal(Class::Destructive);

        assert!(!press(&mut modal, &[ch('y'), key(KeyCode::Enter)]));

        assert_eq!(answer.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn enter_on_an_empty_name_does_not_confirm() {
        let (request, mut answer) = ConfirmRequest::new(ConfirmSpec {
            resource: String::new(),
            ..spec(Class::Destructive)
        });
        let mut modal = ConfirmModal::new(request);

        assert!(!modal.handle(key(KeyCode::Enter)));

        assert_eq!(answer.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn esc_declines_a_destructive_confirmation() {
        let (mut modal, mut answer) = modal(Class::Destructive);

        assert!(press(&mut modal, &[ch('o'), key(KeyCode::Esc)]));

        assert_eq!(answer.try_recv(), Ok(false));
    }

    #[test]
    fn declining_answers_no() {
        let (modal, mut answer) = modal(Class::Destructive);

        modal.decline();

        assert_eq!(answer.try_recv(), Ok(false));
    }

    #[test]
    fn dropping_the_modal_unanswered_closes_the_question() {
        let (modal, mut answer) = modal(Class::Mutate);

        drop(modal);

        assert_eq!(answer.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn the_modal_shows_the_preview_and_the_step_about_to_run() {
        let (modal, _answer) = modal(Class::Mutate);

        insta::assert_snapshot!(drawn(&modal));
    }

    #[test]
    fn a_destructive_modal_asks_for_the_resource_name() {
        let (mut modal, _answer) = modal(Class::Destructive);
        press(&mut modal, &typed("orders"));

        insta::assert_snapshot!(drawn(&modal));
    }

    #[test]
    fn tabs_in_the_preview_keep_their_columns() {
        let (request, _answer) = ConfirmRequest::new(ConfirmSpec {
            preview: vec![View::Doc {
                title: String::new(),
                body: "key\tvalue\x1b[2J".to_owned(),
                lang: String::new(),
            }],
            ..spec(Class::Mutate)
        });
        let modal = ConfirmModal::new(request);

        let text = drawn(&modal);

        assert!(text.contains("key     value[2J"), "{text}");
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let (modal, _answer) = modal(Class::Destructive);

        render_with(3, 2, |frame| modal.render(frame, frame.area()));
        render_with(3, 2, |frame| modal.render(frame, Rect::new(0, 2, 3, 0)));
    }
}
