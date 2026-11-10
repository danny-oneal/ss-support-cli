//! The invocation pipeline: load config, build the per-invocation [`App`],
//! dispatch the subcommand, and turn the outcome into an [`Exit`] with at
//! most one error line on stderr.

use std::future::Future;
use std::io::Write;
use std::sync::Arc;

use ss_core::audit::{self, FileLogger, Logger};
use ss_core::cache;
use ss_core::config::{self, ConfigError};
use ss_core::guard::{EnvAllowlist, Guard};
use ss_core::ops::Deps;
use ss_core::preset::{self, PresetError};
use ss_core::svc::s3::Cached;
use ss_core::vars::{self, Vars};
use tokio_util::sync::CancellationToken;

use crate::cli::{Cli, GlobalFlags};
use crate::cmd;
use crate::io::Io;
use crate::services::{S3Factory, Services};

/// Process exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The operation completed (a successful `--dry-run` included).
    Ok = 0,
    /// The operation failed: step error, guard denied, operator declined, or
    /// an unreadable config or presets file.
    Failed = 1,
    /// Usage or config error: bad flag value, unknown operation or format,
    /// unparsable config/presets file.
    Usage = 2,
    /// Interrupted by Ctrl-C, SIGTERM, or SIGHUP (128 + SIGINT).
    Interrupted = 130,
}

impl Exit {
    /// The numeric code handed to `std::process::exit`.
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Why an invocation failed; the variant decides the exit status.
pub(crate) enum Failure {
    /// The user's input or files are wrong.
    Usage(anyhow::Error),
    /// The command ran and failed.
    Failed(anyhow::Error),
    /// The command was stopped by an interrupt.
    Interrupted(anyhow::Error),
}

impl Failure {
    fn into_parts(self) -> (Exit, anyhow::Error) {
        match self {
            Failure::Usage(e) => (Exit::Usage, e),
            Failure::Failed(e) => (Exit::Failed, e),
            Failure::Interrupted(e) => (Exit::Interrupted, e),
        }
    }
}

/// A bad or unparsable presets file, or an unknown preset, is the user's to
/// fix; failing to read or write the file is not.
impl From<PresetError> for Failure {
    fn from(e: PresetError) -> Self {
        if e.is_usage() {
            Failure::Usage(e.into())
        } else {
            Failure::Failed(e.into())
        }
    }
}

/// An unparsable config file is the user's to fix; failing to read it is
/// not.
impl From<ConfigError> for Failure {
    fn from(e: ConfigError) -> Self {
        if e.is_usage() {
            Failure::Usage(e.into())
        } else {
            Failure::Failed(e.into())
        }
    }
}

/// Everything a command needs, built once per invocation from the config
/// file, the global flags, and the environment variables.
pub(crate) struct App {
    /// Infrastructure handed to operations.
    pub deps: Deps,
    /// Refuses writes outside allowlisted environments.
    pub guard: Arc<dyn Guard>,
    /// Records every operation run.
    pub audit: Arc<dyn Logger>,
    /// Saved parameter sets.
    pub presets: preset::Store,
    /// Who is running the command, for the audit trail.
    pub user: String,
    /// Cancelled on the first interrupt.
    pub cancel: CancellationToken,
}

impl App {
    fn build(
        flags: &GlobalFlags,
        vars: &Vars,
        s3: &S3Factory,
        cancel: CancellationToken,
    ) -> Result<Self, Failure> {
        let cfg = config::load(&config::default_path(vars))?;
        let env = flags.resolve_env(&cfg, vars);
        let cache: Arc<dyn cache::Store> = if flags.no_cache {
            Arc::new(cache::Nop)
        } else {
            Arc::new(cache::Dir::new(cache::default_dir(vars)))
        };
        let guard = Arc::new(EnvAllowlist {
            name: env.name.clone(),
            allowed: env.allowed,
        });
        let port = Cached::new(s3(&env, vars), Arc::clone(&cache));
        let deps = Deps {
            cache,
            env,
            diff: cfg.diff.clone(),
            trace: cfg.trace.clone(),
            s3: Some(Arc::new(port)),
        };
        let user = ["USER", "LOGNAME"]
            .into_iter()
            .find_map(|key| vars::get(vars, key))
            .unwrap_or_default()
            .to_owned();
        Ok(Self {
            deps,
            guard,
            audit: Arc::new(FileLogger::new(audit::default_path(vars))),
            presets: preset::Store::new(preset::default_path(vars)),
            user,
            cancel,
        })
    }
}

/// Runs the CLI on the process's standard streams. The first Ctrl-C,
/// SIGTERM, or SIGHUP cancels the command; a second one of any kind exits at
/// once with [`Exit::Interrupted`].
pub async fn run(cli: Cli, services: Services) -> Exit {
    // Installed before anything runs, so no signal takes its default action
    // once the command has started.
    let mut signals = Signals::install();
    let interrupt = async move {
        signals.next().await;
        tokio::spawn(async move {
            signals.next().await;
            std::process::exit(Exit::Interrupted.code());
        });
    };
    run_with_interrupt(cli, services, Io::std(), interrupt).await
}

/// Runs the CLI on `io`, never interrupted, so tests can drive the whole
/// pipeline in-process.
pub async fn run_with(cli: Cli, services: Services, io: Io) -> Exit {
    run_with_interrupt(cli, services, io, std::future::pending()).await
}

/// Runs the CLI on `io`, cancelling the command when `interrupt` resolves.
/// A failure is reported as one `<command>: <error chain>` line on stderr.
pub async fn run_with_interrupt<F>(cli: Cli, services: Services, io: Io, interrupt: F) -> Exit
where
    F: Future<Output = ()> + Send + 'static,
{
    let cancel = CancellationToken::new();
    let watcher = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            interrupt.await;
            cancel.cancel();
        }
    });
    let result = pipeline(&cli, &services, &io, cancel).await;
    watcher.abort();
    match result {
        Ok(()) => Exit::Ok,
        Err(failure) => {
            let (exit, e) = failure.into_parts();
            // Nowhere is left to report a failure to write the error itself.
            let _ = writeln!(io.stderr.clone(), "{}: {e:#}", cli.prefix());
            exit
        }
    }
}

