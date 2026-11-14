//! `ss-support` — support operations CLI.
//!
//! `main.rs` is a shim: it builds [`Services`], parses [`Cli`], and turns the
//! [`Exit`] returned by [`run`] into the process exit code. Everything
//! testable lives here so the end-to-end suite can call [`run_with`]
//! in-process with captured [`Io`].

mod aws;
mod cli;
mod cmd;
mod confirm;
mod io;
mod kubectl;
mod run;
mod services;
mod tui;

pub use cli::{
    Cli, Command, GlobalFlags, OpCall, OpsCmd, Output, PresetsCmd, RunCmd, command, parse_from,
};
pub use io::{Captured, Io, Sink, Source};
pub use run::{Exit, run, run_tui_with, run_with, run_with_interrupt};
pub use services::{DdbFactory, KubectlFactory, S3Factory, Services};
