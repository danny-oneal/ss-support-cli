//! The first screen: every registered operation, grouped by category, with
//! a fuzzy filter.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ss_core::fuzzy;
use ss_core::ops::{Meta, Operation, Registry};
use unicode_width::UnicodeWidthStr;

use crate::tui::screen::{Cmd, Ctx, Msg, Screen};
use crate::tui::screens::k8spods::K8sPods;
use crate::tui::screens::opsform::OpsForm;
use crate::tui::screens::s3browse::S3Browse;
use crate::tui::text::clean;
use crate::tui::theme;
use crate::tui::widgets::text_input::TextInput;

/// The operation picker.
pub struct OpsPicker {
    /// Every operation, by category and then id.
    ops: Vec<Arc<dyn Operation>>,
    filter: TextInput,
    /// Keys are going to the filter.
    filtering: bool,
    /// Index of the selected operation among the shown ones.
    selected: usize,
}

impl OpsPicker {
    /// A picker over every operation in `registry`.
    pub fn new(registry: &Registry) -> Self {
        OpsPicker {
            ops: registry.all(),
            filter: TextInput::default(),
            filtering: false,
            selected: 0,
        }
    }

    /// The operations to list: all of them in order, or, with a filter,
    /// the ones it matches, best match first.
    fn shown(&self) -> Vec<&Arc<dyn Operation>> {
        let pattern = self.filter.value();
        if pattern.is_empty() {
            return self.ops.iter().collect();
        }
        let texts: Vec<String> = self
            .ops
            .iter()
            .map(|op| {
                let meta = op.meta();
                format!("{} {} {}", meta.id, meta.name, meta.summary)
            })
            .collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        fuzzy::rank(pattern, &texts)
            .into_iter()
            .filter_map(|(index, _)| self.ops.get(index))
            .collect()
    }

    fn step(&mut self, forward: bool) {
        let count = self.shown().len();
        if count == 0 {
            return;
        }
        let by = if forward { 1 } else { count - 1 };
        self.selected = (self.selected + by) % count;
    }