async fn pipeline(
    cli: &Cli,
    services: &Services,
    io: &Io,
    cancel: CancellationToken,
) -> Result<(), Failure> {
    let app = App::build(&cli.global, &services.vars, &services.s3, cancel)?;
    cmd::dispatch(cli, &services.registry, &app, io).await
}

/// The signals that interrupt a command: SIGINT, SIGTERM, and SIGHUP.
#[cfg(unix)]
struct Signals(Vec<tokio::signal::unix::Signal>);

#[cfg(unix)]
impl Signals {
    fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        // A signal whose handler cannot be installed keeps its default action.
        Self(
            [
                SignalKind::interrupt(),
                SignalKind::terminate(),
                SignalKind::hangup(),
            ]
            .into_iter()
            .filter_map(|kind| signal(kind).ok())
            .collect(),
        )
    }

    /// Resolves when any of the signals arrives.
    async fn next(&mut self) {
        std::future::poll_fn(|cx| {
            let arrived = self
                .0
                .iter_mut()
                .any(|s| s.poll_recv(cx) == std::task::Poll::Ready(Some(())));
            if arrived {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await;
    }
}

/// The signal that interrupts a command: Ctrl-C.
#[cfg(not(unix))]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn install() -> Self {
        Self
    }

    /// Resolves on the next Ctrl-C.
    async fn next(&mut self) {
        if tokio::signal::ctrl_c().await.is_err() {
            // No handler could be installed, so there is nothing to wait for.
            std::future::pending::<()>().await;
        }
    }
}
