//! The shell: a stack of screens, the event loop that feeds them, and the
//! title, status, and hint lines drawn around the top screen.

use std::collections::VecDeque;
use std::io;

use anyhow::Context;
use futures_util::{Stream, StreamExt};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::screen::{Cmd, Ctx, Msg, Screen, ScreenId};
use super::theme;

/// The smallest terminal the screens are drawn on.
const MIN_WIDTH: u16 = 20;
const MIN_HEIGHT: u16 = 6;

/// Why the event loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The operator left the TUI.
    Quit,
    /// The invocation was cancelled from outside.
    Interrupted,
}

/// One screen on the stack and the runs it started.
struct Entry {
    id: ScreenId,
    screen: Box<dyn Screen>,
    runs: Vec<CancellationToken>,
}

impl Drop for Entry {
    /// A run never outlives the screen that shows it.
    fn drop(&mut self) {
        for run in &self.runs {
            run.cancel();
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
        let interrupt = matches!(&msg, Msg::Key(key) if target.is_none() && is_ctrl_c(key));
        if target.is_none() && matches!(msg, Msg::Key(_)) {
            self.status.clear();
        }
        if interrupt && index == 0 {
            return false;
        }
        let Some(entry) = self.stack.get_mut(index) else {
            return true;
        };
        let cmds = if interrupt {
            entry.screen.interrupt(&self.ctx)
        } else {
            entry.screen.update(msg, &self.ctx)
        };
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
    /// invocation is cancelled.
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
        let cancel = self.ctx.cancel.clone();
        loop {
            term.draw(|frame| self.draw(frame))
                .context("draw the screen")?;
            let keep_going = tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(Stop::Interrupted),
                Some((id, msg)) = self.inbox.recv() => self.handle(Some(id), msg),
                event = events.next() => match event {
                    None => false,
                    Some(Err(e)) => return Err(e).context("read terminal event"),
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        self.handle(None, Msg::Key(key))
                    }
                    Some(Ok(Event::Resize(width, height))) => {
                        self.handle(None, Msg::Resize(width, height))
                    }
                    Some(Ok(_)) => true,
                },
            };
            if !keep_going {
                return Ok(Stop::Quit);
            }
        }
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
                    let results = self.results.clone();
                    tokio::spawn(async move {
                        let loaded = load.await;
                        // The shell may have closed while the load ran.
                        let _ = results.send((issuer, Msg::Loaded(number, loaded)));
                    });
                }
                Cmd::Run(request) => match position.and_then(|index| self.stack.get_mut(index)) {
                    Some(entry) => entry.runs.push(request.cancel),
                    None => request.cancel.cancel(),
                },
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
    use ss_core::ops::{Input, demo};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tui::screen::{Cmd, Loaded, RunRequest};
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
        shell.handle(None, Msg::Key(ch('x')));

        gate.notify_one();
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

    #[test]
    fn popping_a_screen_cancels_the_run_it_started() {
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

    #[test]
    fn closing_the_shell_cancels_every_run() {
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
            ["root:init", "root:resize:80x24", "root:key:a", "root:key:q"]
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
}
