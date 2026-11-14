//! In-process end-to-end tests: drive `ss_support::run_with` the way `main` does,
//! asserting on the exit code and the captured output.

use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use async_trait::async_trait;
use clap::CommandFactory;
use ss_core::guard::Class;
use ss_core::ops::{Input, Meta, OpResult, Operation, Param, Runner, View};
use ss_support::{Cli, Exit, Io, Services, Sink, Source};

/// Runs `args` against an isolated home under `root` with empty stdin,
/// returning the exit status and the captured stdout and stderr.
async fn ss(root: &Path, args: &[&str]) -> (Exit, String, String) {
    ss_with(Services::isolated(root), args).await
}

/// Runs `args` against `services` with empty stdin.
async fn ss_with(services: Services, args: &[&str]) -> (Exit, String, String) {
    let cli = parse(&services, args);
    let (io, captured) = Io::capture("");
    let exit = ss_support::run_with(cli, services, io).await;
    (exit, captured.stdout(), captured.stderr())
}

fn parse(services: &Services, args: &[&str]) -> Cli {
    ss_support::parse_from(&services.registry, args.iter().copied())
        .unwrap_or_else(|e| panic!("parse {args:?}: {e}"))
}

/// A home under a fresh temp dir holding the two diff fixtures, plus their
/// absolute paths.
fn diff_fixtures() -> (tempfile::TempDir, String, String) {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), "a.json", r#"{"a":1,"b":[1,2]}"#);
    write(root.path(), "b.json", r#"{"a":2,"b":[1,2,3]}"#);
    let path = |name: &str| root.path().join(name).display().to_string();
    let (a, b) = (path("a.json"), path("b.json"));
    (root, a, b)
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("has a parent")).expect("create dirs");
    fs::write(path, body).expect("write file");
}

#[test]
fn exit_codes_are_distinct_and_stable() {
    assert_eq!(Exit::Ok.code(), 0);
    assert_eq!(Exit::Failed.code(), 1);
    assert_eq!(Exit::Usage.code(), 2);
    assert_eq!(Exit::Interrupted.code(), 130);
}

#[test]
fn version_is_the_workspace_version() {
    let cmd = Cli::command();
    assert_eq!(cmd.get_name(), "ss-support");
    assert_eq!(cmd.get_version(), Some(env!("CARGO_PKG_VERSION")));
}

#[tokio::test]
async fn bare_invocation_prints_help() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss(root.path(), &["ss-support"]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("Usage: ss-support"), "{out}");
    assert_eq!(err, "");
}

#[tokio::test]
async fn ops_list_shows_every_operation() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss(root.path(), &["ss-support", "ops", "list"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(
        out.lines().next().is_some_and(|l| l.starts_with("ID")),
        "{out}"
    );
    for want in ["demo.hello", "demo.json-diff", "read"] {
        assert!(out.contains(want), "missing {want:?} in {out}");
    }
}

#[tokio::test]
async fn ops_describe_lists_params() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss(
        root.path(),
        &["ss-support", "ops", "describe", "demo.hello"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    for want in [
        "ID       : demo.hello",
        "--name",
        "Who to greet",
        "--repeat",
        "int",
        "3",
        "yes",
    ] {
        assert!(out.contains(want), "missing {want:?} in {out}");
    }
}

#[tokio::test]
async fn ops_describe_unknown_is_usage() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss(root.path(), &["ss-support", "ops", "describe", "nope"]).await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support ops describe: unknown operation \"nope\" (try `ss-support ops list`)\n"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn config_typo_exits_2_naming_the_key() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), ".config/ss-support/config.yaml", "envs: {}\n");
    let (exit, out, err) = ss(root.path(), &["ss-support", "ops", "list"]).await;
    assert_eq!(exit, Exit::Usage);
    assert!(
        err.starts_with("ss-support ops list: parse config "),
        "{err}"
    );
    assert!(err.contains("unknown field `envs`"), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
    assert_eq!(out, "");
}

