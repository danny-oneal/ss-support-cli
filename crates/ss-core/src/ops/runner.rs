//! Executes one operation: defaults, required-parameter check, guard,
//! audit trail, and the timed, cancellable steps an operation is made of.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::audit::{self, Entry, Logger, Outcome};
use crate::guard::{AllowAll, Class, Guard, GuardError, Request};

use super::{ConfirmSpec, Confirmer, Deps, Input, Meta, OpResult, Operation, View};

/// The lifecycle of one step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepStatus {
    /// The step has started.
    Running,
    /// The step finished successfully.
    Done,
    /// The step returned an error or was cancelled.
    Failed,
    /// The step was not run.
    Skipped,
}

/// Emitted around every step so a multi-step operation renders as a live
/// checklist instead of an opaque spinner.
#[derive(Clone, PartialEq, Debug)]
pub struct Progress {
    /// The step name.
    pub step: String,
    /// Where the step is in its lifecycle.
    pub status: StepStatus,
    /// Time since the step started; zero while it has not run.
    pub elapsed: Duration,
    /// The step's own error, without the step name prefix.
    pub err: Option<String>,
}

/// Why the runner stopped an operation itself.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// A dry run reached the first mutating step.
    #[error("dry-run: stopped before mutating step")]
    DryRun,
    /// The run was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// A required parameter had no value.
    #[error("required param {param:?} missing")]
    MissingParam {
        /// The parameter name.
        param: String,
    },
}

/// How a failed run is classified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureKind {
    /// A required parameter was missing.
    MissingParam,
    /// The guard refused the run.
    Denied,
    /// The operator declined at confirmation.
    Declined,
    /// The run was cancelled.
    Cancelled,
    /// A step failed.
    Failed,
}

/// A run that did not complete. Declined runs also carry the views explaining
/// where they stopped.
#[derive(Debug)]
pub struct RunFailure {
    /// What kind of failure it was.
    pub kind: FailureKind,
    /// The underlying error.
    pub error: anyhow::Error,
    /// Views to show alongside the error.
    pub views: Vec<View>,
}

/// The first error of type `E` in `err`'s chain, looking through context
/// layers.
pub fn find_cause<E: std::error::Error + Send + Sync + 'static>(err: &anyhow::Error) -> Option<&E> {
    err.chain().find_map(|e| e.downcast_ref::<E>())
}

/// Fills the declared default of every parameter the input does not carry.
pub fn with_defaults(op: &dyn Operation, mut input: Input) -> Input {
    for p in op.params() {
        if let Some(default) = p.default
            && !input.contains(p.name)
        {
            input.insert(p.name, default);
        }
    }
    input
}

/// State that lives for one run and is reset by [`Runner::execute`].
#[derive(Default)]
struct RunState {
    meta: Option<Meta>,
    preview: Vec<View>,
    resource: String,
    confirmed: bool,
}

/// Carries [`Deps`] plus every cross-cutting concern: step timing, progress,
/// error wrapping, cancellation, guard, audit, and dry-run.
///
/// A runner executes one operation at a time; create one per run.
pub struct Runner {
    /// Shared infrastructure for the operation.
    pub deps: Deps,
    guard: Arc<dyn Guard>,
    audit: Arc<dyn Logger>,
    progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
    dry_run: bool,
    yes: bool,
    user: String,
    confirmer: Option<Arc<dyn Confirmer>>,
    token: CancellationToken,
    state: RunState,
}

impl Runner {
    /// A runner that permits everything, audits nothing, and reports no
    /// progress.
    pub fn new(deps: Deps) -> Self {
        Runner {
            deps,
            guard: Arc::new(AllowAll),
            audit: Arc::new(audit::Nop),
            progress: None,
            dry_run: false,
            yes: false,
            user: String::new(),
            confirmer: None,
            token: CancellationToken::new(),
            state: RunState::default(),
        }
    }

    /// Sets the guard consulted before the operation runs.
    pub fn guard(mut self, guard: Arc<dyn Guard>) -> Self {
        self.guard = guard;
        self
    }

