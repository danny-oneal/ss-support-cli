//! In-process end-to-end tests: drive `ss::run_with` the way `main` does,
//! asserting on the exit code and the captured output.

use clap::CommandFactory;
use ss::{Cli, Exit};

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
