//! Runs an operation off the UI: the same runner, guard, confirmation rule,
//! and audit trail as a headless run, with progress and the confirmation
//! question delivered as messages.

use std::sync::Arc;

use async_trait::async_trait;
use ss_core::ops::{ConfirmSpec, Confirmer, FailureKind, RunFailure, Runner};
use tokio_util::sync::CancellationToken;

use super::screen::{ConfirmRequest, Ctx, Msg, RunRequest, RunResult};

/// Starts `req` on a background task. `send` delivers the run's progress,
/// warnings, and confirmation question, then exactly one [`Msg::RunDone`];
/// it returns false once nobody is listening, which cancels the run.
pub fn start(ctx: &Ctx, req: RunRequest, send: impl Fn(Msg) -> bool + Send + Sync + 'static) {
    let RunRequest { op, input, cancel } = req;
    let outbox = Outbox {
        send: Arc::new(send),
        cancel: cancel.clone(),
    };
    let mut runner = Runner::new(ctx.deps.clone())
        .guard(Arc::clone(&ctx.guard))
        .audit(Arc::clone(&ctx.audit))
        .dry_run(ctx.flags.dry_run)
        .yes(ctx.flags.yes)
        .user(ctx.user.clone())
        .cancel(cancel)
        .progress({
            let outbox = outbox.clone();
            move |progress| outbox.deliver(Msg::Progress(progress))
        })
        .on_warning({
            let outbox = outbox.clone();
            move |warning| outbox.deliver(Msg::Warning(warning))
        })
        .confirmer(Arc::new(outbox.clone()));
    tokio::spawn(async move {
        let started_with = input.clone();
        // The operation runs on a task of its own so that a panic inside it
        // still ends the run with a result.
        let run = tokio::spawn(async move { runner.execute(op.as_ref(), input).await });
        let outcome = run.await.unwrap_or_else(|_| {
            Err(RunFailure {
                kind: FailureKind::Failed,
                error: anyhow::anyhow!("the operation panicked"),
                views: Vec::new(),
            })
        });
        outbox.deliver(Msg::RunDone(Box::new(RunResult {
            outcome,
            input: started_with,
        })));
    });
}

/// Where a run's messages go, and the token that stops the run once they
/// can no longer be delivered.
#[derive(Clone)]
struct Outbox {
    send: Arc<dyn Fn(Msg) -> bool + Send + Sync>,
    cancel: CancellationToken,
}

impl Outbox {
    fn deliver(&self, msg: Msg) {
        if !(self.send)(msg) {
            self.cancel.cancel();
        }
    }
}

