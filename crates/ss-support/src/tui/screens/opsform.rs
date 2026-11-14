//! The parameter form of one operation, and the screen that shows its run:
//! live steps, the confirmation question, and how the run ended.

use std::iter::repeat_n;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ss_core::fuzzy;
use ss_core::ops::{Choice, FailureKind, Input, Operation, Param, RunFailure};
use ss_core::render;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

use crate::tui::components::confirm::ConfirmModal;
use crate::tui::components::errorpanel::render_notice;
use crate::tui::components::progresslist::ProgressList;
use crate::tui::components::prompt::{Prompt, PromptEvent};
use crate::tui::screen::{Cmd, Ctx, Loaded, Msg, RunRequest, RunResult, Screen};
use crate::tui::screens::opsresult::OpsResult;
use crate::tui::text::clean;
use crate::tui::widgets::text_input::TextInput;
use crate::tui::{presets, theme};

/// One parameter and what has been typed for it.
struct Field {
    param: Param,
    input: TextInput,
}

/// What is open on top of the fields.
enum Overlay {
    /// Nothing; keys edit the form.
    None,
    /// The saved presets to load from.
    Presets { names: Vec<String>, selected: usize },
    /// The name to save the form under.
    Save(Prompt),
}

/// The suggested values for the focused field.
#[derive(Default)]
struct Suggestions {
    /// The load these belong to; results of any other load are ignored.
    request: u64,
    choices: Vec<Choice>,
    /// The highlighted row of the shown choices, once the list is entered.
    selected: Option<usize>,
}

/// The parameter form of one operation.
pub struct OpsForm {
    op: Arc<dyn Operation>,
    dry_run: bool,
    fields: Vec<Field>,
    focus: usize,
    /// Why the form could not be run as filled in.
    error: Option<String>,
    overlay: Overlay,
    suggestions: Suggestions,
    /// The number of the last suggestion load asked for.
    requests: u64,
}

impl OpsForm {
    /// An empty form for `op`.
    pub fn new(op: Arc<dyn Operation>, ctx: &Ctx) -> Self {
        OpsForm::with_input(op, &Input::default(), ctx)
    }

    /// A form for `op` with the values of `input` filled in.
    pub fn with_input(op: Arc<dyn Operation>, input: &Input, ctx: &Ctx) -> Self {
        let fields = op
            .params()
            .into_iter()
            .map(|param| Field {
                input: TextInput::new(input.str(param.name)),
                param,
            })
            .collect();
        OpsForm {
            op,
            dry_run: ctx.flags.dry_run,
            fields,
            focus: 0,
            error: None,
            overlay: Overlay::None,
            suggestions: Suggestions::default(),
            requests: 0,
        }
    }

    /// The values typed so far; empty fields are left out so defaults and
    /// presets keep their meaning.
    fn input(&self) -> Input {
        let mut input = Input::default();
        for field in &self.fields {
            if !field.input.value().is_empty() {
                input.insert(field.param.name, field.input.value());
            }
        }
        input
    }

    /// Asks for the focused field's suggestions, given everything typed so
    /// far. Results of earlier loads are no longer wanted.
    fn fetch(&mut self, ctx: &Ctx) -> Vec<Cmd> {
        self.requests += 1;
        self.suggestions = Suggestions {
            request: self.requests,
            ..Suggestions::default()
        };
        let Some(suggest) = self
            .fields
            .get(self.focus)
            .and_then(|field| field.param.suggest.clone())
        else {
            return Vec::new();
        };
        let load = suggest(ctx.deps.clone(), self.input());
        vec![Cmd::Task(
            self.requests,
            Box::pin(async move {
                match load.await {
                    Ok(choices) => Loaded::Suggestions(choices),
                    Err(e) => Loaded::Failed(format!("{e:#}")),
                }
            }),
        )]
    }

    /// The suggestions matching what is typed in the focused field, best
    /// match first.
    fn shown_suggestions(&self) -> Vec<&Choice> {
        let typed = self
            .fields
            .get(self.focus)
            .map_or("", |field| field.input.value());
        let texts: Vec<String> = self.suggestions.choices.iter().map(choice_text).collect();
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        fuzzy::rank(typed, &texts)
            .into_iter()
            .filter_map(|(index, _)| self.suggestions.choices.get(index))
            .collect()
    }

    fn move_focus(&mut self, forward: bool, ctx: &Ctx) -> Vec<Cmd> {
        let count = self.fields.len();
        if count == 0 {
            return Vec::new();
        }
        let step = if forward { 1 } else { count - 1 };
        self.focus = (self.focus + step) % count;
        self.fetch(ctx)
    }

    fn run(&mut self) -> Vec<Cmd> {
        let missing = self.fields.iter().find(|field| {
            field.param.required && field.param.default.is_none() && field.input.value().is_empty()
        });
        if let Some(field) = missing {
            self.error = Some(format!("{} is required", field.param.display_label()));
            return Vec::new();
        }
        vec![Cmd::Push(Box::new(ProgressScreen::new(
            Arc::clone(&self.op),
            self.input(),
        )))]
    }

    fn open_presets(&mut self, ctx: &Ctx) -> Vec<Cmd> {
        let id = self.op.meta().id;
        match ctx.presets.list(id) {
            Err(e) => vec![Cmd::Status(format!("presets unavailable: {e}"))],
            Ok(names) if names.is_empty() => {
                vec![Cmd::Status(format!("no presets saved for {id}"))]
            }
            Ok(names) => {
                self.overlay = Overlay::Presets { names, selected: 0 };
                Vec::new()
            }
        }
    }

    fn load_preset(&mut self, name: &str, ctx: &Ctx) -> Vec<Cmd> {
        self.overlay = Overlay::None;
        let message = match ctx.presets.get(self.op.meta().id, name) {
            Err(e) => format!("presets unavailable: {e}"),
            Ok(None) => format!("preset {name:?} no longer exists"),
            Ok(Some(values)) => {
                for field in &mut self.fields {
                    field
                        .input
                        .set(values.get(field.param.name).map_or("", String::as_str));
                }
                self.error = None;
                format!("loaded preset {name:?}")
            }
        };
        vec![Cmd::Status(message)]
    }

