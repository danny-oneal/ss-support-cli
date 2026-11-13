//! The confirmation asked before an operation's first mutating step: `y` or
//! `n` for a mutating operation, the typed resource name for a destructive
//! one.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ss_core::guard::Class;
use ss_core::ops::ConfirmSpec;
use ss_core::render;

use crate::tui::screen::ConfirmRequest;
use crate::tui::text::clean;
use crate::tui::theme;
use crate::tui::widgets::text_input::TextInput;

/// One open confirmation question.
pub struct ConfirmModal {
    spec: ConfirmSpec,
    /// The question, until it has been answered.
    request: Option<ConfirmRequest>,
    /// What the operator has typed for a destructive operation.
    typed: TextInput,
    /// The last attempt did not match the resource name.
    mismatch: bool,
}

impl ConfirmModal {
    /// A modal asking `request`.
    pub fn new(request: ConfirmRequest) -> Self {
        ConfirmModal {
            spec: request.spec.clone(),
            request: Some(request),
            typed: TextInput::default(),
            mismatch: false,
        }
    }

    /// Applies a key. Returns true once the question has been answered and
    /// the modal should close.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let destructive = self.destructive();
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let answer = match key.code {
            KeyCode::Esc => Some(false),
            KeyCode::Enter if destructive => {
                let matches =
                    !self.spec.resource.is_empty() && self.typed.value() == self.spec.resource;
                self.mismatch = !matches;
                matches.then_some(true)
            }
            KeyCode::Char('y' | 'Y') if !destructive && plain => Some(true),
            KeyCode::Char('n' | 'N') if !destructive && plain => Some(false),
            _ => {
                if destructive && self.typed.handle(key) {
                    self.mismatch = false;
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

        let prompt_rows = if self.destructive() { 3 } else { 2 };
        let [preview, prompt] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(prompt_rows)]).areas(inner);

        let mut lines = vec![
            Line::styled(
                format!("{} ({})", self.spec.op_id, self.spec.class),
                theme::heading(),
            ),
            Line::from(format!("Step \"{}\" will change:", self.spec.step)),
            Line::default(),
        ];
        lines.extend(
            clean(&render::text(&self.spec.preview))
                .lines()
                .map(|line| Line::from(line.to_owned())),
        );
        frame.render_widget(Paragraph::new(lines), preview);

        if self.destructive() {
            self.render_name_prompt(frame, prompt);
        } else {
            let lines = vec![
                Line::from(format!("Proceed with {}? [y/n]", self.spec.op_id)),
                Line::styled("y proceed · n or esc decline", theme::hints()),
            ];
            frame.render_widget(Paragraph::new(lines), prompt);
        }
    }

    fn render_name_prompt(&self, frame: &mut Frame, area: Rect) {
        let [question, entry, note] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        frame.render_widget(
            Line::styled(
                format!(
                    "This is DESTRUCTIVE. Type \"{}\" to confirm:",
                    self.spec.resource
                ),
                theme::error(),
            ),
            question,
        );
        let [marker, field] =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(entry);
        frame.render_widget("> ", marker);
        self.typed.render(frame, field, true);
        let note_line = if self.mismatch {
            Line::styled("name does not match", theme::error())
        } else {
            Line::styled("enter confirm · esc decline", theme::hints())
        };
        frame.render_widget(note_line, note);
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

    #[test]
    fn a_mutating_confirmation_accepts_y_and_declines_n_and_esc() {
        let (mut yes, mut answer) = modal(Class::Mutate);
        assert!(yes.handle(ch('y')));
        assert_eq!(answer.try_recv(), Ok(true));

        let (mut no, mut answer) = modal(Class::Mutate);
        assert!(no.handle(ch('n')));
        assert_eq!(answer.try_recv(), Ok(false));

        let (mut esc, mut answer) = modal(Class::Mutate);
        assert!(esc.handle(key(KeyCode::Esc)));
        assert_eq!(answer.try_recv(), Ok(false));
    }

    #[test]
    fn enter_and_other_keys_leave_a_mutating_confirmation_open() {
        let (mut modal, mut answer) = modal(Class::Mutate);

        assert!(!press(
            &mut modal,
            &[key(KeyCode::Enter), ch('x'), key(KeyCode::Tab)]
        ));

        assert_eq!(answer.try_recv(), Err(TryRecvError::Empty));
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
        render_with(3, 2, |frame| modal.render(frame, Rect::new(0, 0, 0, 0)));
    }
}
