//! Runner behavior: guard, audit, steps, progress, and cancellation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ss_core::audit::{Entry, FileLogger, Logger, Outcome};
use ss_core::config::Environment;
use ss_core::guard::{Class, EnvAllowlist, GuardError};
use ss_core::ops::{
    Deps, FailureKind, Input, Meta, OpResult, Operation, Param, Progress, Runner, RunnerError,
    StepStatus, View, find_cause,
};

#[derive(Clone)]
enum Act {
    Step(&'static str),
    FailStep(&'static str, &'static str),
    Sleep(&'static str, u64),
    AssertInt(&'static str, i64),
    StopDry,
    Decline,
}

struct Script {
    meta: Meta,
    params: Vec<Param>,
    acts: Vec<Act>,
    mutated: Arc<AtomicUsize>,
}

fn script(class: Class, acts: Vec<Act>) -> Script {
    Script {
        meta: Meta {
            id: "test.op",
            name: "Test",
            summary: "A test operation",
            category: "Test",
            class,
        },
        params: Vec::new(),
        acts,
        mutated: Arc::new(AtomicUsize::new(0)),
    }
}

#[async_trait]
impl Operation for Script {
    fn meta(&self) -> Meta {
        self.meta
    }

    fn params(&self) -> Vec<Param> {
        self.params.clone()
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        self.mutated.fetch_add(1, Ordering::SeqCst);
        for act in &self.acts {
            match act.clone() {
                Act::Step(name) => r.step(name, |_| async { Ok(()) }).await?,
                Act::FailStep(name, msg) => {
                    r.step(name, |_| async move { Err::<(), _>(anyhow::anyhow!(msg)) })
                        .await?
                }
                Act::Sleep(name, ms) => {
                    r.step(name, |_| async move {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok(())
                    })
                    .await?
                }
                Act::StopDry => {
                    return Err(anyhow::Error::new(RunnerError::DryRun).context("change"));
                }
                Act::Decline => {
                    return Err(anyhow::Error::new(GuardError::NotConfirmed {
                        op: "test.op".into(),
                    })
                    .context("test.op"));
                }
                Act::AssertInt(k, n) => {
                    if input.int(k) != n {
                        anyhow::bail!("expected {k}={n}");
                    }
                }
            }
        }
        Ok(OpResult {
            views: vec![View::summary([("Done", "yes")])],
        })
    }
}

#[derive(Default)]
struct RecordingAudit {
    entries: Mutex<Vec<Entry>>,
}

impl Logger for RecordingAudit {
    fn record(&self, e: &Entry) -> std::io::Result<()> {
        self.entries.lock().unwrap().push(e.clone());
        Ok(())
    }
}

struct FailingAudit;

impl Logger for FailingAudit {
    fn record(&self, _e: &Entry) -> std::io::Result<()> {
        Err(std::io::Error::other("disk full"))
    }
}

fn progress_log() -> (Arc<Mutex<Vec<Progress>>>, impl Fn(Progress) + Send + Sync) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    (log, move |p| sink.lock().unwrap().push(p))
}

#[tokio::test]
async fn step_wraps_errors_with_step_name() {
    let op = script(
        Class::Read,
        vec![Act::FailStep("fetch both versions", "AccessDenied")],
    );
    let f = Runner::new(Deps::default())
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::Failed);
    assert_eq!(
        format!("{:#}", f.error),
        "fetch both versions: AccessDenied"
    );
}

#[tokio::test]
async fn step_emits_progress_in_order() {
    let op = script(Class::Read, vec![Act::Step("one"), Act::Step("two")]);
    let (log, sink) = progress_log();
    Runner::new(Deps::default())
        .progress(sink)
        .execute(&op, Input::default())
        .await
        .unwrap();
    let seen: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|p| {
            let status = match p.status {
                StepStatus::Running => "running",
                StepStatus::Done => "done",
                StepStatus::Failed => "failed",
                StepStatus::Skipped => "skipped",
            };
            format!("{}:{status}", p.step)
        })
        .collect();
    assert_eq!(seen, ["one:running", "one:done", "two:running", "two:done"]);
}

#[tokio::test]
async fn failed_step_reports_unprefixed_error() {
    let op = script(Class::Read, vec![Act::FailStep("x", "boom")]);
    let (log, sink) = progress_log();
    let _ = Runner::new(Deps::default())
        .progress(sink)
        .execute(&op, Input::default())
        .await;
    let log = log.lock().unwrap();
    let last = log.last().unwrap();
    assert_eq!(last.status, StepStatus::Failed);
    assert_eq!(last.err.as_deref(), Some("boom"));
}