    /// Puts pasted text where typing would go: the preset name being
    /// entered, or the focused field. It is never a key, so its line breaks
    /// run nothing.
    fn paste(&mut self, text: &str) {
        match &mut self.overlay {
            Overlay::Save(prompt) => prompt.paste(text),
            Overlay::Presets { .. } => {}
            Overlay::None => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    field.input.paste(text);
                    self.error = None;
                    self.suggestions.selected = None;
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Vec<Cmd> {
        match &mut self.overlay {
            Overlay::Save(prompt) => match prompt.handle(key) {
                PromptEvent::Pending => Vec::new(),
                PromptEvent::Cancel => {
                    self.overlay = Overlay::None;
                    Vec::new()
                }
                PromptEvent::Submit(name) => {
                    self.overlay = Overlay::None;
                    vec![Cmd::Status(presets::save(
                        ctx,
                        self.op.meta().id,
                        &name,
                        &self.input(),
                    ))]
                }
            },
            Overlay::Presets { names, selected } => {
                match key.code {
                    KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(names.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        if let Some(name) = names.get(*selected).cloned() {
                            return self.load_preset(&name, ctx);
                        }
                    }
                    _ => {}
                }
                Vec::new()
            }
            Overlay::None => self.on_form_key(key, ctx),
        }
    }

    fn on_form_key(&mut self, key: KeyEvent, ctx: &Ctx) -> Vec<Cmd> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('p') => return self.open_presets(ctx),
                KeyCode::Char('s') => {
                    self.overlay = Overlay::Save(Prompt::new("preset name"));
                    return Vec::new();
                }
                _ => {}
            }
        }
        let shown = self.shown_suggestions().len();
        match (key.code, self.suggestions.selected) {
            (KeyCode::Esc, Some(_)) => self.suggestions.selected = None,
            (KeyCode::Esc, None) => return vec![Cmd::Pop],
            (KeyCode::Tab, _) => return self.move_focus(true, ctx),
            (KeyCode::BackTab, _) => return self.move_focus(false, ctx),
            (KeyCode::Down, None) if shown == 0 => return self.move_focus(true, ctx),
            (KeyCode::Down, None) => self.suggestions.selected = Some(0),
            (KeyCode::Down, Some(row)) => {
                self.suggestions.selected = Some((row + 1).min(shown.saturating_sub(1)));
            }
            (KeyCode::Up, None) => return self.move_focus(false, ctx),
            (KeyCode::Up, Some(row)) => self.suggestions.selected = row.checked_sub(1),
            (KeyCode::Enter, None) => return self.run(),
            (KeyCode::Enter, Some(row)) => {
                let value = self
                    .shown_suggestions()
                    .get(row)
                    .map(|choice| choice.value.clone());
                if let (Some(value), Some(field)) = (value, self.fields.get_mut(self.focus)) {
                    field.input.set(&value);
                    self.error = None;
                }
                // What was chosen may itself lead further, as a prefix
                // leads to the keys under it, so the list is asked for
                // again.
                return self.fetch(ctx);
            }
            _ => {
                if let Some(field) = self.fields.get_mut(self.focus)
                    && field.input.handle(key)
                {
                    self.error = None;
                    self.suggestions.selected = None;
                }
            }
        }
        Vec::new()
    }

    fn render_fields(&self, frame: &mut Frame, area: Rect) {
        let label_cols = self
            .fields
            .iter()
            .map(|field| field.param.display_label().width())
            .max()
            .unwrap_or(0);
        // marker, label, required mark, gap
        let prefix_cols = u16::try_from(2 + label_cols + 2 + 2).unwrap_or(u16::MAX);
        let rows = usize::from(area.height);
        let top = (self.focus + 1).saturating_sub(rows);
        let row_areas = Layout::vertical(repeat_n(Constraint::Length(1), rows)).split(area);
        for ((index, field), row) in self
            .fields
            .iter()
            .enumerate()
            .skip(top)
            .zip(row_areas.iter())
        {
            let focused = index == self.focus;
            let label = field.param.display_label();
            let pad = " ".repeat(label_cols.saturating_sub(label.width()));
            let marker = if focused { "> " } else { "  " };
            let required = if field.param.required { " *" } else { "  " };
            let style = if focused {
                theme::heading()
            } else {
                Style::new()
            };
            let [prefix, value] =
                Layout::horizontal([Constraint::Length(prefix_cols), Constraint::Min(0)])
                    .areas(*row);
            frame.render_widget(
                Line::styled(format!("{marker}{label}{pad}{required}  "), style),
                prefix,
            );
            if field.input.value().is_empty()
                && let Some(default) = field.param.default
            {
                frame.render_widget(
                    Line::styled(format!("default: {default}"), theme::dim()),
                    value,
                );
            }
            let editing = focused && matches!(self.overlay, Overlay::None);
            field.input.render(frame, value, editing);
        }
    }

    fn render_panel(&self, frame: &mut Frame, area: Rect) {
        match &self.overlay {
            Overlay::Save(prompt) => prompt.render(frame, area),
            Overlay::Presets { names, selected } => {
                let rows = names.iter().map(String::as_str);
                render_choices(frame, area, "presets:", rows, Some(*selected));
            }
            Overlay::None => {
                let shown = self.shown_suggestions();
                if !shown.is_empty() {
                    let texts: Vec<String> =
                        shown.iter().map(|choice| choice_text(choice)).collect();
                    let rows = texts.iter().map(String::as_str);
                    render_choices(
                        frame,
                        area,
                        "suggestions (↓ to choose):",
                        rows,
                        self.suggestions.selected,
                    );
                }
            }
        }
    }
}

/// What a suggestion is listed as: its label when it has one.
fn choice_text(choice: &Choice) -> String {
    clean(choice.label.as_deref().unwrap_or(&choice.value))
}

