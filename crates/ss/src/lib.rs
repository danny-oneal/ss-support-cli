//! `ss` — support operations CLI.
//!
//! `main.rs` is a shim: it parses [`Cli`] and turns the [`Exit`] returned by
//! [`run`] into the process exit code. Everything testable lives here so the
//! end-to-end suite can call [`run_with`] in-process with captured output.

use std::io::Write;

mod cli;

pub use cli::{
    Cli, Command, GlobalFlags, OpCall, OpsCmd, Output, PresetsCmd, RunCmd, command, parse_from,
};

/// Process exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The operation completed (a successful `--dry-run` included).
    Ok = 0,
    /// The operation failed: step error, guard denied, operator declined.
    Failed = 1,
    /// Usage or config error: bad flag value, unknown operation or format,
    /// unparsable config/presets file.
    Usage = 2,
    /// Interrupted by Ctrl-C (128 + SIGINT).
    Interrupted = 130,
}

impl Exit {
    /// The numeric code handed to `std::process::exit`.
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Runs the CLI with the real stdout/stderr.
pub async fn run(cli: Cli) -> Exit {
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    run_with(cli, &mut out, &mut err).await
}

/// Runs the CLI writing results to `out` and diagnostics to `err`, so tests
/// can drive the whole pipeline in-process.
pub async fn run_with(_cli: Cli, _out: &mut dyn Write, _err: &mut dyn Write) -> Exit {
    Exit::Ok
}