#[tokio::test]
async fn required_param_and_defaults() {
    let mut op = script(Class::Read, vec![Act::AssertInt("repeat", 3)]);
    op.params = vec![
        Param::new("bucket").required(true),
        Param::new("repeat").default("3"),
    ];

    let f = Runner::new(Deps::default())
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::MissingParam);
    assert_eq!(f.error.to_string(), r#"required param "bucket" missing"#);
    assert_eq!(op.mutated.load(Ordering::SeqCst), 0);

    let input: Input = [("bucket", "b")].into_iter().collect();
    Runner::new(Deps::default())
        .execute(&op, input)
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_param_is_not_audited() {
    let mut op = script(Class::Mutate, vec![]);
    op.params = vec![Param::new("bucket").required(true)];
    let audit = Arc::new(RecordingAudit::default());
    let _ = Runner::new(Deps::default())
        .audit(audit.clone())
        .execute(&op, Input::default())
        .await;
    assert!(audit.entries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn guard_denial_beats_yes_and_is_audited() {
    let op = script(Class::Mutate, vec![Act::Step("change")]);
    let audit = Arc::new(RecordingAudit::default());
    let f = Runner::new(Deps::default())
        .guard(Arc::new(EnvAllowlist {
            name: "prod".into(),
            allowed: false,
        }))
        .yes(true)
        .audit(audit.clone())
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::Denied);
    assert_eq!(op.mutated.load(Ordering::SeqCst), 0);
    let entries = audit.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Outcome::Denied);
    assert_eq!(entries[0].env, "");
    assert!(
        entries[0]
            .error
            .as_deref()
            .unwrap()
            .contains("not in the configured allowlist")
    );
}

#[tokio::test]
async fn non_read_runs_are_audited() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("audit.log");
    let op = script(Class::Mutate, vec![Act::Step("change")]);
    let deps = Deps {
        env: Environment {
            name: "dev".into(),
            account_id: "123".into(),
            ..Environment::default()
        },
        ..Deps::default()
    };
    let input: Input = [("x", "1")].into_iter().collect();
    Runner::new(deps)
        .yes(true)
        .user("danny")
        .audit(Arc::new(FileLogger::new(&path)))
        .execute(&op, input)
        .await
        .unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["op"], "test.op");
    assert_eq!(v["outcome"], "ok");
    assert_eq!(v["user"], "danny");
    assert_eq!(v["env"], "dev");
    assert_eq!(v["account"], "123");
    assert_eq!(v["params"], serde_json::json!({"x": "1"}));
}

#[tokio::test]
async fn read_runs_are_not_audited() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("audit.log");
    let op = script(Class::Read, vec![Act::Step("look")]);
    Runner::new(Deps::default())
        .audit(Arc::new(FileLogger::new(&path)))
        .execute(&op, Input::default())
        .await
        .unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn pre_cancelled_token_stops_steps() {
    let op = script(Class::Read, vec![Act::Step("never runs")]);
    let (log, sink) = progress_log();
    let mut runner = Runner::new(Deps::default()).progress(sink);
    runner.cancel_token().cancel();
    let f = runner.execute(&op, Input::default()).await.unwrap_err();
    assert_eq!(f.kind, FailureKind::Cancelled);
    assert_eq!(format!("{:#}", f.error), "never runs: cancelled");
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_mid_step_is_prompt_and_audited() {
    let op = script(Class::Mutate, vec![Act::Sleep("slow", 5000)]);
    let audit = Arc::new(RecordingAudit::default());
    let mut runner = Runner::new(Deps::default()).yes(true).audit(audit.clone());
    let token = runner.cancel_token();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
    });

    let start = Instant::now();
    let f = runner.execute(&op, Input::default()).await.unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(f.kind, FailureKind::Cancelled);
    assert_eq!(format!("{:#}", f.error), "slow: cancelled");
    let entries = audit.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Outcome::Error);
    assert_eq!(entries[0].error.as_deref(), Some("slow: cancelled"));
}

#[tokio::test]
async fn audit_write_failure_does_not_change_the_outcome() {
    let op = script(Class::Mutate, vec![Act::Step("change")]);
    let quiet = Runner::new(Deps::default())
        .yes(true)
        .execute(&op, Input::default())
        .await
        .unwrap();
    let failing = Runner::new(Deps::default())
        .yes(true)
        .audit(Arc::new(FailingAudit))
        .execute(&op, Input::default())
        .await
        .unwrap();
    assert_eq!(quiet, failing);
}

#[tokio::test]
async fn dry_run_is_a_success_with_a_summary_and_is_audited() {
    let op = script(Class::Mutate, vec![Act::StopDry]);
    let audit = Arc::new(RecordingAudit::default());
    let result = Runner::new(Deps::default())
        .audit(audit.clone())
        .execute(&op, Input::default())
        .await
        .unwrap();
    assert_eq!(
        result.views,
        [View::summary([
            ("Operation", "test.op"),
            ("Outcome", "dry-run: stopped before the first mutating step"),
        ])]
    );
    let entries = audit.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Outcome::DryRun);
    assert_eq!(entries[0].error, None);
}

#[tokio::test]
async fn declined_run_carries_a_summary_and_is_audited() {
    let op = script(Class::Mutate, vec![Act::Decline]);
    let audit = Arc::new(RecordingAudit::default());
    let f = Runner::new(Deps::default())
        .audit(audit.clone())
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::Declined);
    assert_eq!(
        f.views,
        [View::summary([
            ("Operation", "test.op"),
            (
                "Outcome",
                "declined: stopped before the first mutating step"
            ),
        ])]
    );
    let entries = audit.entries.lock().unwrap();
    assert_eq!(entries[0].outcome, Outcome::Declined);
    assert_eq!(
        entries[0].error.as_deref(),
        Some("test.op: operation not confirmed")
    );
}

#[test]
fn find_cause_looks_through_context_layers() {
    let err = anyhow::Error::new(RunnerError::Cancelled)
        .context("inner")
        .context("outer");
    assert!(matches!(
        find_cause::<RunnerError>(&err),
        Some(RunnerError::Cancelled)
    ));
    assert!(find_cause::<GuardError>(&err).is_none());
}