/// Draws `title` and then `rows`, marking the selected one and keeping it
/// in view.
fn render_choices<'a>(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    rows: impl Iterator<Item = &'a str>,
    selected: Option<usize>,
) {
    let [heading, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Line::styled(title.to_owned(), theme::dim()), heading);
    let visible = usize::from(list.height);
    let top = selected.map_or(0, |row| (row + 1).saturating_sub(visible));
    let lines: Vec<Line> = rows
        .enumerate()
        .skip(top)
        .take(visible)
        .map(|(index, text)| {
            if selected == Some(index) {
                Line::styled(format!("> {text}"), theme::selected())
            } else {
                Line::from(format!("  {text}"))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
}

impl Screen for OpsForm {
    fn title(&self) -> String {
        let name = self.op.meta().name;
        if self.dry_run {
            format!("{name} (dry run)")
        } else {
            name.to_owned()
        }
    }

    fn hints(&self) -> String {
        match self.overlay {
            Overlay::None => {
                "tab/↑↓ move · enter run · ctrl+p presets · ctrl+s save preset · esc back"
            }
            Overlay::Presets { .. } => "↑↓ choose · enter load · esc close",
            Overlay::Save(_) => "enter save · esc cancel",
        }
        .to_owned()
    }

    fn init(&mut self, ctx: &Ctx) -> Vec<Cmd> {
        self.fetch(ctx)
    }

    fn update(&mut self, msg: Msg, ctx: &Ctx) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) => self.on_key(key, ctx),
            Msg::Paste(text) => {
                self.paste(&text);
                Vec::new()
            }
            Msg::Loaded(request, loaded) if request == self.suggestions.request => match loaded {
                Loaded::Suggestions(choices) => {
                    self.suggestions.choices = choices;
                    Vec::new()
                }
                Loaded::Failed(e) => vec![Cmd::Status(format!("suggestions unavailable: {e}"))],
                Loaded::Buckets(_)
                | Loaded::Page(_)
                | Loaded::Versions(_)
                | Loaded::Pods(_)
                | Loaded::NoPodsUnconfirmed(_) => Vec::new(),
            },
            Msg::Loaded(..)
            | Msg::Resize
            | Msg::Progress(_)
            | Msg::Warning(_)
            | Msg::Confirm(_)
            | Msg::RunDone(_)
            | Msg::Lines(..)
            | Msg::StreamEnded(..) => Vec::new(),
        }
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        let field_rows = u16::try_from(self.fields.len())
            .unwrap_or(u16::MAX)
            .min(area.height);
        let [fields, rest] =
            Layout::vertical([Constraint::Length(field_rows), Constraint::Min(0)]).areas(area);
        self.render_fields(frame, fields);
        let [_gap, help, error, panel] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(rest);
        if let Some(text) = self
            .fields
            .get(self.focus)
            .and_then(|field| field.param.help)
        {
            frame.render_widget(Line::styled(clean(text), theme::dim()), help);
        }
        if let Some(text) = &self.error {
            frame.render_widget(Line::styled(clean(text), theme::error()), error);
        }
        self.render_panel(frame, panel);
    }
}

/// How a run that did not succeed ended.
struct Ended {
    /// One word for the title bar.
    headline: &'static str,
    /// The error and anything the run left to show.
    detail: String,
    /// Whether it is a failure rather than the operator's own choice.
    failure: bool,
}

/// One run in progress: its steps, its confirmation question, and, when it
/// does not succeed, why.
struct ProgressScreen {
    op: Arc<dyn Operation>,
    /// The parameters, until the run has been started with them.
    input: Option<Input>,
    steps: ProgressList,
    modal: Option<ConfirmModal>,
    cancel: CancellationToken,
    ended: Option<Ended>,
    /// What went wrong beside the run without failing it, so far.
    warnings: Vec<String>,
}

impl ProgressScreen {
    fn new(op: Arc<dyn Operation>, input: Input) -> Self {
        ProgressScreen {
            op,
            input: Some(input),
            steps: ProgressList::default(),
            modal: None,
            cancel: CancellationToken::new(),
            ended: None,
            warnings: Vec::new(),
        }
    }

    fn finish(&mut self, failure: RunFailure) {
        let (headline, is_failure) = match failure.kind {
            FailureKind::Usage => ("invalid input", true),
            FailureKind::Denied => ("refused", true),
            FailureKind::Failed => ("failed", true),
            FailureKind::Declined => ("declined", false),
            FailureKind::Cancelled => ("cancelled", false),
        };
        let mut detail = format!("{:#}", failure.error);
        if !failure.views.is_empty() {
            detail.push_str("\n\n");
            detail.push_str(&render::text(&failure.views));
        }
        // The status line is gone at the next key; these are not.
        for warning in &self.warnings {
            detail.push_str(&format!("\n\nwarning: {warning}"));
        }
        self.ended = Some(Ended {
            headline,
            detail: clean(&detail),
            failure: is_failure,
        });
    }
}

impl Screen for ProgressScreen {
    fn title(&self) -> String {
        let state = self
            .ended
            .as_ref()
            .map_or("running", |ended| ended.headline);
        format!("{} — {state}", self.op.meta().name)
    }

    fn hints(&self) -> String {
        if self.modal.is_some() {
            "ctrl+c decline"
        } else if self.ended.is_some() {
            "esc back"
        } else {
            "ctrl+c or esc cancel"
        }
        .to_owned()
    }

    fn init(&mut self, ctx: &Ctx) -> Vec<Cmd> {
        let Some(input) = self.input.take() else {
            return Vec::new();
        };
        self.cancel = ctx.cancel.child_token();
        vec![Cmd::Run(RunRequest {
            op: Arc::clone(&self.op),
            input,
            cancel: self.cancel.clone(),
        })]
    }

