//! The log of one pod, followed as it grows, in a bounded buffer.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::Arc;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ss_core::svc::kubectl::KubectlPort;

use crate::tui::components::errorpanel::render_error;
use crate::tui::screen::{Cmd, Ctx, Msg, Screen};
use crate::tui::text::{clean, slice_columns, strip_ansi};
use crate::tui::theme;
use crate::tui::widgets::text_input::TextInput;

/// The most log lines kept; older ones are dropped and counted.
pub const MAX_LINES: usize = 5_000;

/// The most log text kept, in bytes: 8 MiB. Beyond it the oldest lines are
/// dropped and counted, as they are beyond [`MAX_LINES`].
pub const MAX_BYTES: usize = 8 << 20;

/// The number this screen gives its one stream.
const STREAM: u64 = 1;

/// How many columns one press of left or right moves.
const SIDEWAYS: usize = 8;

/// Where the stream is in its life.
enum State {
    /// Lines may still arrive.
    Open,
    /// No more lines will arrive; the reason, when the stream failed.
    Ended(Option<String>),
    /// The stream produced nothing but an error, which takes the screen.
    Failed(String),
}

/// The log of one pod.
pub struct LogsTail {
    kubectl: Option<Arc<dyn KubectlPort>>,
    namespace: String,
    pod: String,
    /// The newest lines, at most [`MAX_LINES`] of them and [`MAX_BYTES`] of
    /// text, oldest first, ready to draw.
    lines: VecDeque<String>,
    /// How many bytes of text `lines` holds.
    bytes: usize,
    /// How many lines have been dropped from the front.
    dropped: usize,
    /// Whether the view sticks to the newest line.
    following: bool,
    /// The first line shown, among the lines the filter lets through, while
    /// not following.
    top: usize,
    /// The first column shown.
    left: usize,
    /// Shows only the lines that contain this text.
    filter: TextInput,
    /// Keys are going to the filter.
    filtering: bool,
    state: State,
    /// How many log rows were last drawn; a page moves by this.
    rows: Cell<usize>,
}

impl LogsTail {
    /// The log of `pod` in `namespace` (empty for the context's default).
    pub fn new(namespace: &str, pod: &str, ctx: &Ctx) -> Self {
        let kubectl = ctx.deps.kubectl.clone();
        let state = if kubectl.is_some() {
            State::Open
        } else {
            State::Failed("Kubernetes is not available in this session".to_owned())
        };
        LogsTail {
            kubectl,
            namespace: namespace.to_owned(),
            pod: pod.to_owned(),
            lines: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            following: true,
            top: 0,
            left: 0,
            filter: TextInput::default(),
            filtering: false,
            state,
            rows: Cell::new(1),
        }
    }

    /// Whether the filter lets `line` through.
    fn matches(&self, line: &str) -> bool {
        let wanted = self.filter.value();
        wanted.is_empty() || line.to_lowercase().contains(&wanted.to_lowercase())
    }

    /// The lines the filter lets through, oldest first.
    fn shown(&self) -> Vec<&String> {
        self.lines
            .iter()
            .filter(|line| self.matches(line))
            .collect()
    }

    /// Adds lines to the buffer, dropping the oldest beyond the limits. While
    /// the view is not following, it keeps showing the same lines.
    fn append(&mut self, batch: Vec<String>) {
        for raw in batch {
            // One row per line: colour codes go, and nothing in the line may
            // start a new row or drive the terminal.
            let line = clean(&strip_ansi(&raw)).replace('\n', " ");
            self.bytes += line.len();
            self.lines.push_back(line);
            // The newest line is kept whatever its size.
            while self.lines.len() > MAX_LINES || (self.bytes > MAX_BYTES && self.lines.len() > 1) {
                let Some(oldest) = self.lines.pop_front() else {
                    break;
                };
                self.bytes = self.bytes.saturating_sub(oldest.len());
                self.dropped += 1;
                if !self.following && self.matches(&oldest) {
                    self.top = self.top.saturating_sub(1);
                }
            }
        }
    }

    /// What kubectl reported, when the whole log is its one `error:` line: a
    /// log it could not read at all.
    fn only_error(&self) -> Option<String> {
        if self.lines.len() != 1 || self.dropped > 0 {
            return None;
        }
        let mut reason = self.lines.front()?.strip_prefix("error: ")?;
        while let Some(rest) = reason.strip_prefix("error: ") {
            reason = rest;
        }
        Some(reason.to_owned())
    }

