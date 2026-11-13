//! What an operation returned: one view at a time, with export to a
//! markdown report and saving the run as a preset.

use std::cell::Cell;
use std::fs;
use std::slice;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ss_core::ops::{Input, OpResult, Operation, View, with_defaults};
use ss_core::{render, report};

use crate::tui::components::diffviewer::DiffViewer;
use crate::tui::components::prompt::{Prompt, PromptEvent};
use crate::tui::screen::{Cmd, Ctx, Msg, Screen};
use crate::tui::text::{clean, slice_columns};
use crate::tui::{presets, theme};

/// How one view is shown.
enum Pane {
    /// A semantic diff, with its own keys.
    Diff(Box<DiffViewer>),
    /// Any other view, as scrollable text.
    Text {
        lines: Vec<String>,
        /// The first line shown.
        top: usize,
        /// The first column shown.
        left: usize,
    },
}

/// The result of one run.
pub struct OpsResult {
    op: Arc<dyn Operation>,
    input: Input,
    views: Vec<View>,
    panes: Vec<Pane>,
    current: usize,
    /// The preset-name prompt, while it is open.
    saving: Option<Prompt>,
    /// How many rows the view was last drawn on; a page scrolls by this.
    rows: Cell<usize>,
}

impl OpsResult {
    /// A screen showing `result`, the outcome of running `op` with `input`.
    pub fn new(op: Arc<dyn Operation>, input: Input, result: OpResult, _ctx: &Ctx) -> Self {
        let panes = result.views.iter().map(pane).collect();
        OpsResult {
            op,
            input,
            views: result.views,
            panes,
            current: 0,
            saving: None,
            rows: Cell::new(1),
        }
    }

    fn step(&mut self, forward: bool) {
        let count = self.panes.len();
        if count == 0 {
            return;
        }
        let back = if forward { 1 } else { count - 1 };
        self.current = (self.current + back) % count;
    }

    /// Writes the markdown report and returns what to tell the operator.
    fn export(&self, ctx: &Ctx) -> String {
        let meta = self.op.meta();
        let now = (ctx.now)();
        let path = ctx.export_dir.join(format!(
            "ss-support-{}-{}.md",
            meta.id,
            now.format("%Y%m%d-%H%M%S")
        ));
        let resolved = with_defaults(self.op.as_ref(), self.input.clone());
        let report = report::markdown(&meta, &resolved, &self.views, now);
        match fs::write(&path, report) {
            Ok(()) => format!("exported to {}", path.display()),
            Err(e) => format!("export {}: {e}", path.display()),
        }
    }

    fn scroll(&mut self, key: KeyEvent) {
        let page = self.rows.get().max(1);
        let Some(Pane::Text { lines, top, left }) = self.panes.get_mut(self.current) else {
            return;
        };
        let last_top = lines.len().saturating_sub(page);
        match key.code {
            KeyCode::Up => *top = top.saturating_sub(1),
            KeyCode::Down => *top = (*top + 1).min(last_top),
            KeyCode::PageUp => *top = top.saturating_sub(page),
            KeyCode::PageDown => *top = (*top + page).min(last_top),
            KeyCode::Char('g') | KeyCode::Home => *top = 0,
            KeyCode::Left => *left = left.saturating_sub(1),
            KeyCode::Right => *left += 1,
            _ => {}
        }
    }
}

fn pane(view: &View) -> Pane {
    match view {
        View::Diff { left, right, delta } => Pane::Diff(Box::new(DiffViewer::new(
            left.clone(),
            right.clone(),
            delta.clone(),
        ))),
        View::Summary { .. } | View::Table { .. } | View::Doc { .. } | View::Tree { .. } => {
            Pane::Text {
                lines: clean(&render::text(slice::from_ref(view)))
                    .lines()
                    .map(str::to_owned)
                    .collect(),
                top: 0,
                left: 0,
            }
        }
    }
}

impl Screen for OpsResult {
    fn title(&self) -> String {
        let name = self.op.meta().name;
        if self.panes.is_empty() {
            format!("{name} — result")
        } else {
            format!("{name} — view {}/{}", self.current + 1, self.panes.len())
        }
    }

    fn hints(&self) -> String {
        if self.saving.is_some() {
            return "enter save · esc cancel".to_owned();
        }
        let view_keys = match self.panes.get(self.current) {
            Some(Pane::Diff(_)) => "n/p change · s layout · c collapse · / filter · ",
            Some(Pane::Text { .. }) => "↑↓←→ scroll · ",
            None => "",
        };
        format!("{view_keys}tab next view · e export · ctrl+s save preset · esc back")
    }

    fn init(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        Vec::new()
    }

