//! The interactive terminal front-end: a stack of screens over the same
//! operations, guard, confirmation, and audit trail the command line uses.

// Nothing outside this module calls into it yet.
#![allow(dead_code)]

pub mod components;
pub mod screen;
pub mod shell;
pub mod theme;
pub mod widgets;

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
    if ENTERED.take() {
        // Nothing useful can be done when the terminal itself is failing.
        let _ = leave_sequences(&mut io::stdout());
        let _ = disable_raw_mode();
    }
}

/// Makes a panic put the terminal back before the panic message is printed,
/// so the message is readable and the shell is usable afterwards.
pub(crate) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
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
