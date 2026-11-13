//! The contract between the shell and its screens: what a screen is told
//! ([`Msg`]), what it may ask for ([`Cmd`]), and the shared context it reads
//! ([`Ctx`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ss_core::audit::Logger;
use ss_core::guard::Guard;
use ss_core::ops::{
    BoxFuture, Choice, ConfirmSpec, Deps, Input, OpResult, Operation, Progress, Registry,
    RunFailure,
};
use ss_core::preset;
use ss_core::svc::s3::{Bucket, Page, Version};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::cli::GlobalFlags;
use crate::run::App;

/// How long keys are dropped after a question or a result appears. A key
/// typed, repeated, or pasted before the operator could have read the screen
/// must not answer it.
pub const SETTLE: Duration = Duration::from_millis(250);

/// The global flags that change how an operation runs from the TUI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunFlags {
    /// Stop before the first mutating step.
    pub dry_run: bool,
}

impl RunFlags {
    /// The run flags given on the command line. `--yes` is not among them:
    /// an operator who is present is always asked.
    pub fn of(global: &GlobalFlags) -> RunFlags {
        RunFlags {
            dry_run: global.dry_run,
        }
    }
}

/// Everything screens share for one invocation.
#[derive(Clone)]
pub struct Ctx {
    /// Every operation that can be run.
    pub registry: Registry,
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
    /// Cancelled when the invocation is interrupted.
    pub cancel: CancellationToken,
    /// How operations are run.
    pub flags: RunFlags,
    /// Where exported reports are written.
    pub export_dir: PathBuf,
    /// The clock used to stamp exported reports.
    pub now: fn() -> DateTime<Utc>,
    /// How long keys are dropped when a screen asks with
    /// [`Cmd::IgnoreKeys`].
    pub settle: Duration,
}

impl Ctx {
    /// The context for one invocation's [`App`], exporting to the working
    /// directory.
    pub(crate) fn from_app(app: &App, registry: &Registry, flags: RunFlags) -> Ctx {
        Ctx {
            registry: registry.clone(),
            deps: app.deps.clone(),
            guard: Arc::clone(&app.guard),
            audit: Arc::clone(&app.audit),
            presets: app.presets.clone(),
            user: app.user.clone(),
            cancel: app.cancel.clone(),
            flags,
            export_dir: PathBuf::from("."),
            now: Utc::now,
            settle: SETTLE,
        }
    }
}

/// Identifies one screen on the stack for as long as it stays there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScreenId(pub(crate) u64);

/// A request to run an operation.
pub struct RunRequest {
    /// The operation to run.
    pub op: Arc<dyn Operation>,
    /// The parameters the operator gave.
    pub input: Input,
    /// Cancels the run.
    pub cancel: CancellationToken,
}

/// How a run ended.
pub struct RunResult {
    /// The views on success, or why the run stopped.
    pub outcome: Result<OpResult, RunFailure>,
    /// The parameters the run was started with.
    pub input: Input,
}

/// A confirmation the running operation is waiting on.
pub struct ConfirmRequest {
    /// What is about to change.
    pub spec: ConfirmSpec,
    reply: oneshot::Sender<bool>,
}

impl ConfirmRequest {
    /// A request and the receiver its answer arrives on. A request dropped
    /// unanswered closes the receiver.
    pub fn new(spec: ConfirmSpec) -> (ConfirmRequest, oneshot::Receiver<bool>) {
        let (reply, answer) = oneshot::channel();
        (ConfirmRequest { spec, reply }, answer)
    }

    /// Answers the request: `true` proceeds, `false` declines.
    pub fn answer(self, proceed: bool) {
        // The run may already have been cancelled; then nobody is waiting.
        let _ = self.reply.send(proceed);
    }
}

/// The result of a background load a screen asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded {
    /// Suggested values for a parameter.
    Suggestions(Vec<Choice>),
    /// The buckets visible to the caller.
    Buckets(Vec<Bucket>),
    /// One page of a bucket listing.
    Page(Page),
    /// The versions of one object.
    Versions(Vec<Version>),
    /// The load failed, with the error to show.
    Failed(String),
}

/// Opens a stream of text lines, such as a log being followed. It resolves
/// to the receiver the lines arrive on, or to the error to show when the
/// stream cannot be opened. Dropping the receiver stops the source.
pub type LineStream = BoxFuture<Result<mpsc::Receiver<String>, String>>;

/// Something a screen is told about.
pub enum Msg {
    /// A key was pressed.
    Key(KeyEvent),
    /// Text was pasted. It is only ever text for the focused input: its
    /// line breaks never act as enter and its letters never act as keys.
    Paste(String),
    /// The terminal was resized.
    Resize,
    /// A step of the screen's run changed state.
    Progress(Progress),
    /// The screen's run produced a warning.
    Warning(String),
    /// The screen's run is waiting for a confirmation.
    Confirm(ConfirmRequest),
    /// The screen's run ended.
    RunDone(Box<RunResult>),
    /// A load the screen started with [`Cmd::Task`] finished; the number is
    /// the one the screen gave the task.
    Loaded(u64, Loaded),
    /// More lines of the stream the screen opened with [`Cmd::Stream`] under
    /// this number, oldest first. The next lines are read from the source
    /// only after these have been delivered.
    Lines(u64, Vec<String>),
    /// The stream with this number is over: it reached its end, or it failed
    /// with the error given.
    StreamEnded(u64, Option<String>),
}

/// Something a screen asks the shell to do.
pub enum Cmd {
    /// Show this screen on top of the asking screen.
    Push(Box<dyn Screen>),
    /// Remove the asking screen, returning to the one below. The bottom
    /// screen is never removed.
    Pop,
    /// Put this screen in place of the asking screen.
    Replace(Box<dyn Screen>),
    /// Leave the TUI.
    Quit,
    /// Show this text in the status line.
    Status(String),
    /// Drop keys and pastes for [`Ctx::settle`] from now. A screen asks
    /// when it has just put something new in front of the operator that a
    /// key already on its way must not answer. Ctrl-C is never dropped.
    IgnoreKeys,
    /// Run this load in the background and deliver its result as
    /// [`Msg::Loaded`] with the same number.
    Task(u64, BoxFuture<Loaded>),
    /// Run this operation and deliver its progress, confirmation, and
    /// result to the asking screen.
    Run(RunRequest),
    /// Open this stream and deliver its lines as [`Msg::Lines`] and its end
    /// as [`Msg::StreamEnded`], both with the same number. The stream is
    /// stopped when the asking screen leaves the stack.
    Stream(u64, LineStream),
}

/// One full-screen view with its own state and keys.
pub trait Screen: Send {
    /// The text shown in the title bar.
    fn title(&self) -> String;
    /// The key hints shown in the bottom line.
    fn hints(&self) -> String;
    /// Called once when the screen is placed on the stack.
    fn init(&mut self, ctx: &Ctx) -> Vec<Cmd>;
    /// Reacts to a message. Nothing slow happens here: anything that calls
    /// a remote service is requested through the returned commands. Small
    /// local files (a preset, an exported report) are read and written in
    /// place.
    fn update(&mut self, msg: Msg, ctx: &Ctx) -> Vec<Cmd>;
    /// Draws the screen into `area`.
    fn view(&self, frame: &mut Frame, area: Rect);
    /// Reacts to Ctrl-C on a screen that is not the bottom one. The default
    /// goes back one screen.
    fn interrupt(&mut self, _ctx: &Ctx) -> Vec<Cmd> {
        vec![Cmd::Pop]
    }
}