    fn update(&mut self, msg: Msg, ctx: &Ctx) -> Vec<Cmd> {
        let Msg::Key(key) = msg else {
            return Vec::new();
        };
        if let Some(prompt) = &mut self.saving {
            return match prompt.handle(key) {
                PromptEvent::Pending => Vec::new(),
                PromptEvent::Cancel => {
                    self.saving = None;
                    Vec::new()
                }
                PromptEvent::Submit(name) => {
                    self.saving = None;
                    vec![Cmd::Status(presets::save(
                        ctx,
                        self.op.meta().id,
                        &name,
                        &self.input,
                    ))]
                }
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('s') {
                self.saving = Some(Prompt::new("preset name"));
            }
            return Vec::new();
        }
        if let Some(Pane::Diff(viewer)) = self.panes.get_mut(self.current)
            && viewer.handle(key)
        {
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc => return vec![Cmd::Pop],
            KeyCode::Tab => self.step(true),
            KeyCode::BackTab => self.step(false),
            KeyCode::Char('e') => return vec![Cmd::Status(self.export(ctx))],
            _ => self.scroll(key),
        }
        Vec::new()
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        let [body, prompt] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(u16::from(self.saving.is_some())),
        ])
        .areas(area);
        self.rows.set(usize::from(body.height));
        match self.panes.get(self.current) {
            None => frame.render_widget(
                Line::styled("the operation returned nothing to show", theme::dim()),
                body,
            ),
            Some(Pane::Diff(viewer)) => viewer.render(frame, body),
            Some(Pane::Text { lines, top, left }) => {
                let width = usize::from(body.width);
                let shown: Vec<Line> = lines
                    .iter()
                    .skip(*top)
                    .take(usize::from(body.height))
                    .map(|line| Line::from(slice_columns(line, *left, width)))
                    .collect();
                frame.render_widget(Paragraph::new(shown), body);
            }
        }
        if let Some(saving) = &self.saving {
            saving.render(frame, prompt);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use ratatui::{Terminal, buffer::Buffer};
    use ss_core::diff::{self, Opts};
    use ss_core::guard::Class;
    use ss_core::ops::{Node, Param, View, with_defaults};
    use ss_core::preset;
    use ss_core::{jsonutil, report};

    use super::*;
    use crate::tui::stub_ops::{Behavior, StubOp};
    use crate::tui::testing::{ch, ctrl, ctx, fixed_now, key, render_screen, typed};

    fn op() -> Arc<dyn Operation> {
        Arc::new(
            StubOp::new("stub.echo", Class::Read, Behavior::Echo)
                .described("Echo the input", "returns what it was given", "Stub")
                .with_params(vec![
                    Param::new("name").required(true),
                    Param::new("repeat").default("3"),
                ]),
        )
    }

    fn input() -> Input {
        let mut input = Input::default();
        input.insert("name", "world");
        input
    }

    fn summary() -> View {
        View::summary([("Greeting", "hello world"), ("Rows", "3")])
    }

    fn table() -> View {
        View::Table {
            title: "Largest objects".to_owned(),
            cols: vec!["Key".to_owned(), "Size".to_owned()],
            rows: vec![
                vec!["config/orders.json".to_owned(), "12 KiB".to_owned()],
                vec!["config/日本.json".to_owned(), "3 KiB".to_owned()],
            ],
        }
    }

    fn doc(body: &str) -> View {
        View::Doc {
            title: "Body".to_owned(),
            body: body.to_owned(),
            lang: "json".to_owned(),
        }
    }

    fn tree() -> View {
        View::Tree {
            title: "Trace".to_owned(),
            root: Node {
                label: "record orders#42".to_owned(),
                children: vec![Node {
                    label: "s3://config-dev/orders.json".to_owned(),
                    children: Vec::new(),
                }],
            },
        }
    }

    fn diff_view() -> View {
        let left = jsonutil::parse(br#"{"limit": 100, "name": "orders"}"#).unwrap();
        let right = jsonutil::parse(br#"{"limit": 250, "name": "orders"}"#).unwrap();
        View::Diff {
            left: "V1".to_owned(),
            right: "current".to_owned(),
            delta: diff::json(&left, &right, &Opts::default()),
        }
    }

    fn screen(views: Vec<View>, ctx: &Ctx) -> OpsResult {
        OpsResult::new(op(), input(), OpResult { views }, ctx)
    }

    fn press(
        screen: &mut OpsResult,
        ctx: &Ctx,
        keys: &[ratatui::crossterm::event::KeyEvent],
    ) -> Vec<Cmd> {
        let mut last = Vec::new();
        for &k in keys {
            last = screen.update(Msg::Key(k), ctx);
        }
        last
    }

    fn status(cmds: &[Cmd]) -> &str {
        match cmds {
            [Cmd::Status(text)] => text,
            _ => panic!("expected exactly one status message"),
        }
    }

    #[test]
    fn tab_cycles_through_the_views_and_wraps() {
        let ctx = ctx();
        let mut screen = screen(vec![summary(), table(), tree()], &ctx);
        assert_eq!(screen.title(), "Echo the input — view 1/3");

        press(&mut screen, &ctx, &[key(KeyCode::Tab)]);
        assert_eq!(screen.title(), "Echo the input — view 2/3");
        assert!(render_screen(&screen, 60, 8).contains("Largest objects"));

        press(&mut screen, &ctx, &[key(KeyCode::Tab), key(KeyCode::Tab)]);
        assert_eq!(screen.title(), "Echo the input — view 1/3");

        press(&mut screen, &ctx, &[key(KeyCode::BackTab)]);
        assert_eq!(screen.title(), "Echo the input — view 3/3");
    }

    #[test]
    fn summary_table_doc_and_tree_views_render_as_text() {
        let ctx = ctx();
        let mut screen = screen(
            vec![summary(), table(), doc("{\n  \"a\": 1\n}\n"), tree()],
            &ctx,
        );

        let mut shown = Vec::new();
        for _ in 0..4 {
            shown.push(render_screen(&screen, 44, 6));
            press(&mut screen, &ctx, &[key(KeyCode::Tab)]);
        }

        insta::assert_snapshot!(shown.join("----\n"));
    }

    #[test]
    fn a_result_with_no_views_says_so() {
        let ctx = ctx();
        let mut screen = screen(Vec::new(), &ctx);

        assert_eq!(screen.title(), "Echo the input — result");
        assert!(render_screen(&screen, 60, 4).contains("the operation returned nothing to show"));
        assert!(press(&mut screen, &ctx, &[key(KeyCode::Tab)]).is_empty());
    }

    #[test]
    fn escape_sequences_in_result_text_never_reach_the_screen() {
        let ctx = ctx();
        let screen = screen(vec![doc("before\x1b[2Jafter\x07")], &ctx);
        let mut term = Terminal::new(TestBackend::new(40, 4)).unwrap();

        term.draw(|frame| screen.view(frame, frame.area())).unwrap();

        let buffer: &Buffer = term.backend().buffer();
        let drawn: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(drawn.contains("before[2Jafter"), "{drawn}");
        assert!(!drawn.chars().any(char::is_control), "{drawn:?}");
    }

    #[test]
    fn tabs_in_document_text_are_expanded_to_spaces() {
        let ctx = ctx();
        let screen = screen(vec![doc("key\tvalue")], &ctx);

        assert!(render_screen(&screen, 40, 4).contains("key     value"));
    }

    #[test]
    fn a_long_view_scrolls_by_line_and_by_page() {
        let ctx = ctx();
        let body: String = (0..60).map(|i| format!("line {i:02}\n")).collect();
        let mut screen = screen(vec![doc(&body)], &ctx);
        assert!(render_screen(&screen, 40, 5).contains("line 00"));

        press(&mut screen, &ctx, &[key(KeyCode::Down), key(KeyCode::Down)]);
        let text = render_screen(&screen, 40, 5);
        assert!(text.starts_with("line 01\n"), "{text}");

        // A page is as many lines as were last drawn.
        press(&mut screen, &ctx, &[key(KeyCode::PageDown)]);
        assert!(render_screen(&screen, 40, 5).starts_with("line 06\n"));

        press(
            &mut screen,
            &ctx,
            &[key(KeyCode::Up), key(KeyCode::PageUp), key(KeyCode::PageUp)],
        );
        assert!(
            render_screen(&screen, 40, 5).starts_with("Body\n"),
            "stops at the top"
        );

        press(&mut screen, &ctx, &[key(KeyCode::PageDown); 20]);
        let text = render_screen(&screen, 40, 5);
        assert!(
            text.contains("line 59"),
            "stops with the last line in view: {text}"
        );
    }

    #[test]
    fn a_table_wider_than_the_screen_scrolls_horizontally_without_breaking_wide_characters() {
        let ctx = ctx();
        let mut screen = screen(vec![table()], &ctx);

        press(&mut screen, &ctx, &[key(KeyCode::Right); 8]);

        assert_eq!(
            render_screen(&screen, 16, 5),
            "objects\n\
             \x20           Size\n\
             ----------  ----\n\
             rders.json  12 K\n\
             \x20本.json      3\n"
        );

        press(&mut screen, &ctx, &[key(KeyCode::Left); 20]);
        assert!(
            render_screen(&screen, 16, 5).starts_with("Largest objects\n"),
            "stops at the left edge"
        );
    }

    #[test]
    fn a_diff_view_is_shown_in_the_diff_viewer() {
        let ctx = ctx();
        let mut screen = screen(vec![diff_view()], &ctx);
        assert!(screen.hints().contains("n/p change"), "{}", screen.hints());

        press(&mut screen, &ctx, &[ch('c')]);

        let text = render_screen(&screen, 50, 4);
        assert!(text.contains("> ~ limit: 100 -> 250"), "{text}");
    }

    #[test]
    fn e_writes_the_markdown_report_and_reports_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx();
        ctx.export_dir = dir.path().to_path_buf();
        let views = vec![summary(), table()];
        let mut screen = screen(views.clone(), &ctx);

        let cmds = press(&mut screen, &ctx, &[ch('e')]);

        let path = dir.path().join("ss-support-stub.echo-20260102-030405.md");
        assert_eq!(status(&cmds), format!("exported to {}", path.display()));
        let op = op();
        let resolved = with_defaults(op.as_ref(), input());
        assert_eq!(
            resolved.str("repeat"),
            "3",
            "defaults are part of the report"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            report::markdown(&op.meta(), &resolved, &views, fixed_now())
        );
    }

    #[test]
    fn a_failed_export_shows_the_error_and_stays_on_the_result() {
        let ctx = ctx();
        let mut screen = screen(vec![summary()], &ctx);

        let cmds = press(&mut screen, &ctx, &[ch('e')]);

        let message = status(&cmds);
        assert!(
            message.starts_with(
                "export /nonexistent/ss-support-tests/ss-support-stub.echo-20260102-030405.md: "
            ),
            "{message}"
        );
    }

    #[test]
    fn ctrl_s_saves_the_run_as_a_preset() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx();
        ctx.presets = preset::Store::new(dir.path().join("presets.yaml"));
        let mut screen = screen(vec![summary()], &ctx);

        press(&mut screen, &ctx, &[ctrl('s')]);
        // The empty prompt is drawn; trailing blanks are not part of the text.
        assert!(render_screen(&screen, 40, 4).contains("preset name:"));
        let mut keys = typed("nightly");
        keys.push(key(KeyCode::Enter));
        let cmds = press(&mut screen, &ctx, &keys);

        assert_eq!(status(&cmds), "saved preset \"nightly\" for stub.echo");
        let saved = ctx.presets.get("stub.echo", "nightly").unwrap().unwrap();
        assert_eq!(saved.get("name").map(String::as_str), Some("world"));
        assert!(
            !saved.contains_key("repeat"),
            "defaults are not frozen into the preset"
        );
        assert!(!render_screen(&screen, 40, 4).contains("preset name"));
    }

    #[test]
    fn letters_typed_into_the_preset_name_are_not_commands() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx();
        ctx.export_dir = dir.path().to_path_buf();
        let mut screen = screen(vec![summary(), table()], &ctx);

        press(&mut screen, &ctx, &[ctrl('s')]);
        press(&mut screen, &ctx, &typed("e"));

        assert!(render_screen(&screen, 40, 4).contains("preset name: e"));
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "nothing was exported"
        );
    }

    #[test]
    fn esc_closes_the_preset_prompt_before_it_leaves_the_screen() {
        let ctx = ctx();
        let mut screen = screen(vec![summary()], &ctx);
        press(&mut screen, &ctx, &[ctrl('s')]);

        let cmds = press(&mut screen, &ctx, &[key(KeyCode::Esc)]);
        assert!(cmds.is_empty());
        assert!(!render_screen(&screen, 40, 4).contains("preset name"));

        let cmds = press(&mut screen, &ctx, &[key(KeyCode::Esc)]);
        assert!(matches!(cmds.as_slice(), [Cmd::Pop]));
    }

    #[test]
    fn a_refused_preset_shows_why() {
        let ctx = ctx();
        let mut screen = OpsResult::new(
            op(),
            Input::default(),
            OpResult {
                views: vec![summary()],
            },
            &ctx,
        );
        press(&mut screen, &ctx, &[ctrl('s')]);
        let mut keys = typed("nightly");
        keys.push(key(KeyCode::Enter));

        let cmds = press(&mut screen, &ctx, &keys);

        assert!(
            status(&cmds).starts_with("preset not saved: "),
            "{}",
            status(&cmds)
        );
    }

    #[test]
    fn esc_pops_back_to_the_screen_below() {
        let ctx = ctx();
        let mut screen = screen(vec![summary()], &ctx);

        let cmds = press(&mut screen, &ctx, &[key(KeyCode::Esc)]);

        assert!(matches!(cmds.as_slice(), [Cmd::Pop]));
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let ctx = ctx();
        let mut screen = screen(vec![summary(), diff_view()], &ctx);
        for _ in 0..2 {
            render_screen(&screen, 1, 1);
            press(&mut screen, &ctx, &[key(KeyCode::Tab)]);
        }
    }
}
