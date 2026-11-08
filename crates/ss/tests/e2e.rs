//! In-process end-to-end tests: drive `ss::run_with` the way `main` does,
//! asserting on the exit code and the captured output.

use clap::{CommandFactory, Parser};
use ss::{Cli, Exit};

#[test]
fn exit_codes_follow_the_house_scheme() {
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
async fn no_arguments_runs_and_exits_ok() {
    let cli = Cli::parse_from(["ss"]);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = ss::run_with(cli, &mut out, &mut err).await;
    assert_eq!(code, Exit::Ok);
    assert!(err.is_empty(), "stderr: {}", String::from_utf8_lossy(&err));
}