    /// Sets where audit entries are recorded.
    pub fn audit(mut self, audit: Arc<dyn Logger>) -> Self {
        self.audit = audit;
        self
    }

    /// Sets the callback that receives step progress.
    pub fn progress(mut self, f: impl Fn(Progress) + Send + Sync + 'static) -> Self {
        self.progress = Some(Arc::new(f));
        self
    }

    /// Stops the run before the first mutating step when set.
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Auto-confirms mutating operations when set; it never covers
    /// destructive ones.
    pub fn yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    /// Sets the operator name recorded in the audit trail.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    /// Sets who is asked to confirm mutating steps.
    pub fn confirmer(mut self, confirmer: Arc<dyn Confirmer>) -> Self {
        self.confirmer = Some(confirmer);
        self
    }

    /// Replaces the cancellation token.
    pub fn cancel(mut self, token: CancellationToken) -> Self {
        self.token = token;
        self
    }

    /// A handle that cancels this runner's run.
    pub fn cancel_token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Runs one operation end to end: defaults, required-parameter check,
    /// guard, the operation itself, and an audit record for every run that
    /// is not a read.
    pub async fn execute(
        &mut self,
        op: &dyn Operation,
        input: Input,
    ) -> Result<OpResult, RunFailure> {
        let meta = op.meta();
        let input = with_defaults(op, input);

        if let Some(p) = op
            .params()
            .into_iter()
            .find(|p| p.required && input.str(p.name).is_empty())
        {
            return Err(RunFailure {
                kind: FailureKind::MissingParam,
                error: RunnerError::MissingParam {
                    param: p.name.to_owned(),
                }
                .into(),
                views: Vec::new(),
            });
        }

        let start = Instant::now();
        let req = Request {
            op_id: meta.id.to_owned(),
            class: meta.class,
            resource: input.str("resource").to_owned(),
        };
        if let Err(e) = self.guard.authorize(&req) {
            let error = anyhow::Error::new(e);
            self.record(&meta, &input, Outcome::Denied, start, Some(&error));
            return Err(RunFailure {
                kind: FailureKind::Denied,
                error,
                views: Vec::new(),
            });
        }

        self.state = RunState {
            meta: Some(meta),
            resource: input.str("resource").to_owned(),
            ..RunState::default()
        };

        match op.run(self, &input).await {
            Ok(result) => {
                self.record(&meta, &input, Outcome::Ok, start, None);
                Ok(result)
            }
            Err(error) => self.classify(&meta, &input, start, error),
        }
    }

    fn classify(
        &self,
        meta: &Meta,
        input: &Input,
        start: Instant,
        error: anyhow::Error,
    ) -> Result<OpResult, RunFailure> {
        let stopped = |outcome: &str| {
            vec![View::summary([
                ("Operation", meta.id.to_owned()),
                (
                    "Outcome",
                    format!("{outcome}: stopped before the first mutating step"),
                ),
            ])]
        };
        let runner_err = find_cause::<RunnerError>(&error);

        if matches!(runner_err, Some(RunnerError::DryRun)) {
            self.record(meta, input, Outcome::DryRun, start, None);
            return Ok(OpResult {
                views: stopped("dry-run"),
            });
        }
        if find_cause::<GuardError>(&error).is_some_and(GuardError::is_decline) {
            self.record(meta, input, Outcome::Declined, start, Some(&error));
            return Err(RunFailure {
                kind: FailureKind::Declined,
                views: stopped("declined"),
                error,
            });
        }
        let kind = if matches!(runner_err, Some(RunnerError::Cancelled)) {
            FailureKind::Cancelled
        } else {
            FailureKind::Failed
        };
        self.record(meta, input, Outcome::Error, start, Some(&error));
        Err(RunFailure {
            kind,
            error,
            views: Vec::new(),
        })
    }

