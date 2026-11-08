//! Runner behavior: guard, audit, steps, progress, and cancellation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ss_core::audit::{Entry, FileLogger, Logger, Outcome};
use ss_core::config::Environment;
use ss_core::guard::{Class, EnvAllowlist, GuardError};
use ss_core::ops::{
    ConfirmSpec, Confirmer, Deps, FailureKind, Input, Meta, OpResult, Operation, Param, Progress,
    Runner, RunnerError, StepStatus, View, find_cause,
};

#[derive(Clone)]
enum Act {
    Step(&'static str),
    FailStep(&'static str, &'static str),
    Sleep(&'static str, u64),
    AssertInt(&'static str, i64),
    StopDry,
    Decline,
    Mutate(&'static str),
    Preview,
    Resource(&'static str),
}

struct Script {
    meta: Meta,
    params: Vec<Param>,
    acts: Vec<Act>,
    runs: Arc<AtomicUsize>,
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
        runs: Arc::new(AtomicUsize::new(0)),
        mutated: Arc::new(AtomicUsize::new(0)),
    }
}

fn writer(class: Class) -> Script {
    let mut op = script(
        class,
        vec![
            Act::Preview,
            Act::Resource("cfg-nonprod/tenants/acme.json"),
            Act::Mutate("write one"),
            Act::Mutate("write two"),
        ],
    );
    op.meta.id = "test.write";
    op
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
        self.runs.fetch_add(1, Ordering::SeqCst);
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
                Act::Mutate(name) => {
                    let mutated = self.mutated.clone();
                    r.mutate(name, |_| async move {
                        mutated.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await?
                }
                Act::Preview => r.preview([View::summary([("About to", "write things")])]),
                Act::Resource(name) => r.set_resource(name),
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

struct RecordingConfirmer {
    specs: Mutex<Vec<ConfirmSpec>>,
    answer: Result<bool, &'static str>,
}

impl RecordingConfirmer {
    fn new(answer: Result<bool, &'static str>) -> Arc<Self> {
        Arc::new(RecordingConfirmer {
            specs: Mutex::new(Vec::new()),
            answer,
        })
    }
}

#[async_trait]
impl Confirmer for RecordingConfirmer {
    async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool> {
        self.specs.lock().unwrap().push(spec);
        self.answer.map_err(|e| anyhow::anyhow!(e))
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
    assert_eq!(op.runs.load(Ordering::SeqCst), 0);

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
    assert_eq!(op.runs.load(Ordering::SeqCst), 0);
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
    let mut op = script(Class::Mutate, vec![Act::Step("change")]);
    op.params = vec![Param::new("mode").default("fast")];
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
    assert_eq!(v["params"], serde_json::json!({"x": "1", "mode": "fast"}));
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

    let broken = script(Class::Mutate, vec![Act::FailStep("change", "boom")]);
    let quiet = Runner::new(Deps::default())
        .yes(true)
        .execute(&broken, Input::default())
        .await
        .unwrap_err();
    let failing = Runner::new(Deps::default())
        .yes(true)
        .audit(Arc::new(FailingAudit))
        .execute(&broken, Input::default())
        .await
        .unwrap_err();
    assert_eq!(quiet.kind, FailureKind::Failed);
    assert_eq!(failing.kind, FailureKind::Failed);
    assert_eq!(format!("{:#}", failing.error), "change: boom");
    assert_eq!(format!("{:#}", quiet.error), format!("{:#}", failing.error));
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
    assert_eq!(entries.len(), 1);
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

#[tokio::test]
async fn confirm_asks_once_per_run_with_preview() {
    let op = writer(Class::Mutate);
    let confirmer = RecordingConfirmer::new(Ok(true));
    Runner::new(Deps::default())
        .confirmer(confirmer.clone())
        .execute(&op, Input::default())
        .await
        .unwrap();
    assert_eq!(op.mutated.load(Ordering::SeqCst), 2);
    let specs = confirmer.specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].op_id, "test.write");
    assert_eq!(specs[0].class, Class::Mutate);
    assert_eq!(specs[0].step, "write one");
    assert_eq!(specs[0].resource, "cfg-nonprod/tenants/acme.json");
    assert_eq!(specs[0].preview.len(), 1);
}

#[tokio::test]
async fn resource_defaults_to_op_id() {
    let mut op = writer(Class::Mutate);
    op.acts.retain(|a| !matches!(a, Act::Resource(_)));
    let confirmer = RecordingConfirmer::new(Ok(true));
    Runner::new(Deps::default())
        .confirmer(confirmer.clone())
        .execute(&op, Input::default())
        .await
        .unwrap();
    assert_eq!(confirmer.specs.lock().unwrap()[0].resource, "test.write");
}

#[tokio::test]
async fn decline_stops_before_mutation_and_audits_declined() {
    let op = writer(Class::Mutate);
    let audit = Arc::new(RecordingAudit::default());
    let f = Runner::new(Deps::default())
        .confirmer(RecordingConfirmer::new(Ok(false)))
        .audit(audit.clone())
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::Declined);
    assert_eq!(op.mutated.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.views,
        [View::summary([
            ("Operation", "test.write"),
            (
                "Outcome",
                "declined: stopped before the first mutating step"
            ),
        ])]
    );
    assert!(matches!(
        find_cause::<GuardError>(&f.error),
        Some(GuardError::NotConfirmed { .. })
    ));
    let entries = audit.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Outcome::Declined);
}

#[tokio::test]
async fn confirmer_error_is_a_failure_not_a_decline() {
    let op = writer(Class::Mutate);
    let audit = Arc::new(RecordingAudit::default());
    let f = Runner::new(Deps::default())
        .confirmer(RecordingConfirmer::new(Err("tty gone")))
        .audit(audit.clone())
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::Failed);
    assert!(format!("{:#}", f.error).contains("tty gone"));
    assert_eq!(op.mutated.load(Ordering::SeqCst), 0);
    let entries = audit.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Outcome::Error);
}

#[tokio::test]
async fn yes_covers_mutate_only() {
    let op = writer(Class::Mutate);
    Runner::new(Deps::default())
        .yes(true)
        .execute(&op, Input::default())
        .await
        .unwrap();
    assert_eq!(op.mutated.load(Ordering::SeqCst), 2);

    let op = writer(Class::Destructive);
    let confirmer = RecordingConfirmer::new(Ok(true));
    Runner::new(Deps::default())
        .yes(true)
        .confirmer(confirmer.clone())
        .execute(&op, Input::default())
        .await
        .unwrap();
    let specs = confirmer.specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].class, Class::Destructive);
}

#[tokio::test]
async fn yes_never_covers_destructive_without_a_confirmer() {
    let op = writer(Class::Destructive);
    let f = Runner::new(Deps::default())
        .yes(true)
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::Declined);
    assert_eq!(op.mutated.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.error.to_string(),
        "no confirmer available: operation not confirmed"
    );
}

