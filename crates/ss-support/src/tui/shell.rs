//! The shell: a stack of screens, the event loop that feeds them, and the
//! title, status, and hint lines drawn around the top screen.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::future::join_all;
use futures_util::{Stream, StreamExt};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::screen::{Cmd, Ctx, LineStream, Msg, Screen, ScreenId};
use super::{bridge, theme};

/// The smallest terminal the screens are drawn on.
const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 6;

/// How long leaving the TUI waits for runs still in flight to stop and
/// write their audit entries.
pub const RUN_GRACE: Duration = Duration::from_secs(5);

/// The most lines of a stream delivered to a screen at once.
pub const STREAM_BATCH: usize = 256;

/// The most background results delivered between two draws.
const RESULTS_PER_DRAW: usize = 64;

/// The least time between two draws when nothing but lines of a stream has
/// arrived in between. Everything else is drawn at once.
pub const LINES_REDRAW: Duration = Duration::from_millis(100);

/// Why the event loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The operator left the TUI.
    Quit,
    /// The invocation was cancelled from outside.
    Interrupted,
}

/// One stream a screen has open.
struct OpenStream {
    number: u64,
    /// Each permit lets the forwarder read one more batch from the source.
    credit: Arc<Semaphore>,
    forwarder: AbortHandle,
}

/// One screen on the stack, the runs it started, the loads it asked for,
/// and the streams it opened.
struct Entry {
    id: ScreenId,
    screen: Box<dyn Screen>,
    runs: Vec<CancellationToken>,
    loads: Vec<AbortHandle>,
    streams: Vec<OpenStream>,
}

impl Drop for Entry {
    /// No run, load, or stream outlives the screen that shows it.
    fn drop(&mut self) {
        for run in &self.runs {
            run.cancel();
        }
        for load in &self.loads {
            load.abort();
        }
        for stream in &self.streams {
            // Ending the forwarder drops the receiver, which stops the
            // source.
            stream.forwarder.abort();
        }
    }
}

/// Reads the stream `open` resolves to and hands its lines to `results` for
/// the screen `screen`, one batch at a time: the next batch is read only
/// once a permit says the previous one has been delivered, so a source that
/// produces faster than the screen takes is held back rather than buffered.
async fn forward(
    screen: ScreenId,
    number: u64,
    open: LineStream,
    credit: Arc<Semaphore>,
    results: mpsc::UnboundedSender<(ScreenId, Msg)>,
) {
    let mut lines = match open.await {
        Ok(lines) => lines,
        Err(error) => {
            // The shell may have closed while the stream was opening.
            let _ = results.send((screen, Msg::StreamEnded(number, Some(error))));
            return;
        }
    };
    loop {
        let Ok(permit) = credit.acquire().await else {
            return;
        };
        permit.forget();
        let mut batch = Vec::new();
        if lines.recv_many(&mut batch, STREAM_BATCH).await == 0 {
            let _ = results.send((screen, Msg::StreamEnded(number, None)));
            return;
        }
        if results.send((screen, Msg::Lines(number, batch))).is_err() {
            return;
        }
    }
}

/// The screen stack and everything needed to drive it.
pub struct Shell {
    ctx: Ctx,
    stack: Vec<Entry>,
    next_id: u64,
    status: String,
    results: mpsc::UnboundedSender<(ScreenId, Msg)>,
    inbox: mpsc::UnboundedReceiver<(ScreenId, Msg)>,
    /// Keys and pastes are dropped until this moment.
    keys_ignored_until: Option<Instant>,
    /// Every run started and not yet seen to finish, so that leaving can
    /// wait for them.
    runs: Vec<JoinHandle<()>>,
}

impl Shell {
    /// A shell showing `root`, which is never removed from the stack.
    pub fn new(ctx: Ctx, root: Box<dyn Screen>) -> Shell {
        let (results, inbox) = mpsc::unbounded_channel();
        let mut shell = Shell {
            ctx,
            stack: Vec::new(),
            next_id: 0,
            status: String::new(),
            results,
            inbox,
            keys_ignored_until: None,
            runs: Vec::new(),
        };
        let (id, cmds) = shell.place(root);
        // A root screen that asks to quit while starting still gets drawn
        // once; the next event closes the shell.
        shell.apply(id, cmds);
        shell
    }

    /// Delivers `msg` to the screen `target` names, or to the top screen
    /// when there is none, and carries out what the screen asks for. Returns
    /// false once the TUI should close.
    pub fn handle(&mut self, target: Option<ScreenId>, msg: Msg) -> bool {
        let index = match target {
            None => self.stack.len().checked_sub(1),
            Some(id) => self.stack.iter().position(|entry| entry.id == id),
        };
        let Some(index) = index else {
            return true;
        };
        let typed = target.is_none() && matches!(msg, Msg::Key(_) | Msg::Paste(_));
        let interrupt = matches!(&msg, Msg::Key(key) if target.is_none() && is_ctrl_c(key));
        // Ctrl-C can only cancel, decline, or leave, so it is always let
        // through.
        let ignoring = self
            .keys_ignored_until
            .is_some_and(|until| Instant::now() < until);
        if typed && ignoring && !interrupt {
            return true;
        }
        if typed {
            self.status.clear();
        }
        if interrupt && index == 0 {
            return false;
        }
        let Some(entry) = self.stack.get_mut(index) else {
            return true;
        };
        let delivered_lines = match &msg {
            Msg::Lines(number, _) => Some(*number),
            _ => None,
        };
        let ended = match &msg {
            Msg::StreamEnded(number, _) => Some(*number),
            _ => None,
        };
        let cmds = if interrupt {
            entry.screen.interrupt(&self.ctx)
        } else {
            entry.screen.update(msg, &self.ctx)
        };
        // The screen has this batch, so the stream may read its next one.
        if let Some(number) = delivered_lines
            && let Some(stream) = entry.streams.iter().find(|s| s.number == number)
        {
            stream.credit.add_permits(1);
        }
        if let Some(number) = ended {
            entry.streams.retain(|stream| stream.number != number);
        }
        let id = entry.id;
        self.apply(id, cmds)
    }

