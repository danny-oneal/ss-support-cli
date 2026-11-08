//! In-process end-to-end tests: drive `ss::run_with` the way `main` does,
//! asserting on the exit code and the captured output.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use clap::CommandFactory;
use ss::{Cli, Exit, Io, Services, Sink};

/// Runs `args` against an isolated home under `root` with empty stdin,
/// returning the exit status and the captured stdout and stderr.
async fn ss(root: &Path, args: &[&str]) -> (Exit, String, String) {
    ss_with(Services::isolated(root), args).await
}

/// Runs `args` against `services` with empty stdin.
async fn ss_with(services: Services, args: &[&str]) -> (Exit, String, String) {
    let cli = parse(&services, args);
    let (io, captured) = Io::capture("");
    let exit = ss::run_with(cli, services, io).await;
    (exit, captured.stdout(), captured.stderr())
}

fn parse(services: &Services, args: &[&str]) -> Cli {
    ss::parse_from(&services.registry, args.iter().copied())
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
    assert_eq!(cmd.get_name(), "ss");
    assert_eq!(cmd.get_version(), Some(env!("CARGO_PKG_VERSION")));
}

#[tokio::test]
async fn bare_invocation_prints_help() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss(root.path(), &["ss"]).await;
    assert_eq!(exit, Exit::Ok);
    assert!(out.contains("Usage: ss"), "{out}");
    assert_eq!(err, "");
}

#[tokio::test]
async fn ops_list_shows_every_operation() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss(root.path(), &["ss", "ops", "list"]).await;
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
    let (exit, out, err) = ss(root.path(), &["ss", "ops", "describe", "demo.hello"]).await;
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
    let (exit, out, err) = ss(root.path(), &["ss", "ops", "describe", "nope"]).await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss ops describe: unknown operation \"nope\" (try `ss ops list`)\n"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn config_typo_exits_2_naming_the_key() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), ".config/ss/config.yaml", "envs: {}\n");
    let (exit, out, err) = ss(root.path(), &["ss", "ops", "list"]).await;
    assert_eq!(exit, Exit::Usage);
    assert!(err.starts_with("ss ops list: parse config "), "{err}");
    assert!(err.contains("unknown field `envs`"), "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
    assert_eq!(out, "");
}