    fn open(&self, ctx: &Ctx) -> Vec<Cmd> {
        match self.shown().get(self.selected) {
            Some(&op) => vec![Cmd::Push(Box::new(OpsForm::new(Arc::clone(op), ctx)))],
            None => Vec::new(),
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Vec<Cmd> {
        match key.code {
            KeyCode::Up => self.step(false),
            KeyCode::Down => self.step(true),
            KeyCode::Enter => return self.open(ctx),
            KeyCode::Esc if self.filtering => {
                self.filtering = false;
                self.filter.set("");
                self.selected = 0;
            }
            // Esc means "back" everywhere else; here there is nowhere to go
            // back to, and it must not end the program.
            KeyCode::Esc => {}
            _ if self.filtering => {
                if self.filter.handle(key) {
                    self.selected = 0;
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match c {
                    '/' => self.filtering = true,
                    'j' => self.step(true),
                    'k' => self.step(false),
                    'b' => return vec![Cmd::Push(Box::new(S3Browse::new(ctx)))],
                    'p' => return vec![Cmd::Push(Box::new(K8sPods::new(ctx)))],
                    'q' => return vec![Cmd::Quit],
                    _ => {}
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn render_list(&self, frame: &mut Frame, area: Rect) {
        let shown = self.shown();
        if self.ops.is_empty() {
            frame.render_widget(Line::styled("no operations registered", theme::dim()), area);
            return;
        }
        if shown.is_empty() {
            let notice = format!("no operation matches \"{}\"", self.filter.value());
            frame.render_widget(Line::styled(clean(&notice), theme::dim()), area);
            return;
        }
        let metas: Vec<Meta> = shown.iter().map(|op| op.meta()).collect();
        let id_cols = metas.iter().map(|meta| meta.id.width()).max().unwrap_or(0);
        let name_cols = metas
            .iter()
            .map(|meta| clean(meta.name).width())
            .max()
            .unwrap_or(0);
        let grouped = self.filter.value().is_empty();

        let mut lines = Vec::new();
        let mut selected_line = 0;
        let mut category = "";
        for (index, meta) in metas.iter().enumerate() {
            if grouped && meta.category != category {
                category = meta.category;
                lines.push(Line::styled(clean(category), theme::heading()));
            }
            let name = clean(meta.name);
            let row = format!(
                "{id}{id_pad}  {name}{name_pad}  {class:<11}  {summary}",
                id = meta.id,
                id_pad = " ".repeat(id_cols.saturating_sub(meta.id.width())),
                name_pad = " ".repeat(name_cols.saturating_sub(name.width())),
                class = meta.class.as_str(),
                summary = clean(meta.summary),
            );
            if index == self.selected {
                selected_line = lines.len();
                lines.push(Line::styled(format!("> {row}"), theme::selected()));
            } else {
                lines.push(Line::from(format!("  {row}")));
            }
        }
        let top = (selected_line + 1).saturating_sub(usize::from(area.height));
        frame.render_widget(
            Paragraph::new(lines).scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
            area,
        );
    }
}

impl Screen for OpsPicker {
    fn title(&self) -> String {
        "Operations".to_owned()
    }

    fn hints(&self) -> String {
        if self.filtering {
            "type to filter · ↑↓ move · enter open · esc clear"
        } else {
            "↑↓ move · enter open · / filter · b browse S3 · p pods · q quit"
        }
        .to_owned()
    }

    fn init(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        Vec::new()
    }

    fn update(&mut self, msg: Msg, ctx: &Ctx) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) => self.on_key(key, ctx),
            Msg::Paste(text) => {
                // Text for an open filter, and nothing otherwise: pasted
                // letters are never commands.
                if self.filtering {
                    self.filter.paste(&text);
                    self.selected = 0;
                }
                Vec::new()
            }
            Msg::Resize
            | Msg::Progress(_)
            | Msg::Warning(_)
            | Msg::Confirm(_)
            | Msg::RunDone(_)
            | Msg::Loaded(..)
            | Msg::Lines(..)
            | Msg::StreamEnded(..) => Vec::new(),
        }
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        let [filter, list] = Layout::vertical([
            Constraint::Length(u16::from(self.filtering)),
            Constraint::Min(0),
        ])
        .areas(area);
        if self.filtering {
            let [marker, field] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(filter);
            frame.render_widget(Line::from("/ "), marker);
            self.filter.render(frame, field, true);
        }
        self.render_list(frame, list);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ss_core::guard::Class;
    use ss_core::ops;

    use super::*;
    use crate::tui::stub_ops::{Behavior, StubOp};
    use crate::tui::testing::{ch, ctx, key, render_screen, typed};

    fn picker() -> OpsPicker {
        OpsPicker::new(&ops::builtin())
    }

    fn send(picker: &mut OpsPicker, keys: &[KeyEvent]) -> Vec<Cmd> {
        let ctx = ctx();
        let mut last = Vec::new();
        for &k in keys {
            last = picker.update(Msg::Key(k), &ctx);
        }
        last
    }

    /// The id of the operation the selection is on.
    fn selected(picker: &OpsPicker) -> String {
        render_screen(picker, 100, 30)
            .lines()
            .find_map(|line| line.strip_prefix("> "))
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_default()
            .to_owned()
    }

    fn pushed_title(mut cmds: Vec<Cmd>) -> String {
        match cmds.pop() {
            Some(Cmd::Push(screen)) => screen.title(),
            _ => panic!("expected a screen to be pushed"),
        }
    }

    #[test]
    fn operations_are_grouped_by_category_with_a_class_badge() {
        let mut registry = ops::builtin();
        let (write, _done) = StubOp::write("zz.drop-table", Class::Destructive, "orders-dev");
        registry
            .register(Arc::new(write.described(
                "Drop a table",
                "removes a table and its items",
                "Zeta",
            )))
            .unwrap();

        insta::assert_snapshot!(render_screen(&OpsPicker::new(&registry), 100, 24));
    }

    #[test]
    fn the_first_operation_is_selected_to_begin_with() {
        let first = ops::builtin().all().first().map(|op| op.meta().id).unwrap();

        assert_eq!(selected(&picker()), first);
    }

    #[test]
    fn slash_starts_the_filter_and_typing_ranks_by_fuzzy_score() {
        let mut picker = picker();

        send(&mut picker, &[ch('/')]);
        send(&mut picker, &typed("vdiff"));

        assert_eq!(selected(&picker), "s3.version-diff");
        let shown = render_screen(&picker, 100, 30);
        assert!(shown.starts_with("/ vdiff\n"), "{shown}");
        assert!(!shown.contains("demo.hello"), "{shown}");
    }

    #[test]
    fn letters_typed_while_filtering_are_text_not_commands() {
        let mut picker = picker();
        send(&mut picker, &[ch('/')]);

        let cmds = send(&mut picker, &typed("bqp"));

        assert!(cmds.is_empty(), "b, q, and p neither browse nor quit");
        assert!(render_screen(&picker, 100, 30).starts_with("/ bqp\n"));
    }

    #[test]
    fn arrows_move_and_enter_opens_while_filtering() {
        let mut picker = picker();
        send(&mut picker, &[ch('/')]);
        send(&mut picker, &typed("ddb"));
        let first = selected(&picker);

        send(&mut picker, &[key(KeyCode::Down)]);
        let second = selected(&picker);
        assert_ne!(first, second);
        assert!(second.starts_with("ddb."), "{second}");

        let cmds = send(&mut picker, &[key(KeyCode::Enter)]);
        let opened = ops::builtin().get(&second).unwrap().meta().name;
        assert_eq!(pushed_title(cmds), opened);
    }

    #[test]
    fn esc_clears_the_filter_before_it_quits() {
        let mut picker = picker();
        send(&mut picker, &[ch('/')]);
        send(&mut picker, &typed("vdiff"));

        let cmds = send(&mut picker, &[key(KeyCode::Esc)]);
        assert!(cmds.is_empty());
        assert!(render_screen(&picker, 100, 30).contains("demo.hello"));

        let cmds = send(&mut picker, &[key(KeyCode::Esc)]);
        assert!(cmds.is_empty(), "esc never ends the program");
    }

    #[test]
    fn up_and_down_wrap_and_skip_category_headers() {
        let mut picker = picker();
        let ids: Vec<&str> = ops::builtin().all().iter().map(|op| op.meta().id).collect();

        let mut walked = vec![selected(&picker)];
        for _ in 1..ids.len() {
            send(&mut picker, &[key(KeyCode::Down)]);
            walked.push(selected(&picker));
        }
        assert_eq!(walked, ids, "down visits every operation and no heading");

        send(&mut picker, &[key(KeyCode::Down)]);
        assert_eq!(selected(&picker), ids[0], "wraps past the last operation");

        send(&mut picker, &[key(KeyCode::Up)]);
        assert_eq!(
            selected(&picker),
            ids[ids.len() - 1],
            "wraps before the first"
        );
    }

    #[test]
    fn a_filter_with_no_match_shows_no_operations_and_enter_does_nothing() {
        let mut picker = picker();
        send(&mut picker, &[ch('/')]);
        send(&mut picker, &typed("zzzz"));

        let cmds = send(&mut picker, &[key(KeyCode::Enter)]);

        assert!(cmds.is_empty());
        let shown = render_screen(&picker, 100, 30);
        assert!(shown.contains("no operation matches \"zzzz\""), "{shown}");
    }

    #[test]
    fn enter_pushes_the_form_for_the_selected_operation() {
        let mut picker = picker();
        send(&mut picker, &[key(KeyCode::Down)]);
        let id = selected(&picker);

        let cmds = send(&mut picker, &[key(KeyCode::Enter)]);

        assert_eq!(
            pushed_title(cmds),
            ops::builtin().get(&id).unwrap().meta().name
        );
    }

    #[test]
    fn q_quits() {
        assert!(matches!(
            send(&mut picker(), &[ch('q')]).as_slice(),
            [Cmd::Quit]
        ));
    }

    #[test]
    fn b_opens_the_s3_browser() {
        let cmds = send(&mut picker(), &[ch('b')]);

        assert!(pushed_title(cmds).starts_with("S3"));
    }

    #[test]
    fn p_opens_the_pod_list() {
        let cmds = send(&mut picker(), &[ch('p')]);

        assert!(pushed_title(cmds).starts_with("Pods"));
    }

    #[test]
    fn an_empty_registry_shows_no_operations_registered() {
        let mut picker = OpsPicker::new(&Registry::new());

        assert!(render_screen(&picker, 60, 6).contains("no operations registered"));
        assert!(send(&mut picker, &[key(KeyCode::Enter), key(KeyCode::Down)]).is_empty());
    }

    #[test]
    fn the_selection_stays_in_view_on_a_short_screen() {
        let mut picker = picker();
        let last = ops::builtin().all().last().map(|op| op.meta().id).unwrap();

        send(&mut picker, &[key(KeyCode::Up)]);

        let shown = render_screen(&picker, 100, 4);
        assert!(shown.contains(&format!("> {last}")), "{shown}");
    }

    #[test]
    fn names_with_control_characters_are_listed_safely() {
        let mut registry = Registry::new();
        registry
            .register(Arc::new(
                StubOp::new("stub.odd", Class::Read, Behavior::Echo).described(
                    "Odd\x1b[2Jname",
                    "tab\there",
                    "Stub",
                ),
            ))
            .unwrap();

        let shown = render_screen(&OpsPicker::new(&registry), 80, 6);

        assert!(shown.contains("Odd[2Jname"), "{shown}");
        assert!(!shown.contains('\x1b'), "{shown:?}");
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let mut picker = picker();
        render_screen(&picker, 1, 1);
        render_screen(&picker, 0, 0);
        send(&mut picker, &[ch('/')]);
        render_screen(&picker, 1, 1);
    }

    #[test]
    fn a_paste_is_filter_text_and_never_a_command() {
        let ctx = ctx();
        let mut picker = picker();

        let cmds = picker.update(Msg::Paste("qb\n".to_owned()), &ctx);
        assert!(cmds.is_empty(), "pasted q, b, and enter do nothing");

        send(&mut picker, &[ch('/')]);
        let cmds = picker.update(Msg::Paste("vdiff\n".to_owned()), &ctx);

        assert!(cmds.is_empty(), "the pasted line break opens nothing");
        assert_eq!(selected(&picker), "s3.version-diff");
    }

    #[test]
    fn esc_without_a_filter_does_nothing() {
        let mut picker = picker();

        assert!(send(&mut picker, &[key(KeyCode::Esc)]).is_empty());
    }
}