    /// Draws the title bar, the top screen, the status line, and the hints.
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            let notice = Paragraph::new("terminal too small").wrap(Wrap { trim: true });
            frame.render_widget(notice, area);
            return;
        }
        let Some(top) = self.stack.last() else {
            return;
        };
        let [title, body, status, hints] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let env = match self.ctx.deps.env.name.as_str() {
            "" => "no environment",
            name => name,
        };
        frame.render_widget(
            Paragraph::new(format!(" ss-support · {env} · {}", top.screen.title()))
                .style(theme::title()),
            title,
        );
        top.screen.view(frame, body);
        frame.render_widget(
            Paragraph::new(format!(" {}", self.status)).style(theme::status()),
            status,
        );
        frame.render_widget(
            Paragraph::new(format!(" {}", top.screen.hints())).style(theme::hints()),
            hints,
        );
    }

    /// Runs until the operator quits, the event stream ends, or the
    /// invocation is cancelled. Whichever way it ends, every run still in
    /// flight is cancelled and given [`RUN_GRACE`] to stop and write its
    /// audit entry before this returns.
    ///
    /// # Errors
    ///
    /// A failure to draw or to read an event.
    pub async fn run_with<B, S>(
        mut self,
        term: &mut Terminal<B>,
        mut events: S,
    ) -> anyhow::Result<Stop>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
        S: Stream<Item = io::Result<Event>> + Unpin,
    {
        let stop = self.event_loop(term, &mut events).await;
        self.finish_runs().await;
        stop
    }

    async fn event_loop<B, S>(
        &mut self,
        term: &mut Terminal<B>,
        events: &mut S,
    ) -> anyhow::Result<Stop>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
        S: Stream<Item = io::Result<Event>> + Unpin,
    {
        let cancel = self.ctx.cancel.clone();
        // Whether the screen has changed since it was drawn, whether by more
        // than lines of a stream, and when lines alone may next be drawn.
        let mut stale = true;
        let mut at_once = true;
        let mut lines_due = Instant::now();
        loop {
            if stale && (at_once || Instant::now() >= lines_due) {
                term.draw(|frame| self.draw(frame))
                    .context("draw the screen")?;
                stale = false;
                at_once = false;
                lines_due = Instant::now() + LINES_REDRAW;
            }
            // What the operator typed comes before background results, so a
            // key is acted on by the screen it was typed at and a busy
            // source of results cannot hold back esc or ctrl+c.
            let keep_going = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(Stop::Interrupted),
                event = events.next() => {
                    stale = true;
                    at_once = true;
                    match event {
                        None => false,
                        Some(Err(e)) => return Err(e).context("read terminal event"),
                        Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                            self.handle(None, Msg::Key(key))
                        }
                        Some(Ok(Event::Paste(text))) => self.handle(None, Msg::Paste(text)),
                        Some(Ok(Event::Resize(..))) => self.handle(None, Msg::Resize),
                        Some(Ok(_)) => true,
                    }
                },
                // Lines that arrived since the last draw are now due.
                () = tokio::time::sleep_until(lines_due), if stale => true,
                Some((id, msg)) = self.inbox.recv() => {
                    stale = true;
                    at_once |= !matches!(msg, Msg::Lines(..));
                    self.handle(Some(id), msg) && self.deliver_waiting(&mut at_once)
                }
            };
            if !keep_going {
                return Ok(Stop::Quit);
            }
        }
    }

    /// Delivers the background results that are already waiting, up to a
    /// limit, so that a burst of them costs one draw rather than one each.
    /// Sets `at_once` when one of them is anything but lines of a stream.
    /// Returns false once the TUI should close.
    fn deliver_waiting(&mut self, at_once: &mut bool) -> bool {
        for _ in 1..RESULTS_PER_DRAW {
            let Ok((id, msg)) = self.inbox.try_recv() else {
                break;
            };
            *at_once |= !matches!(msg, Msg::Lines(..));
            if !self.handle(Some(id), msg) {
                return false;
            }
        }
        true
    }

    /// Cancels every run and waits, up to [`RUN_GRACE`], for them to end.
    async fn finish_runs(&mut self) {
        // Dropping the screens cancels the runs they started and stops their
        // loads and streams.
        self.stack.clear();
        let runs = std::mem::take(&mut self.runs);
        // A run that does not stop in time is left behind.
        let _ = tokio::time::timeout(RUN_GRACE, join_all(runs)).await;
    }

    /// Puts `screen` on top of the stack and returns its id and what it
    /// asked for while starting.
    fn place(&mut self, mut screen: Box<dyn Screen>) -> (ScreenId, Vec<Cmd>) {
        let id = ScreenId(self.next_id);
        self.next_id += 1;
        let cmds = screen.init(&self.ctx);
        self.stack.push(Entry {
            id,
            screen,
            runs: Vec::new(),
            loads: Vec::new(),
            streams: Vec::new(),
        });
        (id, cmds)
    }

    /// Carries out `cmds`, asked for by the screen `issuer`. Returns false
    /// once the TUI should close.
    fn apply(&mut self, issuer: ScreenId, cmds: Vec<Cmd>) -> bool {
        let mut queue: VecDeque<(ScreenId, Cmd)> =
            cmds.into_iter().map(|cmd| (issuer, cmd)).collect();
        while let Some((issuer, cmd)) = queue.pop_front() {
            let position = self.stack.iter().position(|entry| entry.id == issuer);
            match cmd {
                Cmd::Quit => return false,
                Cmd::Status(text) => self.status = text,
                Cmd::IgnoreKeys => {
                    self.keys_ignored_until = Some(Instant::now() + self.ctx.settle);
                }
                Cmd::Push(screen) => {
                    let (id, cmds) = self.place(screen);
                    queue.extend(cmds.into_iter().map(|cmd| (id, cmd)));
                }
                Cmd::Pop => {
                    // The bottom screen stays; so does a stack whose asking
                    // screen has already gone.
                    if let Some(index) = position.filter(|&index| index > 0) {
                        self.stack.truncate(index);
                    }
                }
                Cmd::Replace(screen) => {
                    if let Some(index) = position {
                        self.stack.truncate(index);
                        let (id, cmds) = self.place(screen);
                        queue.extend(cmds.into_iter().map(|cmd| (id, cmd)));
                    }
                }
                Cmd::Task(number, load) => {
                    // A load asked for by a screen that has gone has nobody
                    // to show it, so it never starts.
                    if let Some(entry) = position.and_then(|index| self.stack.get_mut(index)) {
                        let results = self.results.clone();
                        let task = tokio::spawn(async move {
                            let loaded = load.await;
                            // The shell may have closed while the load ran.
                            let _ = results.send((issuer, Msg::Loaded(number, loaded)));
                        });
                        entry.loads.retain(|earlier| !earlier.is_finished());
                        entry.loads.push(task.abort_handle());
                    }
                }
                Cmd::Run(request) => match position.and_then(|index| self.stack.get_mut(index)) {
                    Some(entry) => {
                        entry.runs.push(request.cancel.clone());
                        let results = self.results.clone();
                        let run = bridge::start(&self.ctx, request, move |msg| {
                            results.send((issuer, msg)).is_ok()
                        });
                        self.runs.retain(|earlier| !earlier.is_finished());
                        self.runs.push(run);
                    }
                    // Nobody is left to show the run, so it never starts.
                    None => request.cancel.cancel(),
                },
                Cmd::Stream(number, open) => {
                    // A stream asked for by a screen that has gone is never
                    // opened.
                    if let Some(entry) = position.and_then(|index| self.stack.get_mut(index)) {
                        let credit = Arc::new(Semaphore::new(1));
                        let forwarder = tokio::spawn(forward(
                            issuer,
                            number,
                            open,
                            Arc::clone(&credit),
                            self.results.clone(),
                        ))
                        .abort_handle();
                        entry.streams.push(OpenStream {
                            number,
                            credit,
                            forwarder,
                        });
                    }
                }
            }
        }
        true
    }

    /// Waits for the next background result and delivers it. Returns what
    /// [`Shell::handle`] returned.
    #[cfg(test)]
    pub(crate) async fn pump(&mut self) -> bool {
        match self.inbox.recv().await {
            Some((id, msg)) => self.handle(Some(id), msg),
            None => true,
        }
    }

    /// How many screens are on the stack.
    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.stack.len()
    }
}

