//! The fixed subcommands, dispatched from a parsed [`Cli`].

use std::io::Write;

use anyhow::{Context, anyhow};
use ss_core::ops::{Registry, View};

use crate::cli::{Cli, Command};
use crate::io::Io;
use crate::run::{App, Failure};

mod ops;
mod presets;

/// Runs the chosen subcommand; with none, prints the help.
pub(crate) async fn dispatch(
    cli: &Cli,
    registry: &Registry,
    app: &App,
    io: &Io,
) -> Result<(), Failure> {
    match &cli.command {
        None => print(io, &crate::cli::command(registry).render_help().to_string()),
        Some(Command::Ops(cmd)) => ops::run(cmd, registry, io),
        Some(Command::Presets(cmd)) => presets::run(cmd, &app.presets, io),
        Some(Command::Run(_)) => Err(Failure::Usage(anyhow!("not available"))),
    }
}

/// Writes `text` to stdout.
fn print(io: &Io, text: &str) -> Result<(), Failure> {
    io.stdout
        .clone()
        .write_all(text.as_bytes())
        .context("write output")
        .map_err(Failure::Failed)
}

/// A table view; an empty title prints no title line.
fn table(title: &str, cols: &[&str], rows: Vec<Vec<String>>) -> View {
    View::Table {
        title: title.to_owned(),
        cols: cols.iter().map(|&c| c.to_owned()).collect(),
        rows,
    }
}