    fn update(&mut self, msg: Msg, ctx: &Ctx) -> Vec<Cmd> {
        match msg {
            Msg::Progress(progress) => self.steps.apply(progress),
            // Kept for what the run ends on; until then the status line
            // says it.
            Msg::Warning(warning) => {
                let status = Cmd::Status(format!("warning: {warning}"));
                self.warnings.push(warning);
                return vec![status];
            }
            // A question or an ending changes what the next key means, so
            // keys already on their way are dropped: they were typed at
            // the screen as it was.
            Msg::Confirm(request) => {
                self.modal = Some(ConfirmModal::new(request));
                return vec![Cmd::IgnoreKeys];
            }
            Msg::RunDone(result) => {
                self.modal = None;
                let RunResult { outcome, input } = *result;
                match outcome {
                    Ok(result) => {
                        return vec![
                            Cmd::IgnoreKeys,
                            Cmd::Replace(Box::new(OpsResult::new(
                                Arc::clone(&self.op),
                                input,
                                result,
                                std::mem::take(&mut self.warnings),
                                ctx,
                            ))),
                        ];
                    }
                    Err(failure) => {
                        self.finish(failure);
                        return vec![Cmd::IgnoreKeys];
                    }
                }
            }
            Msg::Key(key) => {
                if let Some(modal) = &mut self.modal {
                    if modal.handle(key) {
                        self.modal = None;
                    }
                } else if self.ended.is_some() {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                        return vec![Cmd::Pop];
                    }
                } else if key.code == KeyCode::Esc {
                    self.cancel.cancel();
                }
            }
            Msg::Resize
            | Msg::Paste(_)
            | Msg::Loaded(..)
            | Msg::Lines(..)
            | Msg::StreamEnded(..) => {}
        }
        Vec::new()
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        if let Some(modal) = &self.modal {
            modal.render(frame, area);
            return;
        }
        let Some(ended) = &self.ended else {
            if self.steps.is_empty() {
                frame.render_widget(Line::styled("starting…", theme::dim()), area);
            } else {
                self.steps.render(frame, area);
            }
            return;
        };
        let step_rows = u16::try_from(self.steps.len())
            .unwrap_or(u16::MAX)
            .min(area.height / 2);
        let [steps, notice] =
            Layout::vertical([Constraint::Length(step_rows), Constraint::Min(0)]).areas(area);
        self.steps.render(frame, steps);
        let style = if ended.failure {
            theme::error()
        } else {
            theme::changed()
        };
        render_notice(frame, notice, ended.headline, &ended.detail, style);
    }

    fn interrupt(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        if let Some(modal) = self.modal.take() {
            modal.decline();
        } else if self.ended.is_some() {
            return vec![Cmd::Pop];
        } else {
            self.cancel.cancel();
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ss_core::audit::{Entry, Logger};
    use ss_core::guard::Class;
    use ss_core::ops::{Choice, Param, SuggestFn};
    use ss_core::preset;

    use super::*;
    use crate::tui::screen::Loaded;
    use crate::tui::shell::Shell;
    use crate::tui::stub_ops::{Behavior, StubOp};
    use crate::tui::testing::{
        ch, ctrl, ctx, key, press, render_screen, render_shell, settle, typed,
    };

    /// The inputs a suggestion source was asked with, oldest first.
    type Asked = Arc<Mutex<Vec<Input>>>;

    fn choice(value: &str) -> Choice {
        Choice {
            value: value.to_owned(),
            label: None,
        }
    }

    /// Suggests the buckets, recording what it was asked with.
    fn buckets(asked: &Asked) -> SuggestFn {
        let asked = Arc::clone(asked);
        Arc::new(move |_deps, input| {
            asked.lock().unwrap().push(input);
            Box::pin(async {
                Ok(vec![
                    Choice {
                        value: "config-dev".to_owned(),
                        label: Some("config-dev — service configs".to_owned()),
                    },
                    choice("logs-dev"),
                ])
            })
        })
    }

    /// Suggests one key under the bucket typed so far.
    fn keys(asked: &Asked) -> SuggestFn {
        let asked = Arc::clone(asked);
        Arc::new(move |_deps, input| {
            let bucket = input.str("bucket").to_owned();
            asked.lock().unwrap().push(input);
            Box::pin(async move { Ok(vec![choice(&format!("{bucket}/orders.json"))]) })
        })
    }

    fn failing() -> SuggestFn {
        Arc::new(|_deps, _input| Box::pin(async { Err(anyhow::anyhow!("S3 is not configured")) }))
    }

    fn params(bucket: SuggestFn, key: SuggestFn) -> Vec<Param> {
        vec![
            Param::new("bucket")
                .label("Bucket")
                .required(true)
                .help("Bucket to read")
                .suggest(bucket),
            Param::new("key")
                .label("Key")
                .help("Object key")
                .suggest(key),
            Param::new("max")
                .label("Max")
                .default("100")
                .help("Most rows to show"),
        ]
    }

    fn stub(behavior: Behavior, class: Class, params: Vec<Param>) -> Arc<dyn Operation> {
        Arc::new(
            StubOp::new("stub.op", class, behavior)
                .described("Inspect an object", "reads one object", "Stub")
                .with_params(params),
        )
    }

    /// An operation that echoes its input, with bucket and key suggestions.
    fn echo(asked: &Asked) -> Arc<dyn Operation> {
        stub(
            Behavior::Echo,
            Class::Read,
            params(buckets(asked), keys(asked)),
        )
    }

    /// An operation with one plain required parameter.
    fn plain(behavior: Behavior, class: Class) -> Arc<dyn Operation> {
        stub(
            behavior,
            class,
            vec![Param::new("target").label("Target").required(true)],
        )
    }

    fn send(form: &mut OpsForm, ctx: &Ctx, keys: &[KeyEvent]) -> Vec<Cmd> {
        let mut last = Vec::new();
        for &k in keys {
            last = form.update(Msg::Key(k), ctx);
        }
        last
    }

    fn status(cmds: &[Cmd]) -> &str {
        match cmds {
            [Cmd::Status(text)] => text,
            _ => panic!("expected exactly one status message"),
        }
    }

    fn enter() -> KeyEvent {
        key(KeyCode::Enter)
    }

    /// A shell whose only screen is the form for `op`, with `text` typed
    /// into the first field.
    fn shell_with(ctx: Ctx, op: Arc<dyn Operation>, text: &str) -> Shell {
        let form = OpsForm::new(op, &ctx);
        let mut shell = Shell::new(ctx, Box::new(form));
        press(&mut shell, &typed(text));
        shell
    }

    #[test]
    fn one_field_per_param_with_defaults_shown_and_help_for_the_focused_field() {
        let ctx = ctx();
        let form = OpsForm::new(echo(&Asked::default()), &ctx);

        assert_eq!(form.title(), "Inspect an object");
        insta::assert_snapshot!(render_screen(&form, 60, 10));
    }

    #[test]
    fn a_dry_run_is_named_in_the_title() {
        let mut ctx = ctx();
        ctx.flags.dry_run = true;

        let form = OpsForm::new(echo(&Asked::default()), &ctx);

        assert_eq!(form.title(), "Inspect an object (dry run)");
    }

    #[test]
    fn tab_and_arrows_move_focus_and_wrap() {
        let ctx = ctx();
        let mut form = OpsForm::new(plain_three(), &ctx);
        let focused = |form: &OpsForm| {
            render_screen(form, 60, 10)
                .lines()
                .find_map(|line| line.strip_prefix("> "))
                .map(|rest| {
                    rest.split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_owned()
                })
                .unwrap_or_default()
        };
        assert_eq!(focused(&form), "One");

        send(&mut form, &ctx, &[key(KeyCode::Tab)]);
        assert_eq!(focused(&form), "Two");

        send(&mut form, &ctx, &[key(KeyCode::Down), key(KeyCode::Down)]);
        assert_eq!(focused(&form), "One", "wraps past the last field");

        send(&mut form, &ctx, &[key(KeyCode::Up)]);
        assert_eq!(focused(&form), "Three", "wraps before the first field");

        send(&mut form, &ctx, &[key(KeyCode::BackTab)]);
        assert_eq!(focused(&form), "Two");
    }

    /// Three plain parameters, none with suggestions.
    fn plain_three() -> Arc<dyn Operation> {
        stub(
            Behavior::Echo,
            Class::Read,
            vec![
                Param::new("one").label("One"),
                Param::new("two").label("Two"),
                Param::new("three").label("Three"),
            ],
        )
    }

    #[test]
    fn a_missing_required_param_is_reported_and_nothing_runs() {
        let ctx = ctx();
        let mut form = OpsForm::new(plain(Behavior::Echo, Class::Read), &ctx);

        let cmds = send(&mut form, &ctx, &[enter()]);

        assert!(cmds.is_empty());
        assert!(render_screen(&form, 60, 10).contains("Target is required"));

        send(&mut form, &ctx, &[ch('x')]);
        assert!(
            !render_screen(&form, 60, 10).contains("is required"),
            "typing clears the message"
        );
    }

    #[test]
    fn enter_pushes_a_progress_screen_that_runs_the_filled_input() {
        let ctx = ctx();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);
        send(&mut form, &ctx, &typed("config-dev"));

        let mut cmds = send(&mut form, &ctx, &[enter()]);

        let Some(Cmd::Push(mut progress)) = cmds.pop() else {
            panic!("enter pushes the progress screen");
        };
        let mut started = progress.init(&ctx);
        let Some(Cmd::Run(request)) = started.pop() else {
            panic!("the progress screen starts the run");
        };
        assert_eq!(request.op.meta().id, "stub.op");
        let given: Vec<(&str, &str)> = request
            .input
            .as_map()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            given,
            [("bucket", "config-dev")],
            "empty fields and defaults are left to the runner"
        );
    }

    #[tokio::test]
    async fn suggestions_load_for_the_focused_field_from_the_input_so_far() {
        let asked = Asked::default();
        let mut shell = shell_with(ctx(), echo(&asked), "");
        let shown = settle(&mut shell, "logs-dev").await;
        assert!(shown.contains("config-dev — service configs"), "{shown}");

        press(&mut shell, &typed("logs-dev"));
        press(&mut shell, &[key(KeyCode::Tab)]);
        let shown = settle(&mut shell, "logs-dev/orders.json").await;

        assert!(!shown.contains("service configs"), "{shown}");
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0].str("bucket"), "");
        assert_eq!(asked[1].str("bucket"), "logs-dev");
    }

    #[tokio::test]
    async fn typing_filters_the_suggestions_and_enter_fills_the_field() {
        let mut shell = shell_with(ctx(), echo(&Asked::default()), "");
        settle(&mut shell, "logs-dev").await;

        press(&mut shell, &typed("log"));
        let shown = render_shell(&shell, 80, 24);
        assert!(!shown.contains("config-dev"), "{shown}");

        press(&mut shell, &[key(KeyCode::Down), enter()]);

        let shown = render_shell(&shell, 80, 24);
        assert!(shown.contains("> Bucket *  logs-dev"), "{shown}");
        assert_eq!(
            shell.depth(),
            1,
            "enter chose the suggestion, it did not run"
        );
    }

    #[tokio::test]
    async fn up_leaves_the_suggestions_and_returns_to_the_field() {
        let mut shell = shell_with(ctx(), echo(&Asked::default()), "");
        settle(&mut shell, "logs-dev").await;

        press(
            &mut shell,
            &[
                key(KeyCode::Down),
                key(KeyCode::Down),
                key(KeyCode::Up),
                key(KeyCode::Up),
            ],
        );
        press(&mut shell, &typed("x"));

        let shown = render_shell(&shell, 80, 24);
        assert!(shown.contains("> Bucket *  x"), "{shown}");
    }

    #[tokio::test]
    async fn a_failed_suggestion_shows_a_status_line_and_the_field_stays_editable() {
        let op = stub(
            Behavior::Echo,
            Class::Read,
            params(failing(), keys(&Asked::default())),
        );
        let mut shell = shell_with(ctx(), op, "");

        let shown = settle(&mut shell, "suggestions unavailable").await;
        assert!(
            shown.contains("suggestions unavailable: S3 is not configured"),
            "{shown}"
        );

        press(&mut shell, &typed("typed-by-hand"));
        assert!(render_shell(&shell, 80, 24).contains("> Bucket *  typed-by-hand"));
    }

    #[test]
    fn a_stale_suggestion_response_is_ignored() {
        let ctx = ctx();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);
        let first = match form.init(&ctx).pop() {
            Some(Cmd::Task(id, _)) => id,
            _ => panic!("the first field asks for suggestions"),
        };
        let second = match send(&mut form, &ctx, &[key(KeyCode::Tab)]).pop() {
            Some(Cmd::Task(id, _)) => id,
            _ => panic!("the second field asks for suggestions"),
        };
        assert_ne!(first, second);

        form.update(
            Msg::Loaded(first, Loaded::Suggestions(vec![choice("stale-bucket")])),
            &ctx,
        );
        assert!(!render_screen(&form, 60, 12).contains("stale-bucket"));

        form.update(
            Msg::Loaded(second, Loaded::Suggestions(vec![choice("fresh-key")])),
            &ctx,
        );
        assert!(render_screen(&form, 60, 12).contains("fresh-key"));
    }

    #[test]
    fn a_field_without_suggestions_asks_for_none() {
        let ctx = ctx();
        let mut form = OpsForm::new(plain_three(), &ctx);

        assert!(form.init(&ctx).is_empty());
        assert!(send(&mut form, &ctx, &[key(KeyCode::Tab)]).is_empty());
    }

    fn with_presets() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx();
        ctx.presets = preset::Store::new(dir.path().join("presets.yaml"));
        (dir, ctx)
    }

    #[test]
    fn ctrl_p_lists_presets_and_enter_fills_the_form() {
        let (_dir, ctx) = with_presets();
        let nightly = [
            ("bucket".to_owned(), "config-dev".to_owned()),
            ("key".to_owned(), "orders.json".to_owned()),
        ]
        .into();
        ctx.presets.save("stub.op", "nightly", &nightly).unwrap();
        let weekly = [("bucket".to_owned(), "logs-dev".to_owned())].into();
        ctx.presets.save("stub.op", "weekly", &weekly).unwrap();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);
        send(&mut form, &ctx, &typed("scratch"));

        send(&mut form, &ctx, &[ctrl('p')]);
        let shown = render_screen(&form, 60, 12);
        assert!(shown.contains("> nightly"), "{shown}");
        assert!(shown.contains("  weekly"), "{shown}");

        let cmds = send(&mut form, &ctx, &[key(KeyCode::Down), enter()]);

        assert_eq!(status(&cmds), "loaded preset \"weekly\"");
        let shown = render_screen(&form, 60, 12);
        assert!(shown.contains("Bucket *  logs-dev"), "{shown}");
        assert!(!shown.contains("weekly"), "the list has closed: {shown}");
        assert!(!shown.contains("orders.json"), "{shown}");
    }

    #[test]
    fn ctrl_p_with_no_presets_says_none_saved() {
        let (_dir, ctx) = with_presets();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);

        let cmds = send(&mut form, &ctx, &[ctrl('p')]);

        assert_eq!(status(&cmds), "no presets saved for stub.op");
    }

    #[test]
    fn an_unreadable_presets_file_is_reported() {
        let (dir, ctx) = with_presets();
        std::fs::write(dir.path().join("presets.yaml"), "presets: [not, a, map").unwrap();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);

        let cmds = send(&mut form, &ctx, &[ctrl('p')]);

        assert!(
            status(&cmds).starts_with("presets unavailable: "),
            "{}",
            status(&cmds)
        );
    }

    #[test]
    fn ctrl_s_saves_only_the_filled_params_and_reports_the_name() {
        let (_dir, ctx) = with_presets();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);
        send(&mut form, &ctx, &typed("config-dev"));

        send(&mut form, &ctx, &[ctrl('s')]);
        let mut keys = typed("nightly");
        keys.push(enter());
        let cmds = send(&mut form, &ctx, &keys);

        assert_eq!(status(&cmds), "saved preset \"nightly\" for stub.op");
        let saved = ctx.presets.get("stub.op", "nightly").unwrap().unwrap();
        assert_eq!(
            saved.into_iter().collect::<Vec<_>>(),
            [("bucket".to_owned(), "config-dev".to_owned())]
        );
        assert!(
            render_screen(&form, 60, 12).contains("Bucket *  config-dev"),
            "still on the form"
        );
    }

    #[test]
    fn a_refused_preset_name_shows_the_validation_error() {
        let (_dir, ctx) = with_presets();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);

        send(&mut form, &ctx, &[ctrl('s')]);
        let mut keys = typed("empty");
        keys.push(enter());
        let cmds = send(&mut form, &ctx, &keys);

        assert!(
            status(&cmds).starts_with("preset not saved: "),
            "{}",
            status(&cmds)
        );
    }

    #[test]
    fn esc_closes_an_open_overlay_before_it_leaves_the_form() {
        let (_dir, ctx) = with_presets();
        let one = [("bucket".to_owned(), "config-dev".to_owned())].into();
        ctx.presets.save("stub.op", "nightly", &one).unwrap();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);

        send(&mut form, &ctx, &[ctrl('p')]);
        assert!(send(&mut form, &ctx, &[key(KeyCode::Esc)]).is_empty());
        assert!(!render_screen(&form, 60, 12).contains("nightly"));

        send(&mut form, &ctx, &[ctrl('s')]);
        assert!(send(&mut form, &ctx, &[key(KeyCode::Esc)]).is_empty());
        assert!(!render_screen(&form, 60, 12).contains("preset name"));

        let cmds = send(&mut form, &ctx, &[key(KeyCode::Esc)]);
        assert!(matches!(cmds.as_slice(), [Cmd::Pop]));
    }

    #[test]
    fn with_input_fills_the_fields() {
        let ctx = ctx();
        let mut input = Input::default();
        input.insert("bucket", "config-dev");
        input.insert("key", "orders.json");
        input.insert("unknown", "ignored");

        let form = OpsForm::with_input(echo(&Asked::default()), &input, &ctx);

        let shown = render_screen(&form, 60, 10);
        assert!(shown.contains("Bucket *  config-dev"), "{shown}");
        assert!(shown.contains("Key       orders.json"), "{shown}");
        assert!(!shown.contains("ignored"), "{shown}");
    }

    #[tokio::test]
    async fn a_successful_run_shows_the_result_and_esc_returns_to_the_form_with_its_values() {
        let mut shell = shell_with(ctx(), plain(Behavior::Echo, Class::Read), "orders-dev");

        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "view 1/1").await;
        assert!(shown.contains("target : orders-dev"), "{shown}");
        assert_eq!(
            shell.depth(),
            2,
            "the result took the progress screen's place"
        );

        press(&mut shell, &[key(KeyCode::Esc)]);

        assert_eq!(shell.depth(), 1);
        assert!(render_shell(&shell, 80, 24).contains("> Target *  orders-dev"));
    }

    #[tokio::test]
    async fn ctrl_c_during_a_run_cancels_it_and_shows_cancelled() {
        let mut shell = shell_with(ctx(), plain(Behavior::Hang, Class::Read), "x");
        press(&mut shell, &[enter()]);
        settle(&mut shell, "→ wait").await;

        assert!(
            shell.handle(None, Msg::Key(ctrl('c'))),
            "the shell stays open"
        );
        let shown = settle(&mut shell, "— cancelled").await;

        assert!(shown.contains("Inspect an object — cancelled"), "{shown}");
        assert_eq!(shell.depth(), 2);

        press(&mut shell, &[key(KeyCode::Esc)]);
        assert_eq!(shell.depth(), 1);
    }

    #[tokio::test]
    async fn esc_during_a_run_cancels_it() {
        let mut shell = shell_with(ctx(), plain(Behavior::Hang, Class::Read), "x");
        press(&mut shell, &[enter()]);
        settle(&mut shell, "→ wait").await;

        press(&mut shell, &[key(KeyCode::Esc)]);

        settle(&mut shell, "— cancelled").await;
        assert_eq!(shell.depth(), 2, "the outcome is shown before leaving");
    }

    fn write_op(class: Class) -> (Arc<dyn Operation>, Arc<std::sync::atomic::AtomicBool>) {
        let (op, done) = StubOp::write("stub.op", class, "orders-dev");
        let op = op
            .described("Change an object", "writes one object", "Stub")
            .with_params(vec![Param::new("target").label("Target").required(true)]);
        (Arc::new(op), done)
    }

    #[tokio::test]
    async fn a_confirmed_write_ends_on_the_result_screen() {
        let (op, done) = write_op(Class::Mutate);
        let mut shell = shell_with(ctx(), op, "x");
        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "Proceed with stub.op?").await;
        assert!(shown.contains("Will change : orders-dev"), "{shown}");

        press(&mut shell, &[ch('y'), enter()]);
        let shown = settle(&mut shell, "view 1/1").await;

        assert!(shown.contains("Changed : orders-dev"), "{shown}");
        assert!(done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_destructive_write_waits_for_the_typed_name() {
        let (op, done) = write_op(Class::Destructive);
        let mut shell = shell_with(ctx(), op, "x");
        press(&mut shell, &[enter()]);
        settle(&mut shell, "This is DESTRUCTIVE").await;

        press(&mut shell, &[ch('y'), enter()]);
        assert!(render_shell(&shell, 80, 24).contains("name does not match"));
        assert!(!done.load(Ordering::SeqCst));

        press(&mut shell, &[key(KeyCode::Backspace)]);
        press(&mut shell, &typed("orders-dev"));
        press(&mut shell, &[enter()]);
        settle(&mut shell, "view 1/1").await;

        assert!(done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn ctrl_c_during_a_confirmation_declines_it() {
        let (op, done) = write_op(Class::Mutate);
        let mut shell = shell_with(ctx(), op, "x");
        press(&mut shell, &[enter()]);
        settle(&mut shell, "Proceed with stub.op?").await;

        assert!(
            shell.handle(None, Msg::Key(ctrl('c'))),
            "the shell stays open"
        );
        let shown = settle(&mut shell, "declined").await;

        assert!(shown.contains("Change an object — declined"), "{shown}");
        assert!(shown.contains("⊘ apply (skipped)"), "{shown}");
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_failed_run_shows_the_error_chain_and_esc_returns_to_the_form_with_its_values() {
        let mut shell = shell_with(
            ctx(),
            plain(Behavior::Fail("AccessDenied"), Class::Read),
            "orders-dev",
        );

        press(&mut shell, &[enter()]);
        let shown = settle(&mut shell, "failed").await;

        assert!(shown.contains("Inspect an object — failed"), "{shown}");
        assert!(shown.contains("✗ fetch: AccessDenied"), "{shown}");
        assert!(shown.contains("fetch: AccessDenied"), "{shown}");

        press(&mut shell, &[key(KeyCode::Esc)]);
        assert_eq!(shell.depth(), 1);
        assert!(render_shell(&shell, 80, 24).contains("> Target *  orders-dev"));
    }

    #[tokio::test]
    async fn a_dry_run_ends_on_a_result_that_says_it_stopped_and_holds_the_preview() {
        let mut ctx = ctx();
        ctx.flags.dry_run = true;
        let (op, done) = write_op(Class::Mutate);
        let mut shell = shell_with(ctx, op, "x");

        press(&mut shell, &[enter()]);
        // The note that the run stopped, then what it was about to change.
        let shown = settle(&mut shell, "view 1/2").await;

        assert!(
            shown.contains("dry-run: stopped before the first mutating step"),
            "{shown}"
        );
        assert!(!done.load(Ordering::SeqCst));
    }

    struct Unwritable;

    impl Logger for Unwritable {
        fn record(&self, _e: &Entry) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[tokio::test]
    async fn a_warning_from_the_run_stays_on_the_result() {
        let mut ctx = ctx();
        ctx.audit = Arc::new(Unwritable);
        let (op, _done) = write_op(Class::Mutate);
        let mut shell = shell_with(ctx, op, "x");

        press(&mut shell, &[enter()]);
        settle(&mut shell, "Proceed with stub.op?").await;
        press(&mut shell, &[ch('y'), enter()]);
        settle(&mut shell, "view 1/1").await;
        // A key clears the status line; the warning is part of the result.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        press(&mut shell, &[key(KeyCode::Down)]);

        let shown = render_shell(&shell, 80, 24);
        assert_eq!(
            shown.matches("warning: audit log: disk full").count(),
            1,
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_warning_from_a_run_that_failed_stays_with_the_failure() {
        let mut ctx = ctx();
        ctx.audit = Arc::new(Unwritable);
        let op = StubOp::new("stub.fail", Class::Mutate, Behavior::Fail("boom"))
            .with_params(vec![Param::new("target").label("Target").required(true)]);
        let mut shell = shell_with(ctx, Arc::new(op), "x");

        press(&mut shell, &[enter()]);
        settle(&mut shell, "— failed").await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        press(&mut shell, &[key(KeyCode::Down)]);

        let shown = render_shell(&shell, 80, 24);
        assert!(shown.contains("boom"), "{shown}");
        assert_eq!(
            shown.matches("warning: audit log: disk full").count(),
            1,
            "{shown}"
        );
    }

    #[test]
    fn more_fields_than_rows_keeps_the_focused_field_in_view() {
        let ctx = ctx();
        let mut form = OpsForm::new(plain_three(), &ctx);

        send(&mut form, &ctx, &[key(KeyCode::Tab), key(KeyCode::Tab)]);

        let shown = render_screen(&form, 40, 2);
        assert!(shown.contains("> Three"), "{shown}");
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let (_dir, ctx) = with_presets();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);
        render_screen(&form, 1, 1);
        send(&mut form, &ctx, &[ctrl('s')]);
        render_screen(&form, 1, 1);
        render_screen(&form, 0, 0);
    }

    #[test]
    fn a_paste_fills_the_focused_field_and_never_runs() {
        let ctx = ctx();
        let mut form = OpsForm::new(plain(Behavior::Echo, Class::Read), &ctx);

        let cmds = form.update(Msg::Paste("orders-dev\ny\n".to_owned()), &ctx);

        assert!(cmds.is_empty(), "the pasted line breaks are not enter");
        assert!(render_screen(&form, 60, 10).contains("> Target *  orders-devy"));
    }

    #[test]
    fn a_paste_names_a_preset_without_saving_it() {
        let (_dir, ctx) = with_presets();
        let mut form = OpsForm::new(echo(&Asked::default()), &ctx);
        send(&mut form, &ctx, &typed("config-dev"));
        send(&mut form, &ctx, &[ctrl('s')]);

        let cmds = form.update(Msg::Paste("nightly\n".to_owned()), &ctx);

        assert!(cmds.is_empty());
        assert!(render_screen(&form, 60, 12).contains("preset name: nightly"));
        assert!(ctx.presets.get("stub.op", "nightly").unwrap().is_none());
    }

    fn confirm_request(class: Class) -> (Msg, tokio::sync::oneshot::Receiver<bool>) {
        let (request, answer) =
            crate::tui::screen::ConfirmRequest::new(ss_core::ops::ConfirmSpec {
                op_id: "stub.op".to_owned(),
                class,
                resource: "orders-dev".to_owned(),
                step: "apply".to_owned(),
                preview: Vec::new(),
            });
        (Msg::Confirm(request), answer)
    }

    #[test]
    fn a_question_that_opens_asks_for_keys_to_be_ignored() {
        let ctx = ctx();
        let (op, _done) = write_op(Class::Mutate);
        let mut progress = ProgressScreen::new(op, Input::default());
        let (question, _answer) = confirm_request(Class::Mutate);

        let cmds = progress.update(question, &ctx);

        assert!(matches!(cmds.as_slice(), [Cmd::IgnoreKeys]));
    }

    #[test]
    fn a_run_that_ends_asks_for_keys_to_be_ignored_before_it_shows_how() {
        let ctx = ctx();
        let (op, _done) = write_op(Class::Mutate);
        let done = |outcome| {
            Msg::RunDone(Box::new(RunResult {
                outcome,
                input: Input::default(),
            }))
        };

        let mut succeeded = ProgressScreen::new(Arc::clone(&op), Input::default());
        let cmds = succeeded.update(done(Ok(ss_core::ops::OpResult::default())), &ctx);
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::IgnoreKeys, Cmd::Replace(_)]
        ));

        let mut failed = ProgressScreen::new(op, Input::default());
        let failure = RunFailure {
            kind: FailureKind::Failed,
            error: anyhow::anyhow!("boom"),
            views: Vec::new(),
        };
        let cmds = failed.update(done(Err(failure)), &ctx);
        assert!(matches!(cmds.as_slice(), [Cmd::IgnoreKeys]));
    }

    #[test]
    fn a_paste_cannot_answer_the_question() {
        let ctx = ctx();
        let (op, _done) = write_op(Class::Destructive);
        let mut progress = ProgressScreen::new(op, Input::default());
        let (question, mut answer) = confirm_request(Class::Destructive);
        progress.update(question, &ctx);

        progress.update(Msg::Paste("orders-dev\n".to_owned()), &ctx);
        progress.update(Msg::Key(enter()), &ctx);

        assert_eq!(
            answer.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty),
            "the name has to be typed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_typed_before_the_question_could_be_read_is_not_taken() {
        let mut ctx = ctx();
        ctx.settle = std::time::Duration::from_millis(250);
        let (op, done) = write_op(Class::Mutate);
        let mut shell = shell_with(ctx, op, "x");
        press(&mut shell, &[enter()]);
        settle(&mut shell, "Proceed with stub.op?").await;

        press(&mut shell, &[ch('y'), enter()]);
        tokio::task::yield_now().await;
        assert!(
            !done.load(Ordering::SeqCst),
            "typed too early to be an answer"
        );
        assert!(render_shell(&shell, 80, 24).contains("Proceed with stub.op?"));

        tokio::time::advance(std::time::Duration::from_millis(251)).await;
        press(&mut shell, &[ch('y'), enter()]);
        settle(&mut shell, "view 1/1").await;

        assert!(done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn choosing_a_prefix_suggestion_asks_again_for_what_is_under_it() {
        let asked = Asked::default();
        let under: SuggestFn = {
            let asked = Arc::clone(&asked);
            Arc::new(move |_deps, input| {
                let typed = input.str("key").to_owned();
                asked.lock().unwrap().push(input);
                Box::pin(async move {
                    Ok(if typed.is_empty() {
                        vec![choice("app/")]
                    } else {
                        vec![choice("app/orders.json"), choice("app/users.json")]
                    })
                })
            })
        };
        let op = stub(
            Behavior::Echo,
            Class::Read,
            vec![Param::new("key").label("Key").suggest(under)],
        );
        let mut shell = shell_with(ctx(), op, "");
        settle(&mut shell, "app/").await;

        press(&mut shell, &[key(KeyCode::Down), enter()]);
        let shown = settle(&mut shell, "app/users.json").await;

        assert!(shown.contains("> Key    app/"), "{shown}");
        assert_eq!(
            asked
                .lock()
                .unwrap()
                .last()
                .map(|i| i.str("key").to_owned()),
            Some("app/".to_owned())
        );
    }
}