/// True for Ctrl-C.
fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::stream;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use ss_core::guard::Class;
    use ss_core::ops::{Input, demo};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tui::screen::{Cmd, Loaded, RunRequest};
    use crate::tui::stub_ops::{Behavior, StubOp};
    use crate::tui::testing::{
        Log, Probe, ch, ctrl, ctx, entries, is_char, key, render_shell, screen_text,
    };

    fn log() -> Log {
        Log::default()
    }

    /// A load that finishes when `gate` is notified.
    fn gated(gate: &Arc<Notify>) -> ss_core::ops::BoxFuture<Loaded> {
        let gate = Arc::clone(gate);
        Box::pin(async move {
            gate.notified().await;
            Loaded::Suggestions(Vec::new())
        })
    }

    fn run_request(cancel: &CancellationToken) -> RunRequest {
        RunRequest {
            op: Arc::new(demo::Hello),
            input: Input::default(),
            cancel: cancel.clone(),
        }
    }

    /// A probe that pushes `child` when `p` is pressed.
    fn pusher(log: &Log, child: Probe) -> Probe {
        let mut child = Some(child);
        Probe::new("root", log).on_msg(move |msg| {
            if is_char(msg, 'p') {
                child
                    .take()
                    .map(|c| vec![Cmd::Push(Box::new(c))])
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        })
    }

    fn popper(name: &'static str, log: &Log) -> Probe {
        Probe::new(name, log).on_msg(|msg| {
            if is_char(msg, 'x') {
                vec![Cmd::Pop]
            } else {
                Vec::new()
            }
        })
    }

    #[test]
    fn ctrl_c_on_the_root_screen_quits() {
        let log = log();
        let mut shell = Shell::new(ctx(), Box::new(Probe::new("root", &log)));

        assert!(!shell.handle(None, Msg::Key(ctrl('c'))));
    }

    #[test]
    fn ctrl_c_on_a_pushed_screen_goes_back_one_screen() {
        let log = log();
        let root = pusher(&log, Probe::new("child", &log));
        let mut shell = Shell::new(ctx(), Box::new(root));
        shell.handle(None, Msg::Key(ch('p')));
        assert_eq!(shell.depth(), 2);

        assert!(shell.handle(None, Msg::Key(ctrl('c'))));

        assert_eq!(shell.depth(), 1);
    }

    #[test]
    fn pop_never_removes_the_root_screen() {
        let log = log();
        let mut shell = Shell::new(ctx(), Box::new(popper("root", &log)));

        assert!(shell.handle(None, Msg::Key(ch('x'))));

        assert_eq!(shell.depth(), 1);
    }

    #[test]
    fn quit_closes_the_shell() {
        let log = log();
        let root = Probe::new("root", &log).on_msg(|_| vec![Cmd::Quit]);
        let mut shell = Shell::new(ctx(), Box::new(root));

        assert!(!shell.handle(None, Msg::Key(ch('q'))));
    }

    #[test]
    fn keys_go_to_the_top_screen_only() {
        let log = log();
        let root = pusher(&log, Probe::new("child", &log));
        let mut shell = Shell::new(ctx(), Box::new(root));
        shell.handle(None, Msg::Key(ch('p')));

        shell.handle(None, Msg::Key(ch('z')));

        assert_eq!(
            entries(&log),
            ["root:init", "root:key:p", "child:init", "child:key:z"]
        );
    }

    #[test]
    fn replace_swaps_the_asking_screen_for_the_new_one() {
        let log = log();
        let mut next = Some(Probe::new("next", &log));
        let child = Probe::new("child", &log).on_msg(move |msg| {
            if is_char(msg, 'r') {
                next.take()
                    .map(|n| vec![Cmd::Replace(Box::new(n))])
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        });
        let mut shell = Shell::new(ctx(), Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));

        shell.handle(None, Msg::Key(ch('r')));

        assert_eq!(shell.depth(), 2);
        assert!(render_shell(&shell, 80, 24).contains("next body"));
    }

    #[test]
    fn a_screen_smaller_than_the_minimum_shows_a_notice() {
        let log = log();
        let shell = Shell::new(ctx(), Box::new(Probe::new("root", &log)));

        render_shell(&shell, 1, 1);
        render_shell(&shell, 0, 0);
        let small = render_shell(&shell, 20, 5);

        assert!(small.contains("terminal too small"), "{small}");
        assert!(!small.contains("root body"), "{small}");
    }

    #[test]
    fn title_bar_names_the_environment_and_status_bar_shows_hints() {
        let log = log();
        let shell = Shell::new(ctx(), Box::new(Probe::new("root", &log)));

        insta::assert_snapshot!(render_shell(&shell, 80, 24));
    }

    #[test]
    fn a_missing_environment_is_named_as_such_in_the_title_bar() {
        let log = log();
        let mut ctx = ctx();
        ctx.deps.env.name.clear();
        let shell = Shell::new(ctx, Box::new(Probe::new("root", &log)));

        let text = render_shell(&shell, 80, 24);

        assert!(
            text.starts_with(" ss-support · no environment · root"),
            "{text}"
        );
    }

    #[test]
    fn a_status_message_shows_until_the_next_key() {
        let log = log();
        let root = Probe::new("root", &log).on_msg(|msg| {
            if is_char(msg, 's') {
                vec![Cmd::Status("saved preset \"nightly\"".to_owned())]
            } else {
                Vec::new()
            }
        });
        let mut shell = Shell::new(ctx(), Box::new(root));

        shell.handle(None, Msg::Key(ch('s')));
        assert!(render_shell(&shell, 80, 24).contains("saved preset \"nightly\""));

        shell.handle(None, Msg::Key(key(KeyCode::Down)));
        assert!(!render_shell(&shell, 80, 24).contains("saved preset"));
    }

    #[tokio::test]
    async fn a_task_result_for_a_popped_screen_is_dropped() {
        let log = log();
        let gate = Arc::new(Notify::new());
        let task = gated(&gate);
        let child = popper("child", &log).on_init(move || vec![Cmd::Task(7, task)]);
        let mut shell = Shell::new(ctx(), Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));
        // The load finishes, and its result is waiting when the screen goes.
        gate.notify_one();
        while shell.inbox.is_empty() {
            tokio::task::yield_now().await;
        }
        shell.handle(None, Msg::Key(ch('x')));

        assert!(shell.pump().await);

        assert_eq!(shell.depth(), 1);
        assert!(
            !entries(&log).iter().any(|e| e.contains("loaded")),
            "{:?}",
            entries(&log)
        );
    }

    #[tokio::test]
    async fn a_task_result_reaches_its_screen_even_when_another_screen_is_on_top() {
        let log = log();
        let gate = Arc::new(Notify::new());
        let task = gated(&gate);
        let root =
            pusher(&log, Probe::new("child", &log)).on_init(move || vec![Cmd::Task(3, task)]);
        let mut shell = Shell::new(ctx(), Box::new(root));
        shell.handle(None, Msg::Key(ch('p')));

        gate.notify_one();
        shell.pump().await;

        assert_eq!(
            entries(&log),
            ["root:init", "root:key:p", "child:init", "root:loaded:3"]
        );
    }

    #[tokio::test]
    async fn a_run_reports_to_the_screen_that_started_it() {
        let log = log();
        let request = RunRequest {
            op: Arc::new(StubOp::read("stub.read", &["fetch"])),
            input: Input::default(),
            cancel: CancellationToken::new(),
        };
        let root = pusher(&log, Probe::new("child", &log)).on_init(move || vec![Cmd::Run(request)]);
        let mut shell = Shell::new(ctx(), Box::new(root));
        shell.handle(None, Msg::Key(ch('p')));

        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(5), shell.pump())
                .await
                .expect("the run reports to the shell");
        }

        assert_eq!(
            entries(&log),
            [
                "root:init",
                "root:key:p",
                "child:init",
                "root:progress:fetch",
                "root:progress:fetch",
                "root:done:ok"
            ]
        );
    }

    #[tokio::test]
    async fn a_run_asked_for_by_a_screen_that_has_gone_never_starts() {
        let log = log();
        let cancel = CancellationToken::new();
        let mut request = Some(run_request(&cancel));
        let child = Probe::new("child", &log).on_msg(move |msg| {
            if is_char(msg, 'x') {
                request
                    .take()
                    .map(|r| vec![Cmd::Pop, Cmd::Run(r)])
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        });
        let mut shell = Shell::new(ctx(), Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));

        shell.handle(None, Msg::Key(ch('x')));
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }

        assert!(cancel.is_cancelled());
        assert!(shell.inbox.try_recv().is_err(), "nothing was started");
    }

    #[tokio::test]
    async fn popping_a_screen_cancels_the_run_it_started() {
        let log = log();
        let cancel = CancellationToken::new();
        let request = run_request(&cancel);
        let child = popper("child", &log).on_init(move || vec![Cmd::Run(request)]);
        let mut shell = Shell::new(ctx(), Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));
        assert!(!cancel.is_cancelled());

        shell.handle(None, Msg::Key(ch('x')));

        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn closing_the_shell_cancels_every_run() {
        let log = log();
        let cancel = CancellationToken::new();
        let request = run_request(&cancel);
        let root = Probe::new("root", &log).on_init(move || vec![Cmd::Run(request)]);
        let shell = Shell::new(ctx(), Box::new(root));
        assert!(!cancel.is_cancelled());

        drop(shell);

        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn the_loop_draws_then_quits_when_a_screen_asks_to() {
        let log = log();
        let root = Probe::new("root", &log).on_msg(|msg| {
            if is_char(msg, 'q') {
                vec![Cmd::Quit]
            } else {
                Vec::new()
            }
        });
        let shell = Shell::new(ctx(), Box::new(root));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let events = stream::iter(vec![
            Ok(Event::Resize(80, 24)),
            Ok(Event::Key(ch('a'))),
            Ok(Event::Key(ch('q'))),
            Ok(Event::Key(ch('b'))),
        ]);

        let stop = shell.run_with(&mut term, events).await.unwrap();

        assert_eq!(stop, Stop::Quit);
        assert!(screen_text(&term).contains("root body"));
        assert_eq!(
            entries(&log),
            ["root:init", "root:resize", "root:key:a", "root:key:q"]
        );
    }

    #[tokio::test]
    async fn the_event_stream_ending_stops_the_loop_as_quit() {
        let log = log();
        let shell = Shell::new(ctx(), Box::new(Probe::new("root", &log)));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();

        let stop = shell
            .run_with(&mut term, stream::iter(Vec::new()))
            .await
            .unwrap();

        assert_eq!(stop, Stop::Quit);
    }

    #[tokio::test]
    async fn cancelling_the_invocation_stops_the_loop_as_interrupted() {
        let log = log();
        let ctx = ctx();
        let cancel = ctx.cancel.clone();
        let shell = Shell::new(ctx, Box::new(Probe::new("root", &log)));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel.cancel();
        });

        let stop = shell.run_with(&mut term, stream::pending()).await.unwrap();

        assert_eq!(stop, Stop::Interrupted);
    }

    #[tokio::test]
    async fn an_event_stream_error_ends_the_loop_with_that_error() {
        let log = log();
        let shell = Shell::new(ctx(), Box::new(Probe::new("root", &log)));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let events = stream::iter(vec![Err(io::Error::other("tty went away"))]);

        let err = shell.run_with(&mut term, events).await.unwrap_err();

        assert_eq!(format!("{err:#}"), "read terminal event: tty went away");
    }

    #[tokio::test]
    async fn a_released_key_is_not_delivered() {
        use ratatui::crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let log = log();
        let shell = Shell::new(ctx(), Box::new(Probe::new("root", &log)));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let release = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };

        shell
            .run_with(&mut term, stream::iter(vec![Ok(Event::Key(release))]))
            .await
            .unwrap();

        assert_eq!(entries(&log), ["root:init"]);
    }

    /// A probe that asks for keys to be ignored when `i` is pressed.
    fn settler(log: &Log) -> Probe {
        Probe::new("root", log).on_msg(|msg| {
            if is_char(msg, 'i') {
                vec![Cmd::IgnoreKeys]
            } else {
                Vec::new()
            }
        })
    }

    #[tokio::test(start_paused = true)]
    async fn keys_are_dropped_for_a_moment_after_a_screen_asks() {
        let log = log();
        let mut ctx = ctx();
        ctx.settle = Duration::from_millis(250);
        let mut shell = Shell::new(ctx, Box::new(settler(&log)));

        shell.handle(None, Msg::Key(ch('i')));
        shell.handle(None, Msg::Key(ch('a')));
        shell.handle(None, Msg::Paste("pasted".to_owned()));
        tokio::time::advance(Duration::from_millis(249)).await;
        shell.handle(None, Msg::Key(ch('b')));
        tokio::time::advance(Duration::from_millis(2)).await;
        shell.handle(None, Msg::Key(ch('c')));

        assert_eq!(entries(&log), ["root:init", "root:key:i", "root:key:c"]);
    }

    #[tokio::test(start_paused = true)]
    async fn ctrl_c_is_never_dropped() {
        let log = log();
        let mut ctx = ctx();
        ctx.settle = Duration::from_millis(250);
        let child = Probe::new("child", &log).on_init(|| vec![Cmd::IgnoreKeys]);
        let mut shell = Shell::new(ctx, Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));
        assert_eq!(shell.depth(), 2);

        shell.handle(None, Msg::Key(ch('z')));
        shell.handle(None, Msg::Key(ctrl('c')));

        assert_eq!(shell.depth(), 1, "ctrl+c still went back");
        assert!(!entries(&log).contains(&"child:key:z".to_owned()));
    }

    #[tokio::test]
    async fn background_results_are_never_dropped_while_keys_are() {
        let log = log();
        let mut ctx = ctx();
        ctx.settle = Duration::from_secs(60);
        let gate = Arc::new(Notify::new());
        let task = gated(&gate);
        let root = settler(&log).on_init(move || vec![Cmd::Task(5, task)]);
        let mut shell = Shell::new(ctx, Box::new(root));
        shell.handle(None, Msg::Key(ch('i')));

        gate.notify_one();
        shell.pump().await;

        assert_eq!(entries(&log), ["root:init", "root:key:i", "root:loaded:5"]);
    }

    #[tokio::test]
    async fn a_paste_reaches_the_top_screen_as_text() {
        let log = log();
        let shell = Shell::new(ctx(), Box::new(Probe::new("root", &log)));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let events = stream::iter(vec![Ok(Event::Paste("orders\ny".to_owned()))]);

        shell.run_with(&mut term, events).await.unwrap();

        assert_eq!(entries(&log), ["root:init", "root:paste:orders\ny"]);
    }

    #[tokio::test]
    async fn a_key_already_typed_is_handled_before_a_result_that_is_also_ready() {
        let log = log();
        let root = Probe::new("root", &log).on_msg(|msg| {
            if is_char(msg, 'q') {
                vec![Cmd::Quit]
            } else {
                Vec::new()
            }
        });
        let shell = Shell::new(ctx(), Box::new(root));
        let root_id = shell.stack[0].id;
        shell
            .results
            .send((root_id, Msg::Loaded(9, Loaded::Suggestions(Vec::new()))))
            .unwrap();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let events = stream::iter(vec![Ok(Event::Key(ch('a')))]).chain(stream::pending());
        let events = events.take_until(tokio::time::sleep(Duration::from_millis(200)));

        shell.run_with(&mut term, Box::pin(events)).await.unwrap();

        assert_eq!(
            entries(&log),
            ["root:init", "root:key:a", "root:loaded:9"],
            "the key the operator typed first is not overtaken"
        );
    }

    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<ss_core::audit::Entry>>);

    impl ss_core::audit::Logger for Recording {
        fn record(&self, e: &ss_core::audit::Entry) -> io::Result<()> {
            self.0.lock().unwrap().push(e.clone());
            Ok(())
        }
    }

    /// A shell whose root starts a mutating run that waits until cancelled,
    /// and the audit trail that run writes to.
    fn shell_with_a_run_in_flight(behavior: Behavior) -> (Shell, Arc<Recording>, Ctx) {
        let audit = Arc::new(Recording::default());
        let mut ctx = ctx();
        ctx.audit = Arc::clone(&audit) as Arc<dyn ss_core::audit::Logger>;
        let request = RunRequest {
            op: Arc::new(StubOp::new("stub.slow", Class::Mutate, behavior)),
            input: Input::default(),
            cancel: ctx.cancel.child_token(),
        };
        let log = log();
        let root = Probe::new("root", &log)
            .on_init(move || vec![Cmd::Run(request)])
            .on_msg(|msg| {
                if is_char(msg, 'q') {
                    vec![Cmd::Quit]
                } else {
                    Vec::new()
                }
            });
        let shell = Shell::new(ctx.clone(), Box::new(root));
        (shell, audit, ctx)
    }

    // The run and the loop are on different threads here, as they are in
    // the real binary; on one thread the run would always happen to finish
    // first. Repeated so that a loop which does not wait is caught.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interrupted_loop_returns_only_after_its_run_has_been_audited() {
        for round in 0..25 {
            let (shell, audit, ctx) = shell_with_a_run_in_flight(Behavior::Hang);
            let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                ctx.cancel.cancel();
            });

            let stop = shell.run_with(&mut term, stream::pending()).await.unwrap();

            assert_eq!(stop, Stop::Interrupted);
            let entries = audit.0.lock().unwrap();
            assert_eq!(
                entries.len(),
                1,
                "round {round}: the audit entry was written"
            );
            assert_eq!(entries[0].op_id, "stub.slow");
        }
    }

    #[tokio::test]
    async fn quitting_with_a_run_in_flight_cancels_it_and_waits_for_its_audit_entry() {
        let (shell, audit, _ctx) = shell_with_a_run_in_flight(Behavior::Hang);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        // The run reports its first step before the key is read.
        let events = stream::once(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(Event::Key(ch('q')))
        });

        let stop = shell.run_with(&mut term, Box::pin(events)).await.unwrap();

        assert_eq!(stop, Stop::Quit);
        assert_eq!(audit.0.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_ignores_cancellation_delays_the_exit_only_so_long() {
        let (shell, audit, ctx) = shell_with_a_run_in_flight(Behavior::Stuck);
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        ctx.cancel.cancel();
        let started = tokio::time::Instant::now();

        let stop = shell.run_with(&mut term, stream::pending()).await.unwrap();

        assert_eq!(stop, Stop::Interrupted);
        assert_eq!(started.elapsed(), RUN_GRACE);
        assert!(audit.0.lock().unwrap().is_empty(), "it never finished");
    }

    /// A stream that opens onto `lines`.
    fn stream_of(lines: mpsc::Receiver<String>) -> crate::tui::screen::LineStream {
        Box::pin(async move { Ok(lines) })
    }

    /// A shell whose root screen reads stream 4 from the returned sender.
    fn shell_with_a_stream(log: &Log, capacity: usize) -> (Shell, mpsc::Sender<String>) {
        let (tx, rx) = mpsc::channel(capacity);
        let stream = stream_of(rx);
        let root = Probe::new("root", log).on_init(move || vec![Cmd::Stream(4, stream)]);
        (Shell::new(ctx(), Box::new(root)), tx)
    }

    async fn pump_in_time(shell: &mut Shell) {
        tokio::time::timeout(Duration::from_secs(5), shell.pump())
            .await
            .expect("the stream reports to the shell");
    }

    #[tokio::test]
    async fn stream_lines_reach_the_screen_that_asked_in_order() {
        let log = log();
        let (mut shell, tx) = shell_with_a_stream(&log, 8);
        tx.send("one".to_owned()).await.unwrap();
        tx.send("two".to_owned()).await.unwrap();

        pump_in_time(&mut shell).await;
        tx.send("three".to_owned()).await.unwrap();
        pump_in_time(&mut shell).await;

        assert_eq!(
            entries(&log),
            ["root:init", "root:lines:4:one|two", "root:lines:4:three"]
        );
    }

    #[tokio::test]
    async fn a_closed_stream_ends_without_an_error() {
        let log = log();
        let (mut shell, tx) = shell_with_a_stream(&log, 8);
        tx.send("last".to_owned()).await.unwrap();
        drop(tx);

        pump_in_time(&mut shell).await;
        pump_in_time(&mut shell).await;

        assert_eq!(
            entries(&log),
            ["root:init", "root:lines:4:last", "root:ended:4"]
        );
    }

    #[tokio::test]
    async fn a_stream_that_cannot_open_ends_with_its_error() {
        let log = log();
        let root = Probe::new("root", &log).on_init(|| {
            vec![Cmd::Stream(
                4,
                Box::pin(async { Err("Kubernetes is not configured".to_owned()) }),
            )]
        });
        let mut shell = Shell::new(ctx(), Box::new(root));

        pump_in_time(&mut shell).await;

        assert_eq!(
            entries(&log),
            ["root:init", "root:ended:4:Kubernetes is not configured"]
        );
    }

    #[tokio::test]
    async fn popping_the_screen_drops_the_stream() {
        let log = log();
        let (tx, rx) = mpsc::channel(8);
        let stream = stream_of(rx);
        let child = popper("child", &log).on_init(move || vec![Cmd::Stream(4, stream)]);
        let mut shell = Shell::new(ctx(), Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));
        tx.send("one".to_owned()).await.unwrap();
        pump_in_time(&mut shell).await;

        shell.handle(None, Msg::Key(ch('x')));

        tokio::time::timeout(Duration::from_secs(5), tx.closed())
            .await
            .expect("the receiver is dropped when the screen goes");
    }

    #[tokio::test]
    async fn closing_the_shell_drops_every_stream() {
        let log = log();
        let (shell, tx) = shell_with_a_stream(&log, 8);
        tokio::task::yield_now().await;

        drop(shell);

        tokio::time::timeout(Duration::from_secs(5), tx.closed())
            .await
            .expect("the receiver is dropped with the shell");
    }

    #[tokio::test]
    async fn lines_for_a_screen_that_has_gone_are_dropped() {
        let log = log();
        let (tx, rx) = mpsc::channel(8);
        let stream = stream_of(rx);
        let child = popper("child", &log).on_init(move || vec![Cmd::Stream(4, stream)]);
        let mut shell = Shell::new(ctx(), Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));
        tx.send("late".to_owned()).await.unwrap();
        // The batch reaches the shell's inbox before the screen goes.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        shell.handle(None, Msg::Key(ch('x')));
        while let Ok((id, msg)) = shell.inbox.try_recv() {
            shell.handle(Some(id), msg);
        }

        assert!(
            !entries(&log).iter().any(|entry| entry.contains("lines")),
            "{:?}",
            entries(&log)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fast_stream_is_read_one_batch_at_a_time() {
        let log = log();
        let (shell, tx) = shell_with_a_stream(&log, 64);
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn({
            let sent = Arc::clone(&sent);
            async move {
                for i in 0..10_000 {
                    if tx.send(format!("line {i}")).await.is_err() {
                        return;
                    }
                    sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });

        // The shell is alive but hands nothing to its screen.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let taken = sent.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            taken <= STREAM_BATCH + 64 + 1,
            "{taken} lines were taken from a stream nobody was reading"
        );
        assert!(taken >= 64, "the first batch was read: {taken}");
        drop(shell);
    }

    #[tokio::test]
    async fn the_next_batch_is_read_once_the_screen_has_the_previous_one() {
        let log = log();
        let (mut shell, tx) = shell_with_a_stream(&log, 4);
        tokio::spawn(async move {
            for i in 0..12 {
                if tx.send(format!("{i}")).await.is_err() {
                    return;
                }
            }
        });

        let mut seen = Vec::new();
        while seen.len() < 12 {
            pump_in_time(&mut shell).await;
            seen = entries(&log)
                .iter()
                .filter_map(|entry| entry.strip_prefix("root:lines:4:"))
                .flat_map(|batch| batch.split('|').map(str::to_owned).collect::<Vec<_>>())
                .collect();
        }

        let expected: Vec<String> = (0..12).map(|i| i.to_string()).collect();
        assert_eq!(seen, expected, "every line arrives once and in order");
    }

    #[tokio::test]
    async fn results_that_are_already_waiting_are_delivered_before_the_next_draw() {
        let log = log();
        let draws = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shell = Shell::new(
            ctx(),
            Box::new(Probe::new("root", &log).count_draws(&draws)),
        );
        let root_id = shell.stack[0].id;
        for number in 0..10 {
            shell
                .results
                .send((
                    root_id,
                    Msg::Loaded(number, Loaded::Suggestions(Vec::new())),
                ))
                .unwrap();
        }
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let events = stream::pending().take_until(tokio::time::sleep(Duration::from_millis(100)));

        shell.run_with(&mut term, Box::pin(events)).await.unwrap();

        assert_eq!(
            entries(&log).len(),
            11,
            "all ten results were delivered: {:?}",
            entries(&log)
        );
        let drawn = draws.load(std::sync::atomic::Ordering::SeqCst);
        assert!(drawn <= 3, "ten waiting results cost {drawn} draws");
    }

    /// A load that never finishes and holds `held` for as long as it lives.
    fn endless(held: mpsc::Receiver<()>) -> ss_core::ops::BoxFuture<Loaded> {
        Box::pin(async move {
            let _held = held;
            std::future::pending::<Loaded>().await
        })
    }

    #[tokio::test(start_paused = true)]
    async fn popping_a_screen_stops_the_load_it_asked_for() {
        let log = log();
        let (tx, rx) = mpsc::channel::<()>(1);
        let load = endless(rx);
        let child = popper("child", &log).on_init(move || vec![Cmd::Task(7, load)]);
        let mut shell = Shell::new(ctx(), Box::new(pusher(&log, child)));
        shell.handle(None, Msg::Key(ch('p')));
        tokio::task::yield_now().await;

        shell.handle(None, Msg::Key(ch('x')));

        tokio::time::timeout(Duration::from_secs(5), tx.closed())
            .await
            .expect("the load is dropped when its screen goes");
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_shell_stops_every_load() {
        let log = log();
        let (tx, rx) = mpsc::channel::<()>(1);
        let load = endless(rx);
        let root = Probe::new("root", &log).on_init(move || vec![Cmd::Task(7, load)]);
        let shell = Shell::new(ctx(), Box::new(root));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();

        shell
            .run_with(&mut term, stream::empty())
            .await
            .expect("the loop ends");

        tokio::time::timeout(Duration::from_secs(5), tx.closed())
            .await
            .expect("the load is dropped with the shell");
    }

    /// How many lines the probe was given, over all batches.
    fn lines_seen(log: &Log) -> usize {
        entries(log)
            .iter()
            .filter_map(|entry| entry.strip_prefix("root:lines:4:"))
            .map(|batch| batch.split('|').count())
            .sum()
    }

    #[tokio::test(start_paused = true)]
    async fn lines_that_keep_arriving_are_drawn_once_an_interval_not_once_a_batch() {
        let log = log();
        let draws = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(1);
        let lines = stream_of(rx);
        let root = Probe::new("root", &log)
            .count_draws(&draws)
            .on_init(move || vec![Cmd::Stream(4, lines)]);
        let shell = Shell::new(ctx(), Box::new(root));
        let feeder = tokio::spawn(async move {
            for number in 0..40 {
                tx.send(format!("line {number}"))
                    .await
                    .expect("the stream is read");
                tokio::task::yield_now().await;
            }
            // Held open, so the stream does not end.
            std::future::pending::<()>().await;
        });
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let events = stream::pending().take_until(tokio::time::sleep(Duration::from_secs(1)));

        shell.run_with(&mut term, Box::pin(events)).await.unwrap();
        feeder.abort();

        assert_eq!(lines_seen(&log), 40, "{:?}", entries(&log));
        let batches = entries(&log)
            .iter()
            .filter(|entry| entry.starts_with("root:lines:"))
            .count();
        assert!(batches >= 10, "the lines arrived in {batches} batches");
        let drawn = draws.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (2..=4).contains(&drawn),
            "{batches} batches of lines cost {drawn} draws"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_is_drawn_at_once_while_lines_wait_for_their_interval() {
        let log = log();
        let draws = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(8);
        let lines = stream_of(rx);
        let root = Probe::new("root", &log)
            .count_draws(&draws)
            .on_init(move || vec![Cmd::Stream(4, lines)]);
        let shell = Shell::new(ctx(), Box::new(root));
        tx.send("one".to_owned()).await.unwrap();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let key = stream::once(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(Event::Key(ch('a')))
        });
        // The loop is over before the interval the line waits for has passed.
        let events = key
            .chain(stream::pending())
            .take_until(tokio::time::sleep(Duration::from_millis(50)));

        shell.run_with(&mut term, Box::pin(events)).await.unwrap();

        assert_eq!(
            entries(&log),
            ["root:init", "root:lines:4:one", "root:key:a"]
        );
        assert_eq!(
            draws.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one draw to start with and one for the key"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_line_that_arrives_after_a_quiet_spell_is_drawn_at_once() {
        let log = log();
        let draws = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(8);
        let lines = stream_of(rx);
        let root = Probe::new("root", &log)
            .count_draws(&draws)
            .on_init(move || vec![Cmd::Stream(4, lines)]);
        let shell = Shell::new(ctx(), Box::new(root));
        let feeder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            tx.send("late".to_owned())
                .await
                .expect("the stream is read");
            std::future::pending::<()>().await;
        });
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        // The loop ends before an interval has passed since the line arrived.
        let events = stream::pending().take_until(tokio::time::sleep(Duration::from_millis(1_010)));

        shell.run_with(&mut term, Box::pin(events)).await.unwrap();
        feeder.abort();

        assert_eq!(lines_seen(&log), 1, "{:?}", entries(&log));
        assert_eq!(draws.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
