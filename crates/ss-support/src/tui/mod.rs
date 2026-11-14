//! The interactive terminal front-end: a stack of screens over the same
//! operations, guard, confirmation, and audit trail the command line uses.

pub mod bridge;
pub mod components;
pub mod presets;
pub mod screen;
pub mod screens;
pub mod shell;
pub mod text;
pub mod theme;
pub mod widgets;

#[cfg(test)]
pub(crate) mod stub_ops;
#[cfg(test)]
pub(crate) mod testing;

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use futures_util::Stream;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ss_core::ops::Registry;

use crate::run::{App, Failure};

use screen::Ctx;
pub use screen::RunFlags;
use screens::opspicker::OpsPicker;
use shell::{Shell, Stop};

/// Runs the TUI on the process's terminal until the operator leaves it.
/// The terminal is taken over for the duration and put back on every way
/// out, a panic included.
pub(crate) async fn launch(registry: &Registry, app: &App, flags: RunFlags) -> Result<(), Failure> {
    install_panic_hook();
    let mut out = io::stdout();
    let guard = TermGuard::enter(&mut out)
        .context("take over the terminal")
        .map_err(Failure::Failed)?;
    let outcome = match Terminal::new(CrosstermBackend::new(out)) {
        Ok(mut term) => run_on(registry, app, flags, &mut term, EventStream::new()).await,
        Err(e) => Err(Failure::Failed(
            anyhow::Error::new(e).context("open the terminal"),
        )),
    };
    // The terminal is put back before any error is reported on it.
    drop(guard);
    outcome
}

/// Runs the TUI on `term`, fed by `events`, starting on the operation
/// picker. Leaving the TUI is a success; an invocation cancelled from
/// outside is an interruption.
pub(crate) async fn run_on<B, S>(
    registry: &Registry,
    app: &App,
    flags: RunFlags,
    term: &mut Terminal<B>,
    events: S,
) -> Result<(), Failure>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    S: Stream<Item = io::Result<Event>> + Unpin,
{
    let ctx = Ctx::from_app(app, registry, flags);
    let shell = Shell::new(ctx, Box::new(OpsPicker::new(registry)));
    match shell.run_with(term, events).await {
        Ok(Stop::Quit) => Ok(()),
        Ok(Stop::Interrupted) => Err(Failure::Interrupted(anyhow::anyhow!("interrupted"))),
        Err(e) => Err(Failure::Failed(e)),
    }
}

/// Whether the terminal is in the TUI's mode and still has to be put back.
struct Entered(AtomicBool);

impl Entered {
    const fn new() -> Self {
        Entered(AtomicBool::new(false))
    }

    /// Records that the terminal has been taken over.
    fn mark(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// True exactly once after each [`Entered::mark`], so the terminal is
    /// put back once however many exit paths ask.
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

static ENTERED: Entered = Entered::new();

/// Bracketed paste is turned on so that pasted text arrives marked as a
/// paste and can be kept from acting as keys.
fn enter_sequences(out: &mut impl Write) -> io::Result<()> {
    execute!(out, EnterAlternateScreen, Hide, EnableBracketedPaste)
}

fn leave_sequences(out: &mut impl Write) -> io::Result<()> {
    execute!(out, DisableBracketedPaste, Show, LeaveAlternateScreen)
}

/// Holds the terminal in raw mode on the alternate screen and puts it back
/// when dropped.
pub struct TermGuard(());

impl TermGuard {
    /// Takes over the terminal, writing the control sequences to `out`.
    ///
    /// # Errors
    ///
    /// A failure to switch modes; the terminal is put back first.
    pub fn enter(out: &mut impl Write) -> io::Result<TermGuard> {
        enable_raw_mode()?;
        ENTERED.mark();
        if let Err(e) = enter_sequences(out) {
            restore_terminal();
            return Err(e);
        }
        Ok(TermGuard(()))
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Puts the terminal back the way the TUI found it. Safe to call from any
/// exit path, any number of times; it does nothing when the TUI never took
/// the terminal over.
pub fn restore_terminal() {
    drop(restore_and_silence());
}

/// [`restore_terminal`] for a path that is about to end the process while
/// other threads are still running. When the TUI held the terminal, the
/// returned lock on stdout must be kept until the process exits: it stops
/// the thread that draws from writing another frame onto the terminal that
/// has just been put back. Without the TUI nothing is locked.
#[must_use = "dropping the lock lets another thread draw on the restored terminal"]
pub fn restore_terminal_for_exit() -> Option<io::StdoutLock<'static>> {
    restore_and_silence()
}

/// Puts the terminal back if the TUI holds it, and returns the lock on
/// stdout it was put back under.
fn restore_and_silence() -> Option<io::StdoutLock<'static>> {
    if !ENTERED.take() {
        return None;
    }
    let mut out = io::stdout().lock();
    // Nothing useful can be done when the terminal itself is failing.
    let _ = leave_sequences(&mut out);
    let _ = out.flush();
    let _ = disable_raw_mode();
    Some(out)
}

/// The exit status of a process ended by a panic.
const PANIC_EXIT: i32 = 101;

/// Makes a panic while the TUI holds the terminal put the terminal back,
/// print the panic message where it can be read, and end the process. A
/// TUI that kept running after a panic on some task would be drawing on a
/// terminal it no longer controls.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Held until the process ends, so no frame follows the restore or
        // lands in the middle of the panic message.
        let silenced = restore_terminal_for_exit();
        previous(info);
        if silenced.is_some() {
            // No destructor runs from here on.
            crate::kubectl::stop_children();
            std::process::exit(PANIC_EXIT);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_runs_once_however_often_it_is_called() {
        let entered = Entered::new();
        entered.mark();

        assert!(entered.take());
        assert!(!entered.take());
        assert!(!entered.take());
    }

    #[test]
    fn restore_is_a_no_op_when_the_terminal_was_never_entered() {
        assert!(!Entered::new().take());
        // The process-wide flag is never set under test, so this must not
        // touch the terminal the tests run in.
        restore_terminal();
    }

    #[test]
    fn entering_switches_to_the_alternate_screen_and_hides_the_cursor() {
        let mut out = Vec::new();

        enter_sequences(&mut out).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[?1049h\x1b[?25l\x1b[?2004h"
        );
    }

    #[test]
    fn leaving_shows_the_cursor_and_leaves_the_alternate_screen() {
        let mut out = Vec::new();

        leave_sequences(&mut out).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[?2004l\x1b[?25h\x1b[?1049l"
        );
    }
}