    /// Runs one unit of work: emits progress, times it, prefixes any error
    /// with the step name (`fetch both versions: AccessDenied`), and stops
    /// promptly when the run is cancelled.
    pub async fn step<F, Fut, T>(&mut self, name: &str, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        if self.token.is_cancelled() {
            return Err(anyhow::Error::new(RunnerError::Cancelled).context(name.to_owned()));
        }
        self.emit(name, StepStatus::Running, Duration::ZERO, None);
        let start = Instant::now();
        let token = self.token.clone();
        let outcome = tokio::select! {
            biased;
            _ = token.cancelled() => Err(anyhow::Error::new(RunnerError::Cancelled)),
            r = f(self.token.clone()) => r,
        };
        let elapsed = start.elapsed();
        match outcome {
            Ok(value) => {
                self.emit(name, StepStatus::Done, elapsed, None);
                Ok(value)
            }
            Err(e) => {
                self.emit(name, StepStatus::Failed, elapsed, Some(format!("{e:#}")));
                Err(e.context(name.to_owned()))
            }
        }
    }

    /// Registers what is about to change so the confirmation can show it.
    /// Call it before the first mutating step.
    pub fn preview(&mut self, views: impl IntoIterator<Item = View>) {
        self.state.preview.extend(views);
    }

    /// Names the resource a destructive operation will affect; the operator
    /// must type it back to proceed.
    pub fn set_resource(&mut self, s: impl Into<String>) {
        self.state.resource = s.into();
    }

    /// [`Runner::step`] for state-changing work. A dry run stops here, before
    /// anything changes. The first mutating step of a run collects the
    /// operator's confirmation; `--yes` covers mutating operations but never
    /// destructive ones, and a missing confirmer fails closed.
    pub async fn mutate<F, Fut, T>(&mut self, name: &str, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        if self.dry_run {
            self.emit(name, StepStatus::Skipped, Duration::ZERO, None);
            return Err(RunnerError::DryRun.into());
        }
        if !self.state.confirmed {
            if let Err(e) = self.confirm(name).await {
                self.emit(name, StepStatus::Skipped, Duration::ZERO, None);
                return Err(e);
            }
            self.state.confirmed = true;
        }
        self.step(name, f).await
    }

    async fn confirm(&self, step: &str) -> anyhow::Result<()> {
        let meta = self
            .state
            .meta
            .ok_or_else(|| anyhow::anyhow!("no operation is running"))?;
        if meta.class == Class::Mutate && self.yes {
            return Ok(());
        }
        let op = meta.id.to_owned();
        let Some(confirmer) = &self.confirmer else {
            return Err(GuardError::NoConfirmer { op }.into());
        };
        let resource = if self.state.resource.is_empty() {
            meta.id.to_owned()
        } else {
            self.state.resource.clone()
        };
        let confirmed = confirmer
            .confirm(ConfirmSpec {
                op_id: op.clone(),
                class: meta.class,
                resource,
                step: step.to_owned(),
                preview: self.state.preview.clone(),
            })
            .await?;
        if confirmed {
            Ok(())
        } else {
            Err(GuardError::NotConfirmed { op }.into())
        }
    }

    fn emit(&self, step: &str, status: StepStatus, elapsed: Duration, err: Option<String>) {
        if let Some(f) = &self.progress {
            f(Progress {
                step: step.to_owned(),
                status,
                elapsed,
                err,
            });
        }
    }

    /// Writes the audit entry. Reads are not audited, and a failure to write
    /// never changes the run's own outcome.
    fn record(
        &self,
        meta: &Meta,
        input: &Input,
        outcome: Outcome,
        start: Instant,
        error: Option<&anyhow::Error>,
    ) {
        if meta.class == Class::Read {
            return;
        }
        let entry = Entry {
            time: Utc::now(),
            user: self.user.clone(),
            env: self.deps.env.name.clone(),
            account: self.deps.env.account_id.clone(),
            op_id: meta.id.to_owned(),
            params: input.as_map().clone(),
            outcome,
            duration_ms: i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX),
            error: error.map(|e| format!("{e:#}")),
        };
        let _ = self.audit.record(&entry);
    }
}