#[tokio::test]
async fn presets_list_show_delete() {
    let root = tempfile::tempdir().expect("tempdir");
    write(
        root.path(),
        ".config/ss/presets.yaml",
        "presets:\n  demo.hello:\n    w:\n      name: world\n",
    );

    let (exit, out, err) = ss(root.path(), &["ss", "presets", "list"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("demo.hello") && out.contains('w'), "{out}");

    let (exit, out, err) = ss(root.path(), &["ss", "presets", "show", "demo.hello", "w"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("--name : world"), "{out}");

    let (exit, _, err) = ss(
        root.path(),
        &["ss", "presets", "show", "demo.hello", "nope"],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(err, "ss presets show: demo.hello has no preset \"nope\"\n");

    let (exit, out, err) = ss(root.path(), &["ss", "presets", "delete", "demo.hello", "w"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(out, "deleted preset \"w\" for demo.hello\n");

    let (exit, out, err) = ss(root.path(), &["ss", "presets", "list"]).await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(
        out,
        "no presets saved yet — use `ss run <op> ... --save-preset <name>`\n"
    );
}

#[tokio::test]
async fn unparsable_presets_file_is_usage() {
    let root = tempfile::tempdir().expect("tempdir");
    write(root.path(), ".config/ss/presets.yaml", "presets: [\n");
    let (exit, out, err) = ss(root.path(), &["ss", "presets", "list"]).await;
    assert_eq!(exit, Exit::Usage);
    assert!(err.starts_with("ss presets list: parse presets "), "{err}");
    assert_eq!(out, "");
}

#[tokio::test]
async fn deleting_a_missing_preset_is_usage() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss(root.path(), &["ss", "presets", "delete", "demo.hello", "w"]).await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(err, "ss presets delete: demo.hello has no preset \"w\"\n");
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
        &["ss", "ops", "list"][..],
        &["ss", "run", "demo.json-diff", "--left", &a, "--right", &b],
    ] {
        let cli = parse(&services, args);
        let (io, captured) = Io::capture("");
        let io = Io {
            stdout: Sink::new(ClosedPipe),
            ..io
        };
        let exit = ss::run_with(cli, services.clone(), io).await;
        assert_eq!(exit, Exit::Ok, "{args:?}");
        assert_eq!(captured.stderr(), "", "{args:?}");
    }
}

#[tokio::test]
async fn run_json_diff_text() {
    let (root, a, b) = diff_fixtures();
    let (exit, out, err) = ss(
        root.path(),
        &["ss", "run", "demo.json-diff", "--left", &a, "--right", &b],
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
            "ss",
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
            "ss",
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
        "ss run demo.json-diff: unknown output format \"xml\" (want text or json)\n"
    );
    assert_eq!(out, "");
    assert!(!report.exists());
}

#[tokio::test]
async fn missing_required_param_is_usage() {
    let (root, a, _) = diff_fixtures();
    let (exit, out, err) = ss(root.path(), &["ss", "run", "demo.json-diff", "--left", &a]).await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss run demo.json-diff: required param \"right\" missing\n"
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
            "ss",
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
        err.starts_with("ss run demo.json-diff: read both files: open "),
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
            "ss",
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
            "ss",
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
            "ss",
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
        err.starts_with(&format!("ss run demo.json-diff: export {report_arg}: ")),
        "{err}"
    );
}

#[tokio::test]
async fn save_and_replay_preset() {
    let (root, a, b) = diff_fixtures();
    let (exit, saved_out, err) = ss(
        root.path(),
        &[
            "ss",
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
    let presets =
        fs::read_to_string(root.path().join(".config/ss/presets.yaml")).expect("presets file");
    assert!(presets.contains("left"), "{presets}");
    assert!(!presets.contains("tolerance"), "{presets}");

    let (exit, out, err) = ss(
        root.path(),
        &["ss", "run", "demo.json-diff", "--preset", "p"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(out, saved_out);

    let (exit, out, err) = ss(
        root.path(),
        &["ss", "run", "demo.json-diff", "--preset", "nope"],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss run demo.json-diff: no preset \"nope\" for demo.json-diff (have: p); save one with --save-preset\n"
    );
    assert_eq!(out, "");
}

#[tokio::test]
async fn explicit_flags_override_the_preset() {
    let (root, a, b) = diff_fixtures();
    write(
        root.path(),
        ".config/ss/presets.yaml",
        &format!("presets:\n  demo.json-diff:\n    p:\n      left: {b}\n      right: {b}\n"),
    );
    let (exit, out, err) = ss(
        root.path(),
        &["ss", "run", "demo.json-diff", "--preset", "p", "--left", &a],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("~ a: 1 -> 2"), "{out}");
}

#[tokio::test]
async fn unknown_preset_with_none_saved_is_usage() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss(root.path(), &["ss", "run", "demo.hello", "--preset", "p"]).await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        err,
        "ss run demo.hello: no preset \"p\" for demo.hello (none saved); save one with --save-preset\n"
    );
}

#[tokio::test]
async fn env_flows_into_the_operation() {
    let root = tempfile::tempdir().expect("tempdir");
    write(
        root.path(),
        ".config/ss/config.yaml",
        "environments:\n  dev:\n    profile: p-dev\n",
    );
    let environment = |out: &str| {
        let doc: serde_json::Value = serde_json::from_str(out).expect("stdout is JSON");
        doc["views"][0]["fields"]["Environment"].clone()
    };

    let (exit, out, err) = ss(
        root.path(),
        &[
            "ss",
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
    services.vars.insert("SS_ENV".to_owned(), "dev".to_owned());
    let (exit, out, err) = ss_with(
        services,
        &["ss", "run", "demo.hello", "--name", "x", "-o", "json"],
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
        &["ss", "run", "demo.hello", "--name", "x", "-v"],
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
    let cli = parse(&services, &["ss", "run", "demo.hello", "--name", "x"]);
    let (io, captured) = Io::capture("");
    let exit = ss::run_with_interrupt(
        cli,
        services,
        io,
        tokio::time::sleep(Duration::from_millis(50)),
    )
    .await;
    assert_eq!(exit, Exit::Interrupted);
    assert_eq!(
        captured.stderr(),
        "ss run demo.hello: compose greeting: cancelled\n"
    );
    assert_eq!(captured.stdout(), "");
}