#[async_trait]
impl Confirmer for Outbox {
    /// Asks through the UI and waits for the answer. A question nobody
    /// answers is a decline.
    async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool> {
        let (request, answer) = ConfirmRequest::new(spec);
        self.deliver(Msg::Confirm(request));
        Ok(answer.await.unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use ss_core::audit::{Entry, Logger, Outcome};
    use ss_core::guard::{Class, EnvAllowlist};
    use ss_core::ops::{FailureKind, Input, Operation, StepStatus, View};
    use ss_core::render;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::tui::screen::RunResult;
    use crate::tui::stub_ops::{Behavior, StubOp};
    use crate::tui::testing::ctx;

    const PATIENCE: Duration = Duration::from_secs(5);

    /// How a test answers the confirmation question.
    #[derive(Clone, Copy)]
    enum Answer {
        Yes,
        No,
        /// Drops the question unanswered.
        Ignore,
    }

    struct Run {
        inbox: mpsc::UnboundedReceiver<Msg>,
        cancel: CancellationToken,
    }

    fn begin(ctx: &Ctx, op: impl Operation + 'static) -> Run {
        let (tx, inbox) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let req = RunRequest {
            op: Arc::new(op),
            input: Input::default(),
            cancel: cancel.clone(),
        };
        start(ctx, req, move |msg| tx.send(msg).is_ok());
        Run { inbox, cancel }
    }

    impl Run {
        /// Everything the run reported before it ended, and how it ended.
        /// Nothing may arrive after the result.
        async fn finish(mut self, answer: Answer) -> (Vec<String>, RunResult) {
            let mut events = Vec::new();
            loop {
                let msg = timeout(PATIENCE, self.inbox.recv())
                    .await
                    .expect("the run reports in time")
                    .expect("the run ends with a result");
                match msg {
                    Msg::Progress(p) => events.push(format!("{}:{:?}", p.step, p.status)),
                    Msg::Warning(w) => events.push(format!("warning:{w}")),
                    Msg::Confirm(request) => {
                        events.push(format!("confirm:{}", request.spec.step));
                        match answer {
                            Answer::Yes => request.answer(true),
                            Answer::No => request.answer(false),
                            Answer::Ignore => drop(request),
                        }
                    }
                    Msg::RunDone(result) => {
                        let extra = timeout(PATIENCE, self.inbox.recv())
                            .await
                            .expect("the channel closes after the result");
                        assert!(extra.is_none(), "a message followed the result");
                        return (events, *result);
                    }
                    Msg::Key(_) | Msg::Resize(..) | Msg::Loaded(..) => {
                        panic!("a run never reports input or load messages")
                    }
                }
            }
        }
    }

    fn kind(result: &RunResult) -> FailureKind {
        result.outcome.as_ref().expect_err("the run failed").kind
    }

    #[derive(Default)]
    struct Recording(Mutex<Vec<Entry>>);

    impl Logger for Recording {
        fn record(&self, e: &Entry) -> io::Result<()> {
            self.0.lock().unwrap().push(e.clone());
            Ok(())
        }
    }

    struct Unwritable;

    impl Logger for Unwritable {
        fn record(&self, _e: &Entry) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[tokio::test]
    async fn start_forwards_progress_then_exactly_one_run_done() {
        let run = begin(&ctx(), StubOp::read("stub.read", &["fetch", "compare"]));

        let (events, result) = run.finish(Answer::Ignore).await;

        assert_eq!(
            events,
            [
                "fetch:Running",
                "fetch:Done",
                "compare:Running",
                "compare:Done"
            ]
        );
        let views = result.outcome.expect("the run succeeded").views;
        assert_eq!(views, [View::summary([("Result", "ok")])]);
    }

    #[tokio::test]
    async fn the_result_carries_the_input_the_run_was_started_with() {
        let (tx, mut inbox) = mpsc::unbounded_channel();
        let mut input = Input::default();
        input.insert("name", "world");
        let req = RunRequest {
            op: Arc::new(StubOp::new("stub.echo", Class::Read, Behavior::Echo)),
            input,
            cancel: CancellationToken::new(),
        };

        start(&ctx(), req, move |msg| tx.send(msg).is_ok());

        let Some(Msg::RunDone(result)) = timeout(PATIENCE, inbox.recv()).await.unwrap() else {
            panic!("an operation without steps reports only its result");
        };
        assert_eq!(result.input.str("name"), "world");
        assert_eq!(
            render::text(&result.outcome.unwrap().views),
            "name : world\n"
        );
    }

    #[tokio::test]
    async fn a_confirmed_write_runs_the_mutating_step() {
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");

        let (events, result) = begin(&ctx(), op).finish(Answer::Yes).await;

        assert_eq!(events, ["confirm:apply", "apply:Running", "apply:Done"]);
        assert!(result.outcome.is_ok());
        assert!(done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn the_confirmation_names_the_resource_and_carries_the_preview() {
        let (op, _done) = StubOp::write("stub.drop", Class::Destructive, "orders-dev");
        let mut run = begin(&ctx(), op);

        let Some(Msg::Confirm(request)) = timeout(PATIENCE, run.inbox.recv()).await.unwrap() else {
            panic!("a destructive run asks before anything else");
        };

        assert_eq!(request.spec.op_id, "stub.drop");
        assert_eq!(request.spec.class, Class::Destructive);
        assert_eq!(request.spec.resource, "orders-dev");
        assert_eq!(request.spec.step, "apply");
        assert_eq!(
            request.spec.preview,
            [View::summary([("Will change", "orders-dev")])]
        );
    }

    #[tokio::test]
    async fn a_declined_confirmation_ends_the_run_as_declined() {
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");

        let (events, result) = begin(&ctx(), op).finish(Answer::No).await;

        assert_eq!(events, ["confirm:apply", "apply:Skipped"]);
        assert_eq!(kind(&result), FailureKind::Declined);
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_dropped_confirmation_request_declines() {
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");

        let (_events, result) = begin(&ctx(), op).finish(Answer::Ignore).await;

        assert_eq!(kind(&result), FailureKind::Declined);
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelling_the_token_ends_the_run_as_cancelled() {
        let mut run = begin(
            &ctx(),
            StubOp::new("stub.hang", Class::Read, Behavior::Hang),
        );
        let first = timeout(PATIENCE, run.inbox.recv()).await.unwrap();
        assert!(matches!(first, Some(Msg::Progress(p)) if p.status == StepStatus::Running));

        run.cancel.cancel();
        let (_events, result) = run.finish(Answer::Ignore).await;

        assert_eq!(kind(&result), FailureKind::Cancelled);
    }

    #[tokio::test]
    async fn cancelling_while_the_question_is_open_ends_the_run_as_cancelled() {
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");
        let mut run = begin(&ctx(), op);
        let Some(Msg::Confirm(request)) = timeout(PATIENCE, run.inbox.recv()).await.unwrap() else {
            panic!("the run asks first");
        };

        run.cancel.cancel();
        let (_events, result) = run.finish(Answer::Ignore).await;

        assert_eq!(kind(&result), FailureKind::Cancelled);
        assert!(!done.load(Ordering::SeqCst));
        // Answering late is harmless.
        request.answer(true);
    }

    #[tokio::test]
    async fn the_run_is_cancelled_when_the_ui_stops_listening() {
        let cancel = CancellationToken::new();
        let req = RunRequest {
            op: Arc::new(StubOp::new("stub.hang", Class::Read, Behavior::Hang)),
            input: Input::default(),
            cancel: cancel.clone(),
        };

        start(&ctx(), req, |_msg| false);

        timeout(PATIENCE, cancel.cancelled())
            .await
            .expect("the run is cancelled once a message cannot be delivered");
    }

    #[tokio::test]
    async fn a_guard_denial_is_reported_without_asking_for_confirmation() {
        let mut ctx = ctx();
        ctx.guard = Arc::new(EnvAllowlist {
            name: "prod".to_owned(),
            allowed: false,
        });
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-prod");

        let (events, result) = begin(&ctx, op).finish(Answer::Yes).await;

        assert_eq!(events, [] as [&str; 0]);
        assert_eq!(kind(&result), FailureKind::Denied);
        let error = format!("{:#}", result.outcome.unwrap_err().error);
        assert!(error.contains("prod"), "{error}");
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_run_writes_one_audit_entry_with_its_outcome() {
        let mut ctx = ctx();
        let audit = Arc::new(Recording::default());
        ctx.audit = Arc::clone(&audit) as Arc<dyn Logger>;
        let (op, _done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");

        begin(&ctx, op).finish(Answer::No).await;

        let entries = audit.0.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].op_id, "stub.write");
        assert_eq!(entries[0].outcome, Outcome::Declined);
        assert_eq!(entries[0].user, "tester");
        assert_eq!(entries[0].env, "dev");
    }

    #[tokio::test]
    async fn a_failed_audit_write_is_forwarded_as_a_warning() {
        let mut ctx = ctx();
        ctx.audit = Arc::new(Unwritable);
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");

        let (events, result) = begin(&ctx, op).finish(Answer::Yes).await;

        assert!(result.outcome.is_ok());
        assert!(done.load(Ordering::SeqCst));
        assert_eq!(
            events.last().map(String::as_str),
            Some("warning:audit log: disk full")
        );
    }

    #[tokio::test]
    async fn a_dry_run_stops_before_the_write_without_asking() {
        let mut ctx = ctx();
        ctx.flags.dry_run = true;
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");

        let (events, result) = begin(&ctx, op).finish(Answer::Yes).await;

        assert_eq!(events, ["apply:Skipped"]);
        let text = render::text(&result.outcome.expect("a dry run succeeds").views);
        assert!(
            text.contains("dry-run: stopped before the first mutating step"),
            "{text}"
        );
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn yes_skips_the_question_for_a_mutating_operation() {
        let mut ctx = ctx();
        ctx.flags.yes = true;
        let (op, done) = StubOp::write("stub.write", Class::Mutate, "orders-dev");

        let (events, _result) = begin(&ctx, op).finish(Answer::No).await;

        assert_eq!(events, ["apply:Running", "apply:Done"]);
        assert!(done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn yes_never_skips_the_question_for_a_destructive_operation() {
        let mut ctx = ctx();
        ctx.flags.yes = true;
        let (op, done) = StubOp::write("stub.drop", Class::Destructive, "orders-dev");

        let (events, result) = begin(&ctx, op).finish(Answer::No).await;

        assert_eq!(events, ["confirm:apply", "apply:Skipped"]);
        assert_eq!(kind(&result), FailureKind::Declined);
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_failing_step_ends_the_run_with_the_step_named_in_the_error() {
        let op = StubOp::new("stub.fail", Class::Read, Behavior::Fail("AccessDenied"));

        let (events, result) = begin(&ctx(), op).finish(Answer::Ignore).await;

        assert_eq!(events, ["fetch:Running", "fetch:Failed"]);
        assert_eq!(kind(&result), FailureKind::Failed);
        assert_eq!(
            format!("{:#}", result.outcome.unwrap_err().error),
            "fetch: AccessDenied"
        );
    }

    #[tokio::test]
    async fn a_panicking_operation_ends_the_run_as_failed() {
        let op = StubOp::new("stub.panic", Class::Read, Behavior::Panic);

        let (_events, result) = begin(&ctx(), op).finish(Answer::Ignore).await;

        assert_eq!(kind(&result), FailureKind::Failed);
        assert_eq!(
            format!("{:#}", result.outcome.unwrap_err().error),
            "the operation panicked"
        );
    }
}
