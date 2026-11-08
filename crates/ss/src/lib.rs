//! `ss` — support operations CLI.
//!
//! `main.rs` is a shim: it builds [`Services`], parses [`Cli`], and turns the
//! [`Exit`] returned by [`run`] into the process exit code. Everything
//! testable lives here so the end-to-end suite can call [`run_with`]
//! in-process with captured [`Io`].

mod cli;
mod cmd;
mod confirm;
mod io;
mod run;
mod services;

pub use cli::{
    Cli, Command, GlobalFlags, OpCall, OpsCmd, Output, PresetsCmd, RunCmd, command, parse_from,
};
pub use io::{Captured, Io, Sink, Source};
pub use run::{Exit, run, run_with, run_with_interrupt};
pub use services::Services;
