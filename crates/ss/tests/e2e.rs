//! In-process end-to-end tests: drive `ss::run_with` the way `main` does,
//! asserting on the exit code and the captured output.

use std::fs;
use std::path::Path;

use clap::CommandFactory;
use ss::{Cli, Exit, Io, Services};

/// Runs `args` against an isolated home under `root` with empty stdin,
/// returning the exit status and the captured stdout and stderr.
async fn ss(root: &Path, args: &[&str]) -> (Exit, String, String) {
    let services = Services::isolated(root);
    let cli = ss::parse_from(&services.registry, args.iter().copied())
        .unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
    let (io, captured) = Io::capture("");
    let exit = ss::run_with(cli, services, io).await;
    (exit, captured.stdout(), captured.stderr())
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
async fn interrupt_before_completion_does_not_disturb_listing() {
    let root = tempfile::tempdir().expect("tempdir");
    let services = Services::isolated(root.path());
    let cli = ss::parse_from(&services.registry, ["ss", "ops", "list"]).expect("parses");
    let (io, captured) = Io::capture("");
    let exit = ss::run_with_interrupt(cli, services, io, async {}).await;
    assert_eq!(exit, Exit::Ok);
    assert!(captured.stdout().contains("demo.hello"));
}
