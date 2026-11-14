//! The built binary under SIGTERM and SIGHUP: like Ctrl-C, the signal
//! cancels the running step, the one error line is printed, and the process
//! exits 130.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

fn interrupted_by(signal: &str) {
    let root = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ss-support"))
        .args(["run", "demo.hello", "--name", "x", "-v"])
        .env_clear()
        .env("HOME", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ss-support");
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
    let status = child.wait().expect("wait for ss-support");
    assert_eq!(
        status.code(),
        Some(130),
        "{signal}: {status:?}, stderr: {rest}"
    );
    assert_eq!(
        rest,
        "✗ compose greeting: cancelled\nss-support run demo.hello: compose greeting: cancelled\n",
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

/// The TUI on a pseudo-terminal with a stand-in `kubectl` first on `PATH`:
/// when a signal ends the process, the kubectl it started ends with it. The
/// pseudo-terminal comes from `script`, which is part of util-linux.
#[cfg(target_os = "linux")]
mod kubectl_children {
    use std::fs;
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    /// How long any one step may take before the test gives up.
    const PATIENCE: Duration = Duration::from_secs(20);

    /// One pod, as `kubectl get pods -o json` prints it.
    const ONE_POD: &str = r#"{"items":[{"metadata":{"name":"api-7d9f"},"status":{"phase":"Running","containerStatuses":[{"ready":true,"restartCount":0,"state":{"running":{}}}]}}]}"#;

    /// A TUI running under `script`.
    struct Session {
        root: tempfile::TempDir,
        script: Child,
        keys: ChildStdin,
        /// Everything the TUI has written to its terminal.
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Session {
        /// Starts the TUI with a `kubectl` that is a shell script: `cases`
        /// are the arms of a `case` on its arguments, in which `$ROOT` is a
        /// directory the test can read.
        fn start(cases: &str) -> Session {
            let root = tempfile::tempdir().expect("tempdir");
            let dir = root.path();
            let config = dir.join("home/.config/ss-support");
            fs::create_dir_all(&config).expect("config directory");
            fs::write(
                config.join("config.yaml"),
                "default_env: dev\nenvironments:\n  dev: {profile: test, region: us-west-2, kube_context: dev-cluster}\n",
            )
            .expect("write config");
            let bin = dir.join("bin");
            fs::create_dir_all(&bin).expect("bin directory");
            let kubectl = bin.join("kubectl");
            fs::write(
                &kubectl,
                format!(
                    "#!/bin/sh\nROOT='{}'\ncase \"$*\" in\n{cases}\n  *) echo \"unexpected: $*\" >&2; exit 1 ;;\nesac\n",
                    dir.display()
                ),
            )
            .expect("write the stand-in kubectl");
            fs::set_permissions(&kubectl, fs::Permissions::from_mode(0o755))
                .expect("make the stand-in executable");

            let command = format!(
                "stty rows 30 cols 100; echo $$ > '{}'; exec '{}'",
                dir.join("tui.pid").display(),
                env!("CARGO_BIN_EXE_ss-support")
            );
            let mut script = Command::new("script")
                .args(["-q", "-e", "-f", "-c", &command, "/dev/null"])
                .env_clear()
                .env("HOME", dir.join("home"))
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env("SHELL", "/bin/sh")
                .env("TERM", "xterm-256color")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn script (util-linux) to provide a pseudo-terminal");
            let keys = script.stdin.take().expect("piped stdin");
            let mut output = script.stdout.take().expect("piped stdout");
            let written = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&written);
            thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = output.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    sink.lock().expect("written").extend_from_slice(&chunk[..n]);
                }
            });
            Session {
                root,
                script,
                keys,
                written,
            }
        }

        fn file(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        /// What the TUI has written, without the sequences that position
        /// and colour it.
        fn text(&self) -> String {
            let raw = self.written.lock().expect("written").clone();
            let mut out = Vec::with_capacity(raw.len());
            let mut bytes = raw.iter().copied().peekable();
            while let Some(byte) = bytes.next() {
                if byte == 0x1b && bytes.peek() == Some(&b'[') {
                    bytes.next();
                    // Parameters, then the one byte that ends the sequence.
                    while bytes.next().is_some_and(|b| !(0x40..=0x7e).contains(&b)) {}
                } else {
                    out.push(byte);
                }
            }
            String::from_utf8_lossy(&out).into_owned()
        }

        fn wait_for_text(&self, wanted: &str) {
            let deadline = Instant::now() + PATIENCE;
            while !self.text().contains(wanted) {
                assert!(
                    Instant::now() < deadline,
                    "{wanted:?} never appeared in: {}",
                    self.text()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn press(&mut self, keys: &str) {
            self.keys.write_all(keys.as_bytes()).expect("send keys");
            self.keys.flush().expect("send keys");
        }

        /// The process id a script wrote to `name`, once it is there.
        fn pid_in(&self, name: &str) -> u32 {
            let deadline = Instant::now() + PATIENCE;
            loop {
                if let Ok(text) = fs::read_to_string(self.file(name))
                    && text.ends_with('\n')
                {
                    return text.trim().parse().expect("a process id");
                }
                assert!(
                    Instant::now() < deadline,
                    "{name} was never written; the screen: {}",
                    self.text()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }

        /// Sends `signal` to the TUI. Returns false when the TUI has
        /// already gone.
        fn signal(&self, signal: &str) -> bool {
            Command::new("kill")
                .arg(format!("-{signal}"))
                .arg(self.pid_in("tui.pid").to_string())
                .stderr(Stdio::null())
                .status()
                .expect("run kill")
                .success()
        }

        /// The exit code of the TUI, once it has ended.
        fn exit_code(&mut self) -> Option<i32> {
            let deadline = Instant::now() + PATIENCE;
            loop {
                if let Some(status) = self.script.try_wait().expect("wait for script") {
                    return status.code();
                }
                assert!(Instant::now() < deadline, "the TUI never ended");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for Session {
        /// Leaves nothing running, whatever the test found.
        fn drop(&mut self) {
            let _ = self.script.kill();
            let _ = self.script.wait();
            for name in ["tui.pid", "get.pid", "logs.pid"] {
                if let Ok(text) = fs::read_to_string(self.file(name)) {
                    let _ = Command::new("kill")
                        .args(["-KILL", text.trim()])
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
    }

    /// Whether process `pid` is still running; one that has been killed and
    /// not yet collected is not.
    fn is_running(pid: u32) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next());
        state != Some("Z")
    }

    fn assert_stopped(pid: u32, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while is_running(pid) {
            assert!(
                Instant::now() < deadline,
                "{what} (process {pid}) outlived the TUI"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A `kubectl get pods` that never answers. It ignores the hangup that
    /// reaches it when the pseudo-terminal closes behind the TUI, so that,
    /// as with a signal sent to the TUI alone on a terminal that stays open,
    /// only the TUI can stop it.
    const SILENT_LISTING: &str =
        r#"  *"get pods"*) trap '' HUP; echo $$ > "$ROOT/get.pid"; exec sleep 300 ;;"#;

    #[test]
    fn a_signal_stops_a_kubectl_that_has_not_answered() {
        let mut tui = Session::start(SILENT_LISTING);
        tui.wait_for_text("Operations");
        tui.press("p");
        let kubectl = tui.pid_in("get.pid");

        assert!(tui.signal("TERM"));

        assert_eq!(tui.exit_code(), Some(130));
        assert_stopped(kubectl, "kubectl get pods");
    }

    #[test]
    fn two_signals_at_once_stop_a_kubectl_that_has_not_answered() {
        let mut tui = Session::start(SILENT_LISTING);
        tui.wait_for_text("Operations");
        tui.press("p");
        let kubectl = tui.pid_in("get.pid");

        assert!(tui.signal("TERM"));
        // The first signal may already have ended the TUI.
        tui.signal("HUP");

        assert_eq!(tui.exit_code(), Some(130));
        assert_stopped(kubectl, "kubectl get pods");
    }

    #[test]
    fn a_signal_stops_the_kubectl_of_a_log_being_followed() {
        // Neither a write to the closed pipe nor the hangup of the closing
        // pseudo-terminal ends the stand-in, so only being stopped can.
        let cases = format!(
            "  *\"get pods\"*) echo '{ONE_POD}' ;;\n  *logs*) trap '' PIPE HUP; echo $$ > \"$ROOT/logs.pid\"; while true; do echo tick; sleep 0.05; done ;;"
        );
        let mut tui = Session::start(&cases);
        tui.wait_for_text("Operations");
        tui.press("p");
        tui.wait_for_text("api-7d9f");
        tui.press("\r");
        let kubectl = tui.pid_in("logs.pid");
        tui.wait_for_text("tick");

        assert!(tui.signal("HUP"));

        assert_eq!(tui.exit_code(), Some(130));
        assert_stopped(kubectl, "kubectl logs");
    }

    #[test]
    fn leaving_the_pod_list_stops_a_kubectl_that_has_not_answered() {
        let mut tui = Session::start(SILENT_LISTING);
        tui.wait_for_text("Operations");
        tui.press("p");
        let kubectl = tui.pid_in("get.pid");

        tui.press("\x1b");

        assert_stopped(kubectl, "kubectl get pods");
        tui.press("q");
        assert_eq!(tui.exit_code(), Some(0));
    }
}
