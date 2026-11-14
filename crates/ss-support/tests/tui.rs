//! End-to-end tests of the TUI: the whole event loop runs in-process on an
//! in-memory terminal, fed by scripted keys, against an isolated home.

use std::convert::Infallible;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use futures_util::stream;
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Size};
use ss_core::guard::Class;
use ss_core::ops::{Input, Meta, OpResult, Operation, Param, Runner, View};
use ss_core::svc::s3::FakeS3;
use ss_support::{Exit, Services};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthStr;

const PATIENCE: Duration = Duration::from_secs(5);

/// A test terminal the test can read while the TUI draws on it.
#[derive(Clone)]
struct Screen(Arc<Mutex<TestBackend>>);

impl Screen {
    fn new(width: u16, height: u16) -> Self {
        Screen(Arc::new(Mutex::new(TestBackend::new(width, height))))
    }

    /// What is on screen, one right-trimmed line per row.
    fn text(&self) -> String {
        let backend = self.0.lock().unwrap();
        let buffer = backend.buffer();
        let area = buffer.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            let mut line = String::new();
            let mut hidden = 0;
            for x in area.left()..area.right() {
                if hidden > 0 {
                    hidden -= 1;
                    continue;
                }
                let symbol = buffer[(x, y)].symbol();
                line.push_str(symbol);
                hidden = symbol.width().saturating_sub(1);
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    fn resize(&self, width: u16, height: u16) {
        self.0.lock().unwrap().resize(width, height);
    }
}

impl Backend for Screen {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.0.lock().unwrap().draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.0.lock().unwrap().get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.0.lock().unwrap().set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.0.lock().unwrap().clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.0.lock().unwrap().size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.0.lock().unwrap().window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().flush()
    }
}

/// A running TUI and the means to drive and observe it.
struct Tui {
    events: mpsc::UnboundedSender<io::Result<Event>>,
    screen: Screen,
    interrupt: Arc<Notify>,
    exit: JoinHandle<Exit>,
}

impl Tui {
    /// Starts `ss-support` plus `flags` on an 80x24 terminal.
    fn start(services: Services, flags: &[&str]) -> Tui {
        Tui::start_sized(services, flags, 80, 24)
    }

    fn start_sized(services: Services, flags: &[&str], width: u16, height: u16) -> Tui {
        let args = ["ss-support"].into_iter().chain(flags.iter().copied());
        let cli = ss_support::parse_from(&services.registry, args).expect("parse");
        let (events, inbox) = mpsc::unbounded_channel();
        let keys = Box::pin(stream::unfold(inbox, |mut inbox| async move {
            inbox.recv().await.map(|event| (event, inbox))
        }));
        let screen = Screen::new(width, height);
        let interrupt = Arc::new(Notify::new());
        let exit = tokio::spawn({
            let backend = screen.clone();
            let interrupt = Arc::clone(&interrupt);
            async move {
                let mut term = Terminal::new(backend).expect("terminal");
                let interrupted = async move { interrupt.notified().await };
                ss_support::run_tui_with(cli, services, &mut term, keys, interrupted).await
            }
        });
        Tui {
            events,
            screen,
            interrupt,
            exit,
        }
    }

    fn key(&self, code: KeyCode) {
        self.send(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl(&self, c: char) {
        self.send(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c));
        }
    }

    fn send(&self, key: KeyEvent) {
        self.events
            .send(Ok(Event::Key(key)))
            .expect("the TUI is running");
    }

