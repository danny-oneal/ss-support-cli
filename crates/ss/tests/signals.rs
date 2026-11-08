//! The built binary under SIGTERM and SIGHUP: like Ctrl-C, the signal
//! cancels the running step, the one error line is printed, and the process
//! exits 130.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

fn interrupted_by(signal: &str) {
    let root = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ss"))
        .args(["run", "demo.hello", "--name", "x", "-v"])
        .env_clear()
        .env("HOME", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ss");
    let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr"));

    // The first progress line means the run is under way, so the signal
    // handlers are installed.
    let mut first = String::new();
    stderr.read_line(&mut first).expect("read progress");
    assert_eq!(first, "→ compose greeting ...\n");

    let sent = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(sent.success(), "kill -{signal} failed");

    let mut rest = String::new();
    stderr.read_to_string(&mut rest).expect("read stderr");
    let status = child.wait().expect("wait for ss");
    assert_eq!(
        status.code(),
        Some(130),
        "{signal}: {status:?}, stderr: {rest}"
    );
    assert_eq!(
        rest, "✗ compose greeting: cancelled\nss run demo.hello: compose greeting: cancelled\n",
        "{signal}"
    );
}

#[test]
fn sigterm_cancels_the_run_and_exits_130() {
    interrupted_by("TERM");
}

#[test]
fn sighup_cancels_the_run_and_exits_130() {
    interrupted_by("HUP");
}

#[test]
fn sigint_cancels_the_run_and_exits_130() {
    interrupted_by("INT");
}
