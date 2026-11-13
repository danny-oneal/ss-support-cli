//! The interactive terminal front-end: a stack of screens over the same
//! operations, guard, confirmation, and audit trail the command line uses.

// Nothing outside this module calls into it yet.
#![allow(dead_code)]

pub mod bridge;
pub mod components;
pub mod screen;
pub mod shell;
pub mod theme;
pub mod widgets;

#[cfg(test)]
pub(crate) mod stub_ops;
#[cfg(test)]
pub(crate) mod testing;

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

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

fn enter_sequences(out: &mut impl Write) -> io::Result<()> {
    execute!(out, EnterAlternateScreen, Hide)
}

fn leave_sequences(out: &mut impl Write) -> io::Result<()> {
    execute!(out, Show, LeaveAlternateScreen)
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
    restore();
}

/// Puts the terminal back if the TUI holds it, and says whether it did.
fn restore() -> bool {
    let held = ENTERED.take();
    if held {
        // Nothing useful can be done when the terminal itself is failing.
        let _ = leave_sequences(&mut io::stdout());
        let _ = disable_raw_mode();
    }
    held
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
        let held = restore();
        previous(info);
        if held {
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

        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[?1049h\x1b[?25l");
    }

    #[test]
    fn leaving_shows_the_cursor_and_leaves_the_alternate_screen() {
        let mut out = Vec::new();

        leave_sequences(&mut out).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "\x1b[?25h\x1b[?1049l");
    }
}