    /// The furthest the view can scroll with the rows last drawn.
    fn last_top(&self) -> usize {
        self.shown().len().saturating_sub(self.rows.get().max(1))
    }

    /// The first line shown right now.
    fn current_top(&self) -> usize {
        let last = self.last_top();
        if self.following {
            last
        } else {
            self.top.min(last)
        }
    }

    /// Moves the view to `top`; reaching the newest line follows again.
    fn scroll_to(&mut self, top: usize) {
        if top >= self.last_top() {
            self.following = true;
        } else {
            self.following = false;
            self.top = top;
        }
    }

    fn clear_filter(&mut self) {
        self.filtering = false;
        self.filter.set("");
        self.following = true;
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => self.clear_filter(),
                _ => {
                    if self.filter.handle(key) {
                        self.following = true;
                    }
                }
            }
            return Vec::new();
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Vec::new();
        }
        let page = self.rows.get().max(1);
        let top = self.current_top();
        match key.code {
            KeyCode::Esc if !self.filter.value().is_empty() => self.clear_filter(),
            KeyCode::Esc => return vec![Cmd::Pop],
            KeyCode::Up | KeyCode::Char('k') => self.scroll_to(top.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_to(top.saturating_add(1)),
            KeyCode::PageUp => self.scroll_to(top.saturating_sub(page)),
            KeyCode::PageDown => self.scroll_to(top.saturating_add(page)),
            KeyCode::Char('g') | KeyCode::Home => {
                self.following = false;
                self.top = 0;
            }
            KeyCode::Char('G') | KeyCode::End => self.following = true,
            KeyCode::Char('f') => {
                // Pausing stays where the view is.
                self.top = top;
                self.following = !self.following;
            }
            KeyCode::Left => self.left = self.left.saturating_sub(SIDEWAYS),
            KeyCode::Right => self.left = self.left.saturating_add(SIDEWAYS),
            KeyCode::Char('/') => self.filtering = true,
            _ => {}
        }
        Vec::new()
    }

    /// The one-line summary above the log.
    fn summary(&self, matching: usize) -> String {
        let mut parts = Vec::new();
        match &self.state {
            State::Ended(None) => parts.push("log ended".to_owned()),
            State::Ended(Some(reason)) => parts.push(format!("log ended: {reason}")),
            State::Open | State::Failed(_) => {}
        }
        parts.push(format!("{} lines", grouped(self.lines.len())));
        parts.push(
            if self.following {
                "following"
            } else {
                "paused"
            }
            .to_owned(),
        );
        if self.dropped > 0 {
            parts.push(format!("{} earlier lines dropped", grouped(self.dropped)));
        }
        if !self.filter.value().is_empty() {
            parts.push(format!(
                "{} of {} match",
                grouped(matching),
                grouped(self.lines.len())
            ));
        }
        clean(&parts.join(" · "))
    }
}

/// `n` with a comma between every three digits.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

impl Screen for LogsTail {
    fn title(&self) -> String {
        if self.namespace.is_empty() {
            clean(&format!("Logs — {}", self.pod))
        } else {
            clean(&format!("Logs — {}/{}", self.namespace, self.pod))
        }
    }

    fn hints(&self) -> String {
        if self.filtering {
            "type to filter · enter keep · esc clear"
        } else {
            "f follow · ↑↓ pgup pgdn scroll · g top · G newest · ←→ sideways · / filter · esc back"
        }
        .to_owned()
    }

    fn init(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        let Some(kubectl) = self.kubectl.clone() else {
            return Vec::new();
        };
        let (namespace, pod) = (self.namespace.clone(), self.pod.clone());
        vec![Cmd::Stream(
            STREAM,
            Box::pin(async move {
                kubectl
                    .logs(&namespace, &pod, true)
                    .await
                    .map_err(|e| format!("{:#}", anyhow::Error::new(e)))
            }),
        )]
    }