#[tokio::test]
async fn presets_list_show_delete() {
    let root = tempfile::tempdir().expect("tempdir");
    write(
        root.path(),
        ".config/ss-support/presets.yaml",
        "presets:\n  demo.hello:\n    w:\n      name: world\n",
    );

    let (exit, out, err) = ss(root.path(), &["ss-support", "presets", "list"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("demo.hello") && out.contains('w'), "{out}");

    let (exit, out, err) = ss(
        root.path(),
        &["ss-support", "presets", "show", "demo.hello", "w"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("--name : world"), "{out}");

    let (exit, _, err) = ss(
        root.path(),
        &["ss-support", "presets", "show", "demo.hello", "nope"],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support presets show: demo.hello has no preset \"nope\"\n"
    );

    let (exit, out, err) = ss(
        root.path(),
        &["ss-support", "presets", "delete", "demo.hello", "w"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(out, "deleted preset \"w\" for demo.hello\n");

    let (exit, out, err) = ss(root.path(), &["ss-support", "presets", "list"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(
        out,
        "no presets saved yet — use `ss-support run <op> ... --save-preset <name>`\n"
    );
}

#[tokio::test]
async fn unparsable_presets_file_is_usage() {
    let root = tempfile::tempdir().expect("tempdir");
    write(
        root.path(),
        ".config/ss-support/presets.yaml",
        "presets: [\n",
    );
    let (exit, out, err) = ss(root.path(), &["ss-support", "presets", "list"]).await;
    assert_eq!(exit, Exit::Usage);
    assert!(
        err.starts_with("ss-support presets list: parse presets "),
        "{err}"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn deleting_a_missing_preset_is_usage() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss(
        root.path(),
        &["ss-support", "presets", "delete", "demo.hello", "w"],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support presets delete: demo.hello has no preset \"w\"\n"
    );
}

#[tokio::test]
async fn closed_stdout_exits_0() {
    struct ClosedPipe;
    impl Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
    let (root, a, b) = diff_fixtures();
    let services = Services::isolated(root.path());
    for args in [
        &["ss-support", "ops", "list"][..],
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
        ],
    ] {
        let cli = parse(&services, args);
        let (io, captured) = Io::capture("");
        let io = Io {
            stdout: Sink::new(ClosedPipe),
            ..io
        };
        let exit = ss_support::run_with(cli, services.clone(), io).await;
        assert_eq!(exit, Exit::Ok, "{args:?}");
        assert_eq!(captured.stderr(), "", "{args:?}");
    }
}

#[tokio::test]
async fn run_json_diff_text() {
    let (root, a, b) = diff_fixtures();
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("diff a.json → b.json"), "{out}");
    assert!(out.contains("~ a: 1 -> 2"), "{out}");
    assert_eq!(err, "");
}

#[tokio::test]
async fn run_json_diff_json() {
    let (root, a, b) = diff_fixtures();
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    let doc: serde_json::Value = serde_json::from_str(&out).expect("stdout is JSON");
    assert_eq!(doc["views"][0]["type"], "summary");
    assert_eq!(doc["views"][1]["delta"]["stats"]["changed"], 1);
    assert!(out.contains("\"new\": 2"), "{out}");
    assert!(!out.contains("2.0"), "{out}");
    assert!(out.ends_with("}\n"), "{out}");
}

#[tokio::test]
async fn unknown_output_format_is_usage_before_running() {
    let (root, a, b) = diff_fixtures();
    let report = root.path().join("r.md");
    let report_arg = report.display().to_string();
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
            "-o",
            "xml",
            "--export",
            &report_arg,
        ],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support run demo.json-diff: unknown output format \"xml\" (want text or json)\n"
    );
    assert_eq!(out, "");
    assert!(!report.exists());
}

#[tokio::test]
async fn missing_required_param_is_usage() {
    let (root, a, _) = diff_fixtures();
    let (exit, out, err) = ss(
        root.path(),
        &["ss-support", "run", "demo.json-diff", "--left", &a],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support run demo.json-diff: required param \"right\" missing\n"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn step_failure_exits_1() {
    let (root, _, b) = diff_fixtures();
    let missing = root.path().join("missing.json").display().to_string();
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &missing,
            "--right",
            &b,
        ],
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert!(
        err.starts_with("ss-support run demo.json-diff: read both files: open "),
        "{err}"
    );
    assert_eq!(err.lines().count(), 1, "{err}");
    assert_eq!(out, "");
}

#[tokio::test]
async fn dry_run_on_a_read_op_runs_normally() {
    let (root, a, b) = diff_fixtures();
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
            "--dry-run",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("~ a: 1 -> 2"), "{out}");
}

#[tokio::test]
async fn export_writes_markdown() {
    let (root, a, b) = diff_fixtures();
    let report = root.path().join("r.md");
    let report_arg = report.display().to_string();
    let (exit, _, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
            "--export",
            &report_arg,
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    let md = fs::read_to_string(&report).expect("report written");
    assert!(md.starts_with("# Diff two JSON files"), "{md}");
    assert!(md.contains("`tolerance=0`"), "{md}");
    assert_eq!(err, format!("exported to {report_arg}\n"));
}

#[tokio::test]
async fn export_failure_exits_1() {
    let (root, a, b) = diff_fixtures();
    let report = root.path().join("no-such-dir").join("r.md");
    let report_arg = report.display().to_string();
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
            "--export",
            &report_arg,
        ],
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert!(out.contains("~ a: 1 -> 2"), "{out}");
    assert!(
        err.starts_with(&format!(
            "ss-support run demo.json-diff: export {report_arg}: "
        )),
        "{err}"
    );
}

#[tokio::test]
async fn save_and_replay_preset() {
    let (root, a, b) = diff_fixtures();
    let (exit, saved_out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
            "--save-preset",
            "p",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(
        err.contains("saved preset \"p\" for demo.json-diff"),
        "{err}"
    );
    let presets = fs::read_to_string(root.path().join(".config/ss-support/presets.yaml"))
        .expect("presets file");
    assert!(presets.contains("left"), "{presets}");
    assert!(!presets.contains("tolerance"), "{presets}");

    let (exit, out, err) = ss(
        root.path(),
        &["ss-support", "run", "demo.json-diff", "--preset", "p"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(out, saved_out);

    let (exit, out, err) = ss(
        root.path(),
        &["ss-support", "run", "demo.json-diff", "--preset", "nope"],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support run demo.json-diff: no preset \"nope\" for demo.json-diff (have: p); save one with --save-preset\n"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn explicit_flags_override_the_preset() {
    let (root, a, b) = diff_fixtures();
    write(
        root.path(),
        ".config/ss-support/presets.yaml",
        &format!("presets:\n  demo.json-diff:\n    p:\n      left: {b}\n      right: {b}\n"),
    );
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--preset",
            "p",
            "--left",
            &a,
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("~ a: 1 -> 2"), "{out}");
}

#[tokio::test]
async fn unknown_preset_with_none_saved_is_usage() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss(
        root.path(),
        &["ss-support", "run", "demo.hello", "--preset", "p"],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support run demo.hello: no preset \"p\" for demo.hello (none saved); save one with --save-preset\n"
    );
}

#[tokio::test]
async fn env_flows_into_the_operation() {
    let root = tempfile::tempdir().expect("tempdir");
    write(
        root.path(),
        ".config/ss-support/config.yaml",
        "environments:\n  dev:\n    profile: p-dev\n",
    );
    let environment = |out: &str| {
        let doc: serde_json::Value = serde_json::from_str(out).expect("stdout is JSON");
        doc["views"][0]["fields"]["Environment"].clone()
    };

    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.hello",
            "--name",
            "x",
            "--env",
            "dev",
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(environment(&out), "dev");

    let mut services = Services::isolated(root.path());
    services
        .vars
        .insert("SS_SUPPORT_ENV".to_owned(), "dev".to_owned());
    let (exit, out, err) = ss_with(
        services,
        &[
            "ss-support",
            "run",
            "demo.hello",
            "--name",
            "x",
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(environment(&out), "dev");
}

#[tokio::test]
async fn verbose_prints_progress() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss(
        root.path(),
        &["ss-support", "run", "demo.hello", "--name", "x", "-v"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(err.contains("→ compose greeting ...\n"), "{err}");
    assert!(err.contains("✓ compose greeting ("), "{err}");
    assert!(err.contains("✓ tabulate ("), "{err}");
}

#[tokio::test]
async fn ctrl_c_mid_run_exits_130() {
    let root = tempfile::tempdir().expect("tempdir");
    let services = Services::isolated(root.path());
    let cli = parse(
        &services,
        &["ss-support", "run", "demo.hello", "--name", "x"],
    );
    let (io, captured) = Io::capture("");
    let exit = ss_support::run_with_interrupt(
        cli,
        services,
        io,
        tokio::time::sleep(Duration::from_millis(50)),
    )
    .await;
    assert_eq!(exit, Exit::Interrupted);
    assert_eq!(
        captured.stderr(),
        "ss-support run demo.hello: compose greeting: cancelled\n"
    );
    assert_eq!(captured.stdout(), "");
}

/// A mutating operation that changes nothing but counts its runs.
struct Touch {
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Operation for Touch {
    fn meta(&self) -> Meta {
        Meta {
            id: "test.touch",
            name: "Touch",
            summary: "Mutates nothing",
            category: "Test",
            class: Class::Mutate,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![Param::new("target").default("x")]
    }

    async fn run(&self, r: &mut Runner, _input: &Input) -> anyhow::Result<OpResult> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        r.mutate("touch", |_| async { Ok(()) }).await?;
        Ok(OpResult {
            views: vec![View::summary([("Touched", "yes")])],
        })
    }
}

/// Isolated services under `root` whose registry also holds [`Touch`], with
/// `dev` allowlisted; returns the services and Touch's run counter.
fn touch_services(root: &Path) -> (Services, Arc<AtomicUsize>) {
    write(
        root,
        ".config/ss-support/config.yaml",
        "environments:\n  dev: {}\n",
    );
    let runs = Arc::new(AtomicUsize::new(0));
    let mut services = Services::isolated(root);
    services
        .registry
        .register(Arc::new(Touch {
            runs: Arc::clone(&runs),
        }))
        .expect("register test.touch");
    (services, runs)
}

/// A stdin that blocks until its sender is dropped, then reads as empty.
struct Gate(mpsc::Receiver<()>);

impl Read for Gate {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        let _ = self.0.recv();
        Ok(0)
    }
}

#[tokio::test]
async fn bad_op_param_value_is_usage() {
    let (root, a, b) = diff_fixtures();
    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss-support",
            "run",
            "demo.json-diff",
            "--left",
            &a,
            "--right",
            &b,
            "--tolerance",
            "abc",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support run demo.json-diff: invalid tolerance \"abc\": invalid float literal\n"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn unreadable_config_exits_1() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut services = Services::isolated(root.path());
    services.vars.insert(
        "SS_SUPPORT_CONFIG".to_owned(),
        root.path().display().to_string(),
    );
    let (exit, out, err) = ss_with(services, &["ss-support", "ops", "list"]).await;
    assert_eq!(exit, Exit::Failed);
    assert!(
        err.starts_with("ss-support ops list: read config "),
        "{err}"
    );
    assert_eq!(err.lines().count(), 1, "{err}");
    assert_eq!(out, "");
}

#[tokio::test]
async fn save_preset_without_params_is_usage_before_running() {
    let root = tempfile::tempdir().expect("tempdir");
    let (services, runs) = touch_services(root.path());
    let (exit, out, err) = ss_with(
        services,
        &[
            "ss-support",
            "run",
            "test.touch",
            "--env",
            "dev",
            "--yes",
            "--save-preset",
            "p",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss-support run test.touch: refusing to save an empty preset\n"
    );
    assert_eq!(out, "");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(
        !root
            .path()
            .join(".local/state/ss-support/audit.log")
            .exists()
    );
    assert!(!root.path().join(".config/ss-support/presets.yaml").exists());
}

#[tokio::test]
async fn eof_at_the_prompt_ends_the_prompt_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let (services, _) = touch_services(root.path());
    let (exit, out, err) = ss_with(
        services,
        &["ss-support", "run", "test.touch", "--env", "dev"],
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        err,
        "Proceed with test.touch? [y/N] \nss-support run test.touch: operation not confirmed\n"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn interrupt_at_the_prompt_ends_the_prompt_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let (services, runs) = touch_services(root.path());
    let cli = parse(
        &services,
        &["ss-support", "run", "test.touch", "--env", "dev"],
    );
    let (release, gate) = mpsc::channel();
    let (io, captured) = Io::capture("");
    let io = Io {
        stdin: Source::new(BufReader::new(Gate(gate))),
        ..io
    };
    let exit = ss_support::run_with_interrupt(
        cli,
        services,
        io,
        tokio::time::sleep(Duration::from_millis(100)),
    )
    .await;
    drop(release);
    assert_eq!(exit, Exit::Interrupted);
    assert_eq!(
        captured.stderr(),
        "Proceed with test.touch? [y/N] \nss-support run test.touch: touch: cancelled\n"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let audit = fs::read_to_string(root.path().join(".local/state/ss-support/audit.log"))
        .expect("interrupted run is audited");
    assert!(audit.contains("\"outcome\":\"error\""), "{audit}");
}

#[tokio::test]
async fn audit_write_failure_warns_and_keeps_the_exit_code() {
    let root = tempfile::tempdir().expect("tempdir");
    let (mut services, _) = touch_services(root.path());
    write(root.path(), "state", "a regular file, not a directory");
    services.vars.insert(
        "XDG_STATE_HOME".to_owned(),
        root.path().join("state").display().to_string(),
    );
    let (exit, out, err) = ss_with(
        services,
        &["ss-support", "run", "test.touch", "--env", "dev", "--yes"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(err.starts_with("warning: audit log: "), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(out.contains("Touched"), "{out}");
}

/// The completion script for `shell`, which must be produced without
/// anything on stderr.
async fn completion_script(root: &Path, shell: &str) -> String {
    let (exit, out, err) = ss(root, &["ss-support", "completion", shell]).await;
    assert_eq!(exit, Exit::Ok, "{shell}: {err}");
    assert_eq!(err, "", "{shell}");
    out
}

#[tokio::test]
async fn completion_scripts_name_every_operation_and_flag() {
    let root = tempfile::tempdir().expect("tempdir");
    for shell in ["bash", "zsh", "fish"] {
        let script = completion_script(root.path(), shell).await;

        for op in ss_core::ops::builtin().all() {
            let id = op.meta().id;
            assert!(script.contains(id), "{shell}: no {id}");
        }
        for word in ["presets", "describe"] {
            assert!(script.contains(word), "{shell}: no {word}");
        }
        for flag in ["bucket", "dry-run", "save-preset"] {
            // fish names a long flag without its dashes.
            let want = if shell == "fish" {
                format!("-l {flag}")
            } else {
                format!("--{flag}")
            };
            assert!(script.contains(&want), "{shell}: no {want}");
        }
    }
}

#[tokio::test]
async fn each_completion_script_is_in_its_shell_s_own_form() {
    let root = tempfile::tempdir().expect("tempdir");

    let bash = completion_script(root.path(), "bash").await;
    assert!(bash.contains("complete -F _ss__support"), "{bash:.300}");
    let zsh = completion_script(root.path(), "zsh").await;
    assert!(zsh.starts_with("#compdef ss-support"), "{zsh:.300}");
    let fish = completion_script(root.path(), "fish").await;
    assert!(fish.contains("complete -c ss-support"), "{fish:.300}");
}

#[test]
fn an_unknown_shell_is_refused_naming_the_three() {
    let services = Services::isolated(Path::new("/nonexistent"));
    let err = ss_support::parse_from(
        &services.registry,
        ["ss-support", "completion", "powershell"],
    )
    .expect_err("only bash, zsh, and fish are offered");

    let message = err.to_string();
    for shell in ["bash", "zsh", "fish"] {
        assert!(message.contains(shell), "{message}");
    }
    assert_eq!(err.exit_code(), 2);
}

#[tokio::test]
async fn completion_reads_no_config() {
    let root = tempfile::tempdir().expect("tempdir");
    write(
        root.path(),
        ".config/ss-support/config.yaml",
        "not: [valid: config",
    );

    let script = completion_script(root.path(), "bash").await;

    assert!(script.contains("s3.version-diff"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_bash_script_loads_in_bash() {
    let root = tempfile::tempdir().expect("tempdir");
    let script = root.path().join("ss-support.bash");
    fs::write(&script, completion_script(root.path(), "bash").await).expect("write script");

    let out = std::process::Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r#"source "$1" && complete -p ss-support"#,
            "bash",
        ])
        .arg(&script)
        .output()
        .expect("run bash");

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("-F _ss__support ss-support"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

#[cfg(unix)]
#[tokio::test]
async fn the_zsh_script_loads_in_zsh() {
    let root = tempfile::tempdir().expect("tempdir");
    let script = root.path().join("_ss-support");
    fs::write(&script, completion_script(root.path(), "zsh").await).expect("write script");

    let run = std::process::Command::new("zsh")
        .args([
            "-f",
            "-c",
            r#"autoload -Uz compinit && compinit -u -D && source "$1" && print -r -- "$_comps[ss-support]""#,
            "zsh",
        ])
        .arg(&script)
        .env("HOME", root.path())
        .output();
    let out = match run {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping: zsh is not installed");
            return;
        }
        Err(e) => panic!("run zsh: {e}"),
    };

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "_ss-support",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// What bash offers for the last of `words`, the line being `words`.
#[cfg(unix)]
async fn bash_offers(root: &Path, words: &[&str]) -> Vec<String> {
    let script = root.join("ss-support.bash");
    fs::write(&script, completion_script(root, "bash").await).expect("write script");
    let last = words.len() - 1;
    let prev = if last == 0 { "" } else { words[last - 1] };
    let program = format!(
        r#"source "$1" && COMP_WORDS=({line}) && COMP_CWORD={last} && _ss__support ss-support '{cur}' '{prev}' && printf '%s\n' "${{COMPREPLY[@]}}""#,
        line = words.join(" "),
        cur = words[last],
    );
    let out = std::process::Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &program, "bash"])
        .arg(&script)
        .output()
        .expect("run bash");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn bash_completes_subcommands_operations_and_their_flags() {
    let root = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        bash_offers(root.path(), &["ss-support", "pre"]).await,
        ["presets"]
    );
    assert_eq!(
        bash_offers(root.path(), &["ss-support", "run", "s3.re"]).await,
        ["s3.restore-version"]
    );
    assert_eq!(
        bash_offers(root.path(), &["ss-support", "run", "ddb.item-"]).await,
        ["ddb.item-compare", "ddb.item-edit", "ddb.item-history"]
    );
    let flags = bash_offers(
        root.path(),
        &["ss-support", "run", "s3.restore-version", "--"],
    )
    .await;
    for want in ["--bucket", "--key", "--version", "--dry-run", "--preset"] {
        assert!(
            flags.iter().any(|flag| flag == want),
            "no {want} in {flags:?}"
        );
    }
    assert_eq!(
        bash_offers(root.path(), &["ss-support", "presets", "sh"]).await,
        ["show"]
    );
}