#[tokio::test]
async fn missing_confirmer_fails_closed() {
    let op = writer(Class::Mutate);
    let f = Runner::new(Deps::default())
        .execute(&op, Input::default())
        .await
        .unwrap_err();
    assert_eq!(f.kind, FailureKind::Declined);
    assert_eq!(op.mutated.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dry_run_skips_confirmation_entirely() {
    let op = writer(Class::Mutate);
    let confirmer = RecordingConfirmer::new(Ok(true));
    let audit = Arc::new(RecordingAudit::default());
    let (log, sink) = progress_log();
    let result = Runner::new(Deps::default())
        .dry_run(true)
        .confirmer(confirmer.clone())
        .audit(audit.clone())
        .progress(sink)
        .execute(&op, Input::default())
        .await
        .unwrap();
    assert_eq!(op.mutated.load(Ordering::SeqCst), 0);
    assert!(confirmer.specs.lock().unwrap().is_empty());
    assert_eq!(
        result.views[0],
        View::summary([
            ("Operation", "test.write"),
            ("Outcome", "dry-run: stopped before the first mutating step"),
        ])
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|p| p.step == "write one" && p.status == StepStatus::Skipped)
    );
    let entries = audit.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].outcome, Outcome::DryRun);
    assert_eq!(entries[0].error, None);
}