    fn update(&mut self, msg: Msg, _ctx: &Ctx) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) => return self.on_key(key),
            Msg::Paste(text) => {
                // Text for an open filter, and nothing otherwise.
                if self.filtering {
                    self.filter.paste(&text);
                    self.following = true;
                }
            }
            Msg::Lines(STREAM, batch) => self.append(batch),
            Msg::StreamEnded(STREAM, reason) => {
                self.state = match reason {
                    // A stream that failed before its first line has nothing
                    // to show but the error.
                    Some(error) if self.lines.is_empty() => State::Failed(error),
                    Some(error) => State::Ended(Some(error)),
                    None => match self.only_error() {
                        Some(error) => State::Failed(error),
                        None => State::Ended(None),
                    },
                };
            }
            Msg::Lines(..)
            | Msg::StreamEnded(..)
            | Msg::Resize
            | Msg::Progress(_)
            | Msg::Warning(_)
            | Msg::Confirm(_)
            | Msg::RunDone(_)
            | Msg::Loaded(..) => {}
        }
        Vec::new()
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        if let State::Failed(error) = &self.state {
            render_error(frame, area, error);
            return;
        }
        let filtered = self.filtering || !self.filter.value().is_empty();
        let [summary, filter, body] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(filtered)),
            Constraint::Min(0),
        ])
        .areas(area);
        self.rows.set(usize::from(body.height));
        let shown = self.shown();
        frame.render_widget(
            Line::styled(self.summary(shown.len()), theme::dim()),
            summary,
        );
        if filtered {
            let [marker, field] =
                Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(filter);
            frame.render_widget(Line::from("/ "), marker);
            self.filter.render(frame, field, self.filtering);
        }
        if shown.is_empty() {
            let text = if self.lines.is_empty() {
                match self.state {
                    State::Open => "waiting for log lines…".to_owned(),
                    State::Ended(_) | State::Failed(_) => "the log is empty".to_owned(),
                }
            } else {
                clean(&format!("no line matches \"{}\"", self.filter.value()))
            };
            frame.render_widget(Line::styled(text, theme::dim()), body);
            return;
        }
        let width = usize::from(body.width);
        let rows: Vec<Line> = shown
            .iter()
            .skip(self.current_top())
            .take(usize::from(body.height))
            .map(|line| Line::from(slice_columns(line, self.left, width)))
            .collect();
        frame.render_widget(Paragraph::new(rows), body);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ss_core::svc::SvcError;
    use ss_core::svc::kubectl::KubectlPort;
    use tokio::sync::mpsc;

    use super::*;
    use crate::tui::shell::Shell;
    use crate::tui::testing::{ch, ctx, key, press, render_screen, render_shell, settle, typed};

    /// A port whose one log stream the test feeds by hand.
    struct Tail(Mutex<Option<Result<mpsc::Receiver<String>, String>>>);

    #[async_trait]
    impl KubectlPort for Tail {
        async fn get_json(&self, _: &str, _: &str, name: &str) -> Result<Vec<u8>, SvcError> {
            Err(SvcError::not_found(name.to_owned()))
        }

        async fn list_json(&self, _: &str, resource: &str) -> Result<Vec<u8>, SvcError> {
            Err(SvcError::not_found(resource.to_owned()))
        }

        async fn logs(
            &self,
            _namespace: &str,
            _pod: &str,
            _follow: bool,
        ) -> Result<mpsc::Receiver<String>, SvcError> {
            match self.0.lock().unwrap().take() {
                Some(Ok(lines)) => Ok(lines),
                Some(Err(e)) => Err(SvcError::Other(anyhow::anyhow!(e))),
                None => Err(SvcError::Other(anyhow::anyhow!(
                    "the log was already opened"
                ))),
            }
        }
    }

    /// A shell showing the log of `api-7d9f`, and the sender its lines come
    /// from.
    fn tail(capacity: usize) -> (Shell, mpsc::Sender<String>) {
        let (tx, rx) = mpsc::channel(capacity);
        let mut ctx = ctx();
        ctx.deps.kubectl = Some(Arc::new(Tail(Mutex::new(Some(Ok(rx))))));
        let screen = LogsTail::new("", "api-7d9f", &ctx);
        (Shell::new(ctx, Box::new(screen)), tx)
    }

    async fn feed(tx: &mpsc::Sender<String>, lines: impl IntoIterator<Item = String>) {
        for line in lines {
            tx.send(line).await.expect("the log is being read");
        }
    }

    fn numbered(range: std::ops::Range<usize>) -> impl Iterator<Item = String> {
        range.map(|i| format!("line {i:04}"))
    }

    /// The log rows on an 80x24 screen: everything between the title, the
    /// screen's own first row, and the two bottom lines.
    fn body(shell: &Shell) -> Vec<String> {
        let shown = render_shell(shell, 80, 24);
        let rows: Vec<&str> = shown.lines().collect();
        rows.get(2..rows.len().saturating_sub(2))
            .unwrap_or_default()
            .iter()
            .map(|row| (*row).to_owned())
            .filter(|row| !row.is_empty())
            .collect()
    }

    fn shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    #[tokio::test]
    async fn lines_appear_as_they_arrive_and_the_view_follows_the_newest() {
        let (mut shell, tx) = tail(64);
        feed(&tx, numbered(0..30)).await;
        let shown = settle(&mut shell, "line 0029").await;
        assert!(shown.contains("Logs — api-7d9f"), "{shown}");
        assert!(shown.contains("30 lines · following"), "{shown}");
        assert!(
            !shown.contains("line 0005"),
            "the oldest lines scrolled off: {shown}"
        );

        feed(&tx, numbered(30..31)).await;
        settle(&mut shell, "line 0030").await;

        assert_eq!(body(&shell).last().map(String::as_str), Some("line 0030"));
    }

    #[tokio::test]
    async fn scrolling_up_stops_following_and_new_lines_do_not_move_the_view() {
        let (mut shell, tx) = tail(64);
        feed(&tx, numbered(0..30)).await;
        settle(&mut shell, "line 0029").await;
        let before = body(&shell);

        press(&mut shell, &[key(KeyCode::Up)]);
        let scrolled = body(&shell);
        assert_ne!(scrolled, before);
        assert!(render_shell(&shell, 80, 24).contains("paused"));

        feed(&tx, numbered(30..33)).await;
        settle(&mut shell, "33 lines").await;

        assert_eq!(body(&shell), scrolled, "the view did not move");
    }

    #[tokio::test]
    async fn f_and_shift_g_return_to_the_newest_line() {
        let (mut shell, tx) = tail(64);
        feed(&tx, numbered(0..40)).await;
        settle(&mut shell, "line 0039").await;

        press(&mut shell, &[key(KeyCode::PageUp)]);
        assert!(!render_shell(&shell, 80, 24).contains("line 0039"));
        press(&mut shell, &[ch('f')]);
        let shown = render_shell(&shell, 80, 24);
        assert!(
            shown.contains("line 0039") && shown.contains("following"),
            "{shown}"
        );

        press(&mut shell, &[key(KeyCode::PageUp), shift('G')]);
        assert!(render_shell(&shell, 80, 24).contains("line 0039"));
    }

    #[tokio::test]
    async fn scrolling_down_to_the_newest_line_follows_again() {
        let (mut shell, tx) = tail(64);
        feed(&tx, numbered(0..40)).await;
        settle(&mut shell, "line 0039").await;

        press(&mut shell, &[key(KeyCode::Up), key(KeyCode::Up)]);
        assert!(render_shell(&shell, 80, 24).contains("paused"));
        press(&mut shell, &[key(KeyCode::Down), key(KeyCode::Down)]);

        assert!(render_shell(&shell, 80, 24).contains("following"));
    }

    #[tokio::test]
    async fn g_jumps_to_the_first_line_kept() {
        let (mut shell, tx) = tail(64);
        feed(&tx, numbered(0..40)).await;
        settle(&mut shell, "line 0039").await;

        press(&mut shell, &[ch('g')]);

        assert_eq!(body(&shell).first().map(String::as_str), Some("line 0000"));
    }

    #[tokio::test]
    async fn the_buffer_keeps_the_newest_lines_and_counts_the_dropped() {
        let (mut shell, tx) = tail(MAX_LINES + 100);
        feed(&tx, numbered(0..MAX_LINES + 10)).await;

        let shown = settle(&mut shell, "10 earlier lines dropped").await;

        assert!(shown.contains("5,000 lines"), "{shown}");
        assert!(
            shown.contains(&format!("line {:04}", MAX_LINES + 9)),
            "{shown}"
        );
        press(&mut shell, &[ch('g')]);
        assert_eq!(body(&shell).first().map(String::as_str), Some("line 0010"));
    }

    #[tokio::test]
    async fn the_view_holds_its_place_when_old_lines_are_dropped() {
        let (mut shell, tx) = tail(MAX_LINES + 100);
        feed(&tx, numbered(0..MAX_LINES)).await;
        settle(&mut shell, "5,000 lines").await;
        press(&mut shell, &[key(KeyCode::PageUp), key(KeyCode::PageUp)]);
        let held = body(&shell);

        feed(&tx, numbered(MAX_LINES..MAX_LINES + 7)).await;
        settle(&mut shell, "7 earlier lines dropped").await;
        assert_eq!(body(&shell), held, "the same lines are still on screen");

        // At the very top, the oldest line kept takes the place of the one
        // that was dropped.
        press(&mut shell, &[ch('g')]);
        assert_eq!(body(&shell).first().map(String::as_str), Some("line 0007"));
        feed(&tx, numbered(MAX_LINES + 7..MAX_LINES + 9)).await;
        settle(&mut shell, "9 earlier lines dropped").await;
        assert_eq!(body(&shell).first().map(String::as_str), Some("line 0009"));
    }

    #[tokio::test]
    async fn colour_codes_are_removed_and_other_escapes_are_inert() {
        let (mut shell, tx) = tail(8);
        feed(
            &tx,
            [
                "\x1b[31mERROR\x1b[0m failed to connect".to_owned(),
                "tab\there \x1b]0;title\x07done\x07".to_owned(),
            ],
        )
        .await;

        let shown = settle(&mut shell, "failed to connect").await;

        assert!(shown.contains("ERROR failed to connect"), "{shown}");
        assert!(shown.contains("tab     here done"), "{shown}");
        assert!(
            !shown.contains("[31m") && !shown.contains('\x1b'),
            "{shown:?}"
        );
    }

    #[tokio::test]
    async fn slash_filters_lines_and_shows_how_many_match() {
        let (mut shell, tx) = tail(64);
        feed(
            &tx,
            (0..20).map(|i| {
                if i % 5 == 0 {
                    format!("ERROR step {i}")
                } else {
                    format!("info step {i}")
                }
            }),
        )
        .await;
        settle(&mut shell, "info step 19").await;

        press(&mut shell, &[ch('/')]);
        press(&mut shell, &typed("error"));

        let shown = render_shell(&shell, 80, 24);
        assert!(shown.contains("/ error"), "{shown}");
        assert!(shown.contains("4 of 20 match"), "{shown}");
        assert_eq!(
            body(&shell)
                .iter()
                .filter(|row| row.contains("step"))
                .count(),
            4,
            "{shown}"
        );
        assert!(!shown.contains("info step"), "{shown}");
    }

    #[tokio::test]
    async fn letters_typed_into_the_filter_are_not_commands_and_esc_clears_it() {
        let (mut shell, tx) = tail(64);
        feed(&tx, numbered(0..5)).await;
        settle(&mut shell, "line 0004").await;

        press(&mut shell, &[ch('/')]);
        press(&mut shell, &typed("fg"));
        let shown = render_shell(&shell, 80, 24);
        assert!(
            shown.contains("/ fg") && shown.contains("following"),
            "{shown}"
        );
        assert_eq!(shell.depth(), 1);

        press(&mut shell, &[key(KeyCode::Esc)]);
        let shown = render_shell(&shell, 80, 24);
        assert!(
            !shown.contains("/ fg") && shown.contains("line 0004"),
            "{shown}"
        );
        assert_eq!(shell.depth(), 1, "esc cleared the filter and went nowhere");
    }

    #[tokio::test]
    async fn a_paste_goes_into_the_filter_only_while_it_is_open() {
        let (mut shell, tx) = tail(64);
        feed(&tx, numbered(0..5)).await;
        settle(&mut shell, "line 0004").await;

        shell.handle(None, Msg::Paste("f\n".to_owned()));
        assert!(render_shell(&shell, 80, 24).contains("following"));

        press(&mut shell, &[ch('/')]);
        shell.handle(None, Msg::Paste("0003\n".to_owned()));

        let shown = render_shell(&shell, 80, 24);
        assert!(
            shown.contains("/ 0003") && shown.contains("1 of 5 match"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_long_line_scrolls_sideways() {
        let (mut shell, tx) = tail(8);
        let long = format!("{}END", "0123456789".repeat(10));
        feed(&tx, [long]).await;
        let shown = settle(&mut shell, "0123456789").await;
        assert!(!shown.contains("END"), "{shown}");

        press(&mut shell, &[key(KeyCode::Right); 10]);

        assert!(render_shell(&shell, 80, 24).contains("END"));
        press(&mut shell, &[key(KeyCode::Left); 40]);
        assert!(body(&shell).iter().any(|row| row.starts_with("0123456789")));
    }

    #[tokio::test]
    async fn a_stream_that_ends_says_the_log_ended() {
        let (mut shell, tx) = tail(8);
        feed(&tx, numbered(0..2)).await;
        drop(tx);

        let shown = settle(&mut shell, "log ended").await;

        assert!(shown.contains("line 0001"), "{shown}");
    }

    #[tokio::test]
    async fn a_stream_that_cannot_open_shows_the_error_and_esc_leaves() {
        let mut ctx = ctx();
        ctx.deps.kubectl = Some(Arc::new(Tail(Mutex::new(Some(Err(
            "kubectl not found at \"kubectl\" (install kubectl or put it on PATH)".to_owned(),
        ))))));
        let mut screen = LogsTail::new("orders", "api-7d9f", &ctx);
        assert_eq!(screen.title(), "Logs — orders/api-7d9f");
        let mut shell = Shell::new(ctx.clone(), Box::new(screen));
        let shown = settle(&mut shell, "kubectl not found").await;
        assert!(shown.contains("┌ error"), "{shown}");

        screen = LogsTail::new("orders", "api-7d9f", &ctx);
        screen.init(&ctx);
        let cmds = screen.update(Msg::Key(key(KeyCode::Esc)), &ctx);
        assert!(matches!(cmds.as_slice(), [Cmd::Pop]));
    }

    #[tokio::test]
    async fn kubernetes_not_available_shows_the_error() {
        let ctx = ctx();
        let mut screen = LogsTail::new("", "api-7d9f", &ctx);

        assert!(screen.init(&ctx).is_empty());

        assert!(render_screen(&screen, 60, 6).contains("Kubernetes is not available"));
    }

    #[tokio::test]
    async fn lines_of_another_stream_are_ignored() {
        let (mut shell, tx) = tail(8);
        feed(&tx, numbered(0..1)).await;
        settle(&mut shell, "line 0000").await;

        shell.handle(None, Msg::Lines(99, vec!["from elsewhere".to_owned()]));

        let shown = render_shell(&shell, 80, 24);
        assert!(shown.contains("1 lines"), "{shown}");
        assert!(!shown.contains("from elsewhere"), "{shown}");
    }

    #[tokio::test]
    async fn a_log_that_is_only_an_error_from_kubectl_shows_the_error() {
        let (mut shell, tx) = tail(8);
        feed(
            &tx,
            ["error: Error from server (NotFound): pods \"api-7d9f\" not found".to_owned()],
        )
        .await;
        drop(tx);

        let shown = settle(&mut shell, "┌ error").await;

        assert!(
            shown.contains("│Error from server (NotFound): pods \"api-7d9f\" not found"),
            "{shown}"
        );
        assert!(!shown.contains("following"), "{shown}");
    }

    #[tokio::test]
    async fn an_error_after_other_lines_stays_in_the_log() {
        let (mut shell, tx) = tail(8);
        feed(
            &tx,
            ["line 0000".to_owned(), "error: unexpected EOF".to_owned()],
        )
        .await;
        drop(tx);

        let shown = settle(&mut shell, "log ended").await;

        assert!(
            shown.contains("line 0000") && shown.contains("error: unexpected EOF"),
            "{shown}"
        );
        assert!(!shown.contains("┌ error"), "{shown}");
    }

    #[tokio::test]
    async fn long_lines_are_dropped_by_size_before_the_line_limit() {
        let (mut shell, tx) = tail(16);
        let size = MAX_BYTES / 8;
        feed(
            &tx,
            (0..12).map(|i| format!("{i:02}{}", "x".repeat(size - 2))),
        )
        .await;

        let shown = settle(&mut shell, "4 earlier lines dropped").await;

        assert!(
            shown.lines().any(|row| row.starts_with("8 lines ·")),
            "{shown}"
        );
        press(&mut shell, &[ch('g')]);
        assert!(
            body(&shell)
                .first()
                .is_some_and(|row| row.starts_with("04x")),
            "the oldest line kept is the fifth"
        );
    }

    #[tokio::test]
    async fn an_empty_log_says_it_is_waiting() {
        let (shell, _tx) = tail(8);

        assert!(render_shell(&shell, 80, 24).contains("waiting for log lines"));
    }

    #[test]
    fn a_tiny_area_draws_without_panicking() {
        let (_tx, rx) = mpsc::channel(1);
        let mut ctx = ctx();
        ctx.deps.kubectl = Some(Arc::new(Tail(Mutex::new(Some(Ok(rx))))));
        let mut screen = LogsTail::new("", "api-7d9f", &ctx);
        screen.update(
            Msg::Lines(STREAM, vec!["a line of the log".to_owned()]),
            &ctx,
        );
        screen.update(Msg::Key(ch('/')), &ctx);
        assert!(render_screen(&screen, 80, 6).contains("a line of the log"));

        for (width, height) in [(1, 1), (0, 0), (3, 2), (80, 1), (1, 6)] {
            render_screen(&screen, width, height);
        }
    }
}