    /// Waits until the screen contains `text` and returns the screen.
    async fn wait_for(&self, text: &str) -> String {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let shown = self.screen.text();
            if shown.contains(text) {
                return shown;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{text:?} never appeared; the screen shows:\n{shown}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until keys are taken again after a question or a result has
    /// appeared.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    fn paste(&self, text: &str) {
        self.events
            .send(Ok(Event::Paste(text.to_owned())))
            .expect("the TUI is running");
    }

    /// Opens the form of the operation `id` from the picker.
    async fn open(&self, id: &str) {
        self.wait_for("Operations").await;
        self.key(KeyCode::Char('/'));
        self.type_text(id);
        self.wait_for(&format!("> {id}")).await;
        self.key(KeyCode::Enter);
    }

    /// Waits for the TUI to end and returns its exit status.
    async fn exit(self) -> Exit {
        tokio::time::timeout(PATIENCE, self.exit)
            .await
            .expect("the TUI ends in time")
            .expect("the TUI does not panic")
    }
}

fn home() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Makes `dev` the default environment and allows writes in it.
fn allow_dev(root: &Path) {
    let path = root.join(".config/ss-support/config.yaml");
    fs::create_dir_all(path.parent().expect("has a parent")).expect("create dirs");
    fs::write(
        path,
        "default_env: dev\nenvironments:\n  dev: {profile: test, region: us-west-2}\n",
    )
    .expect("write config");
}

/// A writing operation: previews, names `orders-dev`, then sets `done` in
/// its one mutating step.
struct WriteOp {
    class: Class,
    done: Arc<AtomicBool>,
}

#[async_trait]
impl Operation for WriteOp {
    fn meta(&self) -> Meta {
        Meta {
            id: "stub.write",
            name: "Change a table",
            summary: "changes one table",
            category: "Stub",
            class: self.class,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![Param::new("target").label("Target").required(true)]
    }

    async fn run(&self, r: &mut Runner, _input: &Input) -> anyhow::Result<OpResult> {
        r.preview([View::summary([("Will change", "orders-dev")])]);
        r.set_resource("orders-dev");
        let done = Arc::clone(&self.done);
        r.mutate("apply", |_| async move {
            done.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await?;
        Ok(OpResult {
            views: vec![View::summary([("Changed", "orders-dev")])],
        })
    }
}

/// An operation whose one step runs until the run is cancelled.
struct HangOp;

#[async_trait]
impl Operation for HangOp {
    fn meta(&self) -> Meta {
        Meta {
            id: "stub.hang",
            name: "Wait forever",
            summary: "never finishes",
            category: "Stub",
            class: Class::Read,
        }
    }

    fn params(&self) -> Vec<Param> {
        Vec::new()
    }

    async fn run(&self, r: &mut Runner, _input: &Input) -> anyhow::Result<OpResult> {
        r.step("wait", |token| async move {
            token.cancelled().await;
            Ok(())
        })
        .await?;
        Ok(OpResult::default())
    }
}

/// Isolated services with a write operation of `class` registered.
fn with_write(root: &Path, class: Class) -> (Services, Arc<AtomicBool>) {
    let done = Arc::new(AtomicBool::new(false));
    let mut services = Services::isolated(root);
    services
        .registry
        .register(Arc::new(WriteOp {
            class,
            done: Arc::clone(&done),
        }))
        .expect("register");
    (services, done)
}

fn with_hang(root: &Path) -> Services {
    let mut services = Services::isolated(root);
    services
        .registry
        .register(Arc::new(HangOp))
        .expect("register");
    services
}

#[tokio::test]
async fn picker_to_form_to_result_shows_the_greeting() {
    let root = home();
    let tui = Tui::start(Services::isolated(root.path()), &[]);

    tui.open("demo.hello").await;
    tui.wait_for("Who to greet").await;
    tui.type_text("operator");
    tui.key(KeyCode::Enter);
    let shown = tui.wait_for("view 1/").await;
    tui.settle().await;

    assert!(shown.contains("operator"), "{shown}");
    tui.key(KeyCode::Esc);
    let shown = tui.wait_for("Who to greet").await;
    assert!(
        shown.contains("operator"),
        "the form kept its value: {shown}"
    );
}

#[tokio::test]
async fn the_filter_finds_an_operation_by_fuzzy_name() {
    let root = home();
    let tui = Tui::start(Services::isolated(root.path()), &[]);
    tui.wait_for("Operations").await;

    tui.key(KeyCode::Char('/'));
    tui.type_text("vdiff");

    tui.wait_for("> s3.version-diff").await;
}

#[tokio::test]
async fn a_destructive_operation_waits_for_the_typed_name_then_runs() {
    let root = home();
    allow_dev(root.path());
    let (services, done) = with_write(root.path(), Class::Destructive);
    let tui = Tui::start(services, &[]);

    tui.open("stub.write").await;
    tui.wait_for("Target").await;
    tui.type_text("x");
    tui.key(KeyCode::Enter);
    let shown = tui.wait_for("This is DESTRUCTIVE").await;
    tui.settle().await;
    assert!(shown.contains("Will change : orders-dev"), "{shown}");

    tui.type_text("orders");
    tui.key(KeyCode::Enter);
    tui.wait_for("name does not match").await;
    assert!(!done.load(Ordering::SeqCst));

    tui.type_text("-dev");
    tui.key(KeyCode::Enter);
    let shown = tui.wait_for("view 1/1").await;

    assert!(shown.contains("Changed : orders-dev"), "{shown}");
    assert!(done.load(Ordering::SeqCst));
}

#[tokio::test]
async fn yes_is_ignored_and_a_mutating_run_still_asks() {
    let root = home();
    allow_dev(root.path());
    let (services, done) = with_write(root.path(), Class::Mutate);
    let tui = Tui::start(services, &["--yes"]);

    tui.open("stub.write").await;
    tui.wait_for("Target").await;
    tui.type_text("x");
    tui.key(KeyCode::Enter);

    tui.wait_for("Proceed with stub.write?").await;
    tui.settle().await;
    assert!(!done.load(Ordering::SeqCst));
}

#[tokio::test]
async fn keys_typed_ahead_of_the_question_do_not_answer_it() {
    let root = home();
    allow_dev(root.path());
    let (services, done) = with_write(root.path(), Class::Mutate);
    let tui = Tui::start(services, &[]);
    tui.open("stub.write").await;
    tui.wait_for("Target").await;

    // Everything at once, as a paste without paste marks or fast typing
    // would deliver it: the value, enter to run, then "y" and enter.
    tui.type_text("orders");
    tui.key(KeyCode::Enter);
    tui.key(KeyCode::Char('y'));
    tui.key(KeyCode::Enter);

    let shown = tui.wait_for("Proceed with stub.write?").await;
    tui.settle().await;
    assert!(!done.load(Ordering::SeqCst), "{shown}");
    assert!(tui.screen.text().contains("Proceed with stub.write?"));
}

#[tokio::test]
async fn a_pasted_line_fills_the_field_and_runs_nothing() {
    let root = home();
    let tui = Tui::start(Services::isolated(root.path()), &[]);
    tui.open("demo.hello").await;
    tui.wait_for("Who to greet").await;

    tui.paste("operator\n");
    let shown = tui.wait_for("operator").await;

    assert!(shown.contains("Hello (demo)"), "still on the form: {shown}");
    assert!(!shown.contains("running"), "{shown}");
    tui.key(KeyCode::Enter);
    tui.wait_for("Hello, operator!").await;
}

#[tokio::test]
async fn declining_the_confirmation_returns_to_the_form_with_the_decline_shown() {
    let root = home();
    allow_dev(root.path());
    let (services, done) = with_write(root.path(), Class::Mutate);
    let tui = Tui::start(services, &[]);

    tui.open("stub.write").await;
    tui.wait_for("Target").await;
    tui.type_text("orders");
    tui.key(KeyCode::Enter);
    tui.wait_for("Proceed with stub.write?").await;
    tui.settle().await;
    tui.key(KeyCode::Char('n'));
    tui.key(KeyCode::Enter);
    let shown = tui.wait_for("Change a table — declined").await;
    tui.settle().await;
    assert!(shown.contains("operation not confirmed"), "{shown}");

    tui.key(KeyCode::Esc);
    let shown = tui.wait_for("> Target *  orders").await;

    assert!(!shown.contains("declined"), "{shown}");
    assert!(!done.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_write_outside_the_allowlist_is_refused_and_shown() {
    let root = home();
    let (services, done) = with_write(root.path(), Class::Mutate);
    let tui = Tui::start(services, &[]);

    tui.open("stub.write").await;
    tui.wait_for("Target").await;
    tui.type_text("x");
    tui.key(KeyCode::Enter);
    let shown = tui.wait_for("Change a table — refused").await;

    assert!(
        !shown.contains("Proceed with"),
        "nothing was asked: {shown}"
    );
    assert!(!done.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_dry_run_changes_nothing_and_says_so() {
    let root = home();
    allow_dev(root.path());
    let (services, done) = with_write(root.path(), Class::Mutate);
    let tui = Tui::start(services, &["--dry-run"]);

    tui.open("stub.write").await;
    tui.wait_for("Change a table (dry run)").await;
    tui.type_text("x");
    tui.key(KeyCode::Enter);
    let shown = tui.wait_for("view 1/1").await;

    assert!(
        shown.contains("dry-run: stopped before the first mutating step"),
        "{shown}"
    );
    assert!(!done.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_run_from_the_tui_appends_one_audit_entry() {
    let root = home();
    allow_dev(root.path());
    let (services, _done) = with_write(root.path(), Class::Mutate);
    let tui = Tui::start(services, &[]);

    tui.open("stub.write").await;
    tui.wait_for("Target").await;
    tui.type_text("orders");
    tui.key(KeyCode::Enter);
    tui.wait_for("Proceed with stub.write?").await;
    tui.settle().await;
    tui.key(KeyCode::Char('y'));
    tui.key(KeyCode::Enter);
    tui.wait_for("view 1/1").await;

    let log = fs::read_to_string(root.path().join(".local/state/ss-support/audit.log"))
        .expect("the audit log exists");
    let entries: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
        .collect();
    assert_eq!(entries.len(), 1, "{log}");
    assert_eq!(entries[0]["op"], "stub.write");
    assert_eq!(entries[0]["outcome"], "ok");
    assert_eq!(entries[0]["env"], "dev");
    assert_eq!(entries[0]["user"], "tester");
    assert_eq!(entries[0]["params"]["target"], "orders");
}

#[tokio::test]
async fn ctrl_c_during_a_run_cancels_it_and_the_shell_keeps_running() {
    let root = home();
    let tui = Tui::start(with_hang(root.path()), &[]);

    tui.open("stub.hang").await;
    tui.wait_for("Wait forever").await;
    tui.key(KeyCode::Enter);
    tui.wait_for("→ wait").await;
    tui.ctrl('c');
    tui.wait_for("Wait forever — cancelled").await;
    tui.settle().await;

    // Back to the form, back to the picker, then out.
    tui.key(KeyCode::Esc);
    tui.key(KeyCode::Esc);
    tui.wait_for("Operations").await;
    tui.key(KeyCode::Esc);
    tui.key(KeyCode::Char('q'));
    assert_eq!(tui.exit().await, Exit::Ok);
}

#[tokio::test]
async fn ctrl_c_on_the_picker_exits_ok() {
    let root = home();
    let tui = Tui::start(Services::isolated(root.path()), &[]);
    tui.wait_for("Operations").await;

    tui.ctrl('c');

    assert_eq!(tui.exit().await, Exit::Ok);
}

#[tokio::test]
async fn the_browser_marks_two_versions_and_diffs_them() {
    let root = home();
    let at = |hour| {
        Utc.with_ymd_and_hms(2026, 1, 2, hour, 0, 0)
            .single()
            .expect("valid")
    };
    let fake = FakeS3::new();
    fake.insert("config-dev", "orders.json", "v1", r#"{"limit":100}"#, at(1));
    fake.insert("config-dev", "orders.json", "v2", r#"{"limit":250}"#, at(2));
    let services = Services::isolated(root.path()).with_s3(Arc::new(fake));
    let tui = Tui::start(services, &[]);
    tui.wait_for("Operations").await;

    tui.key(KeyCode::Char('b'));
    tui.wait_for("> config-dev").await;
    tui.key(KeyCode::Enter);
    tui.wait_for("> orders.json").await;
    tui.key(KeyCode::Enter);
    tui.wait_for("[ ] v1").await;
    tui.key(KeyCode::Char(' '));
    tui.key(KeyCode::Down);
    tui.key(KeyCode::Char(' '));
    tui.wait_for("[x] v1").await;
    tui.key(KeyCode::Char('d'));
    tui.wait_for("Compare object versions").await;
    tui.key(KeyCode::Enter);
    tui.wait_for("view 1/2").await;
    tui.settle().await;
    tui.key(KeyCode::Tab);
    let shown = tui.wait_for("view 2/2").await;

    assert!(shown.contains("limit"), "{shown}");
    assert!(shown.contains("100") && shown.contains("250"), "{shown}");
}

#[tokio::test]
async fn an_unconfigured_s3_shows_the_error_and_esc_returns_to_the_picker() {
    let root = home();
    let tui = Tui::start(Services::isolated(root.path()), &[]);
    tui.wait_for("Operations").await;

    tui.key(KeyCode::Char('b'));
    tui.wait_for("S3 is not configured").await;
    tui.key(KeyCode::Esc);

    tui.wait_for("Operations").await;
}

#[tokio::test]
async fn resizing_during_a_run_keeps_the_progress_list() {
    let root = home();
    let tui = Tui::start(with_hang(root.path()), &[]);
    tui.open("stub.hang").await;
    tui.wait_for("Wait forever").await;
    tui.key(KeyCode::Enter);
    tui.wait_for("→ wait").await;

    tui.screen.resize(100, 30);
    tui.events
        .send(Ok(Event::Resize(100, 30)))
        .expect("running");
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tui.screen.text().lines().count() != 30 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never redrawn at the new size"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let shown = tui.screen.text();
    assert!(shown.contains("→ wait"), "{shown}");
    assert!(shown.contains("Wait forever — running"), "{shown}");
}

#[tokio::test]
async fn a_terminal_too_small_to_draw_on_says_so() {
    let root = home();
    let tui = Tui::start_sized(Services::isolated(root.path()), &[], 19, 5);

    let shown = tui.wait_for("too small").await;

    assert!(!shown.contains("Operations"), "{shown}");
}

#[tokio::test]
async fn cancelling_the_invocation_exits_interrupted() {
    let root = home();
    let tui = Tui::start(with_hang(root.path()), &[]);
    tui.open("stub.hang").await;
    tui.wait_for("Wait forever").await;
    tui.key(KeyCode::Enter);
    tui.wait_for("→ wait").await;

    tui.interrupt.notify_one();

    assert_eq!(tui.exit().await, Exit::Interrupted);
}

#[tokio::test]
async fn the_event_stream_ending_exits_ok() {
    let root = home();
    let Tui {
        events,
        exit,
        screen,
        ..
    } = Tui::start(Services::isolated(root.path()), &[]);
    let _keep = screen;

    drop(events);

    let exit = tokio::time::timeout(PATIENCE, exit).await.unwrap().unwrap();
    assert_eq!(exit, Exit::Ok);
}

#[tokio::test]
async fn an_unparsable_config_exits_2_without_starting_the_tui() {
    let root = home();
    let path = root.path().join(".config/ss-support/config.yaml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "envs: {}\n").unwrap();
    let tui = Tui::start(Services::isolated(root.path()), &[]);

    let screen = tui.screen.clone();
    assert_eq!(tui.exit().await, Exit::Usage);
    assert_eq!(screen.text().trim(), "", "nothing was drawn");
}
