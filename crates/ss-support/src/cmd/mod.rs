//! The fixed subcommands, dispatched from a parsed [`Cli`].

use std::io::{ErrorKind, Write};

use ss_core::ops::{Registry, View};

use crate::cli::{Cli, Command};
use crate::io::Io;
use crate::run::{App, Failure};
use crate::tui::{self, RunFlags};

mod completion;
mod ops;
mod presets;
mod run_op;

pub(crate) use completion::run as completion;
pub(crate) use run_op::elapsed_text;

/// Runs the chosen subcommand. With none, an operator at a terminal gets the
/// interactive front-end, and anything else (a pipe, a script) gets the help.
pub(crate) async fn dispatch(
    cli: &Cli,
    registry: &Registry,
    app: &App,
    io: &Io,
) -> Result<(), Failure> {
    match &cli.command {
        None if io.is_terminal => tui::launch(registry, app, RunFlags::of(&cli.global)).await,
        None => print(io, &crate::cli::command(registry).render_help().to_string()),
        Some(Command::Ops(cmd)) => ops::run(cmd, registry, io),
        Some(Command::Presets(cmd)) => presets::run(cmd, &app.presets, io),
        Some(Command::Run(cmd)) => run_op::run(cmd, &cli.global, registry, app, io).await,
        Some(Command::Completion { shell }) => completion::run(*shell, registry, io),
    }
}

/// Writes `text` to stdout. A reader that has gone away (`ss-support ops list |
/// head -1`) is not a failure.
fn print(io: &Io, text: &str) -> Result<(), Failure> {
    let mut stdout = io.stdout.clone();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => Err(Failure::Failed(
            anyhow::Error::new(e).context("write output"),
        )),
        _ => Ok(()),
    }
}

/// Writes one diagnostic line to stderr.
fn note(io: &Io, line: &str) {
    // Nowhere is left to report a failure to write a diagnostic.
    let _ = writeln!(io.stderr.clone(), "{line}");
}

/// A table view; an empty title prints no title line.
fn table(title: &str, cols: &[&str], rows: Vec<Vec<String>>) -> View {
    View::Table {
        title: title.to_owned(),
        cols: cols.iter().map(|&c| c.to_owned()).collect(),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::io::Sink;

    /// A stdout whose reader has gone away.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    /// A stdout that fails for any other reason.
    struct Full;

    impl Write for Full {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn closed_stdout_is_not_a_failure() {
        let (io, captured) = Io::capture("");
        let io = Io {
            stdout: Sink::new(ClosedPipe),
            ..io
        };
        assert!(print(&io, "rows\n").is_ok());
        assert_eq!(captured.stderr(), "");
    }

    #[test]
    fn other_stdout_errors_fail() {
        let (io, _) = Io::capture("");
        let io = Io {
            stdout: Sink::new(Full),
            ..io
        };
        let Err(Failure::Failed(e)) = print(&io, "rows\n") else {
            panic!("want a failure");
        };
        assert_eq!(format!("{e:#}"), "write output: disk full");
    }
}
