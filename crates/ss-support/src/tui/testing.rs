//! Helpers shared by the TUI's unit tests: rendering to text, key
//! construction, a default context, and a scriptable screen.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::{Frame, Terminal};
use ss_core::audit;
use ss_core::guard::AllowAll;
use ss_core::ops::{Deps, Registry};
use ss_core::preset;
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

use super::screen::{Cmd, Ctx, Msg, RunFlags, Screen};
use super::shell::Shell;

/// The rows of `buf` as text, right-trimmed, one line per row. Cells hidden
/// behind a wide character are skipped.
pub(crate) fn buffer_text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let mut line = String::new();
        let mut hidden = 0usize;
        for x in area.left()..area.right() {
            if hidden > 0 {
                hidden -= 1;
                continue;
            }
            let symbol = buf[(x, y)].symbol();
            line.push_str(symbol);
            hidden = symbol.width().saturating_sub(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// What `term` currently shows, as text.
pub(crate) fn screen_text(term: &Terminal<TestBackend>) -> String {
    buffer_text(term.backend().buffer())
}

/// Draws with `f` on a `width` x `height` terminal and returns the text.
pub(crate) fn render_with(width: u16, height: u16, f: impl FnOnce(&mut Frame)) -> String {
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    term.draw(f).expect("draw");
    screen_text(&term)
}

/// The whole shell on a `width` x `height` terminal.
pub(crate) fn render_shell(shell: &Shell, width: u16, height: u16) -> String {
    render_with(width, height, |frame| shell.draw(frame))
}

/// One screen's body on a `width` x `height` terminal.
pub(crate) fn render_screen(screen: &dyn Screen, width: u16, height: u16) -> String {
    render_with(width, height, |frame| {
        screen.view(frame, Rect::new(0, 0, width, height));
    })
}

/// Sends `keys` to the shell's top screen, one after another.
pub(crate) fn press(shell: &mut Shell, keys: &[KeyEvent]) {
    for &k in keys {
        shell.handle(None, Msg::Key(k));
    }
}

/// Delivers background results until the 80x24 screen contains `text`, and
/// returns that screen.
///
/// # Panics
///
/// When the text has not appeared after five seconds.
pub(crate) async fn settle(shell: &mut Shell, text: &str) -> String {
    let deadline = std::time::Duration::from_secs(5);
    loop {
        let shown = render_shell(shell, 80, 24);
        if shown.contains(text) {
            return shown;
        }
        if tokio::time::timeout(deadline, shell.pump()).await.is_err() {
            panic!("{text:?} never appeared; the screen shows:\n{shown}");
        }
    }
}

/// A key press without modifiers.
pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A character key press.
pub(crate) fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

/// A character key press with Ctrl held.
pub(crate) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// The key presses that type `text`.
pub(crate) fn typed(text: &str) -> Vec<KeyEvent> {
    text.chars().map(ch).collect()
}

/// A fixed instant for deterministic file names and reports.
pub(crate) fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
        .single()
        .expect("valid instant")
}

/// A context with an empty registry, no ports, a permissive guard, a
/// discarded audit trail, no saved presets, and an environment named `dev`.
pub(crate) fn ctx() -> Ctx {
    let mut deps = Deps::default();
    deps.env.name = "dev".to_owned();
    Ctx {
        registry: Registry::new(),
        deps,
        guard: Arc::new(AllowAll),
        audit: Arc::new(audit::Nop),
        presets: preset::Store::new("/nonexistent/ss-support-tests/presets.yaml"),
        user: "tester".to_owned(),
        cancel: CancellationToken::new(),
        flags: RunFlags::default(),
        export_dir: PathBuf::from("/nonexistent/ss-support-tests"),
        now: fixed_now,
    }
}

/// What a [`Probe`] has been told, oldest first.
pub(crate) type Log = Arc<Mutex<Vec<String>>>;

/// The entries of `log`.
pub(crate) fn entries(log: &Log) -> Vec<String> {
    log.lock().expect("log").clone()
}

/// What a [`Probe`] returns when it is placed on the stack.
type InitScript = Box<dyn FnOnce() -> Vec<Cmd> + Send>;
/// What a [`Probe`] returns for each message.
type MsgScript = Box<dyn FnMut(&Msg) -> Vec<Cmd> + Send>;

/// A screen that records every message as `<name>:<event>` and answers with
/// whatever its script returns.
pub(crate) struct Probe {
    name: &'static str,
    log: Log,
    init: Option<InitScript>,
    script: MsgScript,
}

impl Probe {
    /// A probe that answers every message with no commands.
    pub(crate) fn new(name: &'static str, log: &Log) -> Probe {
        Probe {
            name,
            log: Arc::clone(log),
            init: None,
            script: Box::new(|_| Vec::new()),
        }
    }

    /// The commands returned when the screen is placed on the stack.
    pub(crate) fn on_init(mut self, f: impl FnOnce() -> Vec<Cmd> + Send + 'static) -> Probe {
        self.init = Some(Box::new(f));
        self
    }

    /// The commands returned for each message.
    pub(crate) fn on_msg(mut self, f: impl FnMut(&Msg) -> Vec<Cmd> + Send + 'static) -> Probe {
        self.script = Box::new(f);
        self
    }

    fn note(&self, event: &str) {
        self.log
            .lock()
            .expect("log")
            .push(format!("{}:{event}", self.name));
    }
}

impl Screen for Probe {
    fn title(&self) -> String {
        self.name.to_owned()
    }

    fn hints(&self) -> String {
        format!("{} hints", self.name)
    }

    fn init(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        self.note("init");
        self.init.take().map(|f| f()).unwrap_or_default()
    }

    fn update(&mut self, msg: Msg, _ctx: &Ctx) -> Vec<Cmd> {
        let event = match &msg {
            Msg::Key(k) => match k.code {
                KeyCode::Char(c) => format!("key:{c}"),
                other => format!("key:{other:?}"),
            },
            Msg::Resize(w, h) => format!("resize:{w}x{h}"),
            Msg::Progress(p) => format!("progress:{}", p.step),
            Msg::Warning(w) => format!("warning:{w}"),
            Msg::Confirm(c) => format!("confirm:{}", c.spec.op_id),
            Msg::RunDone(r) => format!("done:{}", if r.outcome.is_ok() { "ok" } else { "err" }),
            Msg::Loaded(id, _) => format!("loaded:{id}"),
        };
        self.note(&event);
        (self.script)(&msg)
    }

    fn view(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Line::from(format!("{} body", self.name)), area);
    }
}

/// True when `msg` is the character key `c` without modifiers.
pub(crate) fn is_char(msg: &Msg, c: char) -> bool {
    matches!(msg, Msg::Key(k) if k.code == KeyCode::Char(c) && k.modifiers == KeyModifiers::NONE)
}
