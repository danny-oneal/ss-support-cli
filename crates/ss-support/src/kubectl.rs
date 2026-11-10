//! The Kubernetes port backed by the `kubectl` binary.
//!
//! Every call runs `kubectl --context <ctx> [-n <ns>] get …` or `… logs …`
//! and nothing else; the adapter never runs a command that changes cluster
//! state. A missing resource becomes [`SvcError::NotFound`]; every other
//! failure is reported with kubectl's own message. Each child is spawned with
//! `kill_on_drop`, so dropping a call or a log receiver never leaves kubectl
//! running.

use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use ss_core::config::Environment;
use ss_core::svc::SvcError;
use ss_core::svc::kubectl::{KubectlPort, LOG_BUFFER, LOG_TAIL};
use ss_core::vars::Vars;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::services::KubectlFactory;

/// The most output one `get` or `list` call may return: 32 MiB.
pub(crate) const MAX_OUTPUT: usize = 32 << 20;

/// How much of a child's stderr is kept for error messages: the last 64 KiB.
const STDERR_TAIL: usize = 64 << 10;

/// How [`KubectlExec`] runs kubectl.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// The kubectl executable: a path, or a bare name looked up on `PATH`.
    pub binary: PathBuf,
    /// The most stdout bytes one `get` or `list` call may produce.
    pub max_output: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("kubectl"),
            max_output: MAX_OUTPUT,
        }
    }
}

/// [`KubectlPort`] that shells out to `kubectl`, always against an explicit
/// context.
#[derive(Debug, Clone)]
pub(crate) struct KubectlExec {
    context: String,
    opts: Options,
}

impl KubectlExec {
    /// Builds the adapter for `context`. Nothing is run until a call is made.
    pub(crate) fn new(context: impl Into<String>, opts: Options) -> Self {
        Self {
            context: context.into(),
            opts,
        }
    }

    /// The full argument list: `--context <ctx>`, then `-n <namespace>` unless
    /// the namespace is empty, then `rest`.
    fn args(&self, namespace: &str, rest: &[&str]) -> Vec<String> {
        let mut args = vec!["--context".to_string(), self.context.clone()];
        if !namespace.is_empty() {
            args.push("-n".to_string());
            args.push(namespace.to_string());
        }
        args.extend(rest.iter().map(|s| (*s).to_string()));
        args
    }

    /// Spawns kubectl with piped output, no stdin, and `kill_on_drop` set.
    /// Returns the child with its stdout and stderr handles taken.
    ///
    /// # Errors
    ///
    /// Fails when kubectl cannot be started; a missing binary gets an
    /// actionable message.
    fn spawn(
        &self,
        namespace: &str,
        rest: &[&str],
    ) -> Result<(Child, ChildStdout, ChildStderr), SvcError> {
        let mut child = Command::new(&self.opts.binary)
            .args(self.args(namespace, rest))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| self.spawn_error(&e, rest))?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(SvcError::Other(anyhow!(
                "kubectl {}: output pipes unavailable",
                rest.join(" ")
            )));
        };
        Ok((child, stdout, stderr))
    }

    fn spawn_error(&self, err: &io::Error, rest: &[&str]) -> SvcError {
        if err.kind() == io::ErrorKind::NotFound {
            return SvcError::Other(anyhow!(
                "kubectl not found at {:?} (install kubectl or put it on PATH)",
                self.opts.binary.display().to_string()
            ));
        }
        SvcError::Other(anyhow!("kubectl {}: {err}", rest.join(" ")))
    }

    /// Runs one `kubectl get …` call to completion and returns its stdout.
    ///
    /// # Errors
    ///
    /// [`SvcError::NotFound`] when kubectl reports the resource missing;
    /// otherwise a failure to start kubectl, output over the limit, or
    /// kubectl's own error message.
    async fn run_get(&self, namespace: &str, rest: &[&str]) -> Result<Vec<u8>, SvcError> {
        let shown = rest.join(" ");
        let (mut child, mut stdout, mut stderr) = self.spawn(namespace, rest)?;
        let max = self.opts.max_output;
        let read_out = async {
            let mut body = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = stdout
                    .read(&mut chunk)
                    .await
                    .map_err(|e| SvcError::Other(anyhow!("kubectl {shown}: read output: {e}")))?;
                if n == 0 {
                    return Ok(body);
                }
                if body.len() + n > max {
                    return Err(SvcError::Other(anyhow!(
                        "kubectl {shown}: output exceeds the {max} byte limit"
                    )));
                }
                body.extend_from_slice(&chunk[..n]);
            }
        };
        let read_err = async { Ok::<_, SvcError>(read_tail(&mut stderr, STDERR_TAIL).await) };
        // Either side failing returns at once; dropping `child` then kills it.
        let (body, errtext) = tokio::try_join!(read_out, read_err)?;
        let status = child
            .wait()
            .await
            .map_err(|e| SvcError::Other(anyhow!("kubectl {shown}: wait: {e}")))?;
        if status.success() {
            return Ok(body);
        }
        let errtext = String::from_utf8_lossy(&errtext);
        let errtext = errtext.trim();
        if is_not_found(errtext) {
            return Err(SvcError::not_found(format!("kubectl {shown}: {errtext}")));
        }
        Err(SvcError::Other(if errtext.is_empty() {
            anyhow!("kubectl {shown}: {}", exit_description(status))
        } else {
            anyhow!("kubectl {shown}: {errtext}")
        }))
    }
}

/// Whether kubectl's stderr says the resource does not exist.
fn is_not_found(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("notfound") || lower.contains("not found")
}

/// How a failed child ended: its exit code, or that a signal ended it.
fn exit_description(status: std::process::ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by a signal".to_string(),
        |code| format!("exit status {code}"),
    )
}

/// A [`KubectlPort`] that cannot reach a cluster: every call fails with the
/// message it was built with.
#[derive(Debug, Clone)]
pub(crate) struct Unavailable {
    message: String,
}

impl Unavailable {
    /// A port whose every call fails with `message`.
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn error(&self) -> SvcError {
        SvcError::Other(anyhow!("{}", self.message))
    }
}

#[async_trait]
impl KubectlPort for Unavailable {
    async fn get_json(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, SvcError> {
        Err(self.error())
    }

    async fn list_json(&self, _: &str, _: &str) -> Result<Vec<u8>, SvcError> {
        Err(self.error())
    }

    async fn logs(&self, _: &str, _: &str, _: bool) -> Result<mpsc::Receiver<String>, SvcError> {
        Err(self.error())
    }
}

/// The Kubernetes source for real invocations: `kubectl` from `PATH`.
pub(crate) fn factory() -> KubectlFactory {
    factory_with(Options::default())
}

/// A Kubernetes source that runs kubectl as `opts` says. An environment
/// with no `kube_context` gets a port whose every call fails naming the fix,
/// so kubectl never falls back to its own current context.
pub(crate) fn factory_with(opts: Options) -> KubectlFactory {
    Arc::new(move |env: &Environment, _: &Vars| -> Arc<dyn KubectlPort> {
        if env.kube_context.is_empty() {
            return Arc::new(Unavailable::new(format!(
                "no Kubernetes context for environment {:?}: set kube_context in config.yaml or pass --context",
                env.name
            )));
        }
        Arc::new(KubectlExec::new(env.kube_context.clone(), opts.clone()))
    })
}

#[async_trait]
impl KubectlPort for KubectlExec {
    async fn get_json(
        &self,
        namespace: &str,
        resource: &str,
        name: &str,
    ) -> Result<Vec<u8>, SvcError> {
        self.run_get(namespace, &["get", resource, name, "-o", "json"])
            .await
    }

    async fn list_json(&self, namespace: &str, resource: &str) -> Result<Vec<u8>, SvcError> {
        self.run_get(namespace, &["get", resource, "-o", "json"])
            .await
    }

    async fn logs(
        &self,
        namespace: &str,
        pod: &str,
        follow: bool,
    ) -> Result<mpsc::Receiver<String>, SvcError> {
        let tail = LOG_TAIL.to_string();
        let mut rest = vec!["logs", pod, "--tail", tail.as_str()];
        if follow {
            rest.push("-f");
        }
        let (child, stdout, stderr) = self.spawn(namespace, &rest)?;
        let (tx, rx) = mpsc::channel(LOG_BUFFER);
        tokio::spawn(pump_logs(child, stdout, stderr, tx));
        Ok(rx)
    }
}

/// Forwards kubectl's stdout to `tx` line by line. Returns as soon as the
/// receiver is gone, which drops `child` and so kills it. When kubectl exits
/// with a failure and wrote to stderr, the last line sent is `error: {stderr}`.
async fn pump_logs(
    mut child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    tx: mpsc::Sender<String>,
) {
    // Drain stderr concurrently so a chatty kubectl never blocks on a full pipe.
    let errors = tokio::spawn(read_tail(stderr, STDERR_TAIL));
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = tokio::select! {
            () = tx.closed() => {
                errors.abort();
                return;
            }
            read = reader.read_until(b'\n', &mut line) => read,
        };
        if !matches!(read, Ok(n) if n > 0) {
            break;
        }
        if tx.send(line_text(&line)).await.is_err() {
            errors.abort();
            return;
        }
    }
    let failed = !child.wait().await.is_ok_and(|status| status.success());
    let errtext = errors.await.unwrap_or_default();
    let errtext = String::from_utf8_lossy(&errtext);
    let errtext = errtext.trim();
    if failed && !errtext.is_empty() {
        // The receiver may already be gone; there is nobody left to tell.
        let _ = tx.send(format!("error: {errtext}")).await;
    }
}

/// One log line without its line terminator; invalid UTF-8 is replaced.
fn line_text(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    String::from_utf8_lossy(raw).into_owned()
}

/// Reads `reader` to its end and returns only the last `cap` bytes, so a
/// chatty process cannot grow memory without bound. A read failure ends the
/// read early and keeps what arrived.
async fn read_tail(mut reader: impl AsyncRead + Unpin, cap: usize) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return tail,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > cap {
                    tail.drain(..tail.len() - cap);
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use ss_core::config::Environment;
    use ss_core::vars::Vars;
    use tempfile::TempDir;

    /// Held for a whole test. Writing a script while another test forks a
    /// child lets that child briefly inherit the open write handle, and
    /// executing the script then fails with "Text file busy"; running the stub
    /// tests one at a time rules that out.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        SERIAL.lock().await
    }

    fn stub(dir: &TempDir, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join("kubectl");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        path
    }

    fn exec(binary: &Path) -> KubectlExec {
        KubectlExec::new(
            "c",
            Options {
                binary: binary.to_path_buf(),
                ..Options::default()
            },
        )
    }

    fn sidecar(binary: &Path, ext: &str) -> PathBuf {
        let mut name = binary.as_os_str().to_os_string();
        name.push(ext);
        PathBuf::from(name)
    }

    fn recorded(binary: &Path) -> String {
        std::fs::read_to_string(sidecar(binary, ".args"))
            .expect("stub recorded its arguments")
            .trim()
            .to_string()
    }

    async fn pid_of(binary: &Path) -> String {
        let path = sidecar(binary, ".pid");
        for _ in 0..200 {
            if let Ok(text) = tokio::fs::read_to_string(&path).await
                && text.ends_with('\n')
            {
                return text.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the stub never wrote its pid");
    }

    async fn assert_gone(pid: &str) {
        for _ in 0..100 {
            let alive = tokio::process::Command::new("sh")
                .args(["-c", &format!("kill -0 {pid}")])
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .expect("run sh")
                .success();
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("process {pid} is still running");
    }

    async fn collect(mut rx: tokio::sync::mpsc::Receiver<String>) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        lines
    }

    #[test]
    fn classify_not_found() {
        assert!(is_not_found(
            r#"Error from server (NotFound): configmaps "x" not found"#
        ));
        assert!(is_not_found("error: pods \"p\" NOT FOUND"));
        assert!(!is_not_found(
            "Unable to connect to the server: dial tcp: i/o timeout"
        ));
    }

    #[tokio::test]
    async fn passes_context_namespace_and_args() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo "$@" >> "$0.args"; echo '{"ok":true}'"#);
        let out = KubectlExec::new(
            "dev-cluster",
            Options {
                binary: bin.clone(),
                ..Options::default()
            },
        )
        .get_json("menu", "configmap", "menu-config")
        .await
        .expect("get succeeds");
        assert_eq!(String::from_utf8_lossy(&out).trim(), r#"{"ok":true}"#);
        assert_eq!(
            recorded(&bin),
            "--context dev-cluster -n menu get configmap menu-config -o json"
        );
    }

    #[tokio::test]
    async fn an_empty_namespace_omits_the_flag() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo "$@" >> "$0.args"; echo '{}'"#);
        exec(&bin).list_json("", "pods").await.expect("list");
        assert_eq!(recorded(&bin), "--context c get pods -o json");
    }

    #[tokio::test]
    async fn not_found_stderr_maps_to_not_found() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(
            &dir,
            r#"echo 'Error from server (NotFound): configmaps "x" not found' >&2; exit 1"#,
        );
        let err = exec(&bin)
            .get_json("n", "configmap", "x")
            .await
            .expect_err("missing resource");
        assert!(err.is_not_found(), "{err}");
    }

    #[tokio::test]
    async fn stderr_surfaces_in_the_error() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(
            &dir,
            "echo 'Unable to connect to the server: dial tcp: timeout' >&2; exit 1",
        );
        let err = exec(&bin)
            .list_json("", "pods")
            .await
            .expect_err("unreachable cluster");
        let msg = err.to_string();
        assert!(!err.is_not_found(), "{msg}");
        assert!(msg.starts_with("kubectl get pods -o json: "), "{msg}");
        assert!(msg.contains("Unable to connect"), "{msg}");
    }

    #[tokio::test]
    async fn an_empty_stderr_names_the_exit_status() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, "exit 3");
        let err = exec(&bin)
            .list_json("", "pods")
            .await
            .expect_err("failed exit");
        assert!(err.to_string().contains("exit status 3"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_binary_is_actionable() {
        let _serial = serial().await;
        let err = exec(Path::new("/nonexistent/kubectl"))
            .list_json("", "pods")
            .await
            .expect_err("no such binary");
        assert_eq!(
            err.to_string(),
            r#"kubectl not found at "/nonexistent/kubectl" (install kubectl or put it on PATH)"#
        );
    }

    #[tokio::test]
    async fn output_over_the_cap_is_refused() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, "echo 12345678901234567890");
        let port = KubectlExec::new(
            "c",
            Options {
                binary: bin,
                max_output: 10,
            },
        );
        let err = port
            .list_json("", "pods")
            .await
            .expect_err("output over the cap");
        assert!(
            err.to_string().contains("exceeds the 10 byte limit"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn logs_stream_lines_in_order() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(
            &dir,
            r#"echo "$@" >> "$0.args"; echo line-1; echo line-2; echo line-3"#,
        );
        let rx = exec(&bin)
            .logs("default", "pod-a", false)
            .await
            .expect("start stream");
        assert_eq!(collect(rx).await, ["line-1", "line-2", "line-3"]);
        assert_eq!(
            recorded(&bin),
            "--context c -n default logs pod-a --tail 200"
        );
    }

    #[tokio::test]
    async fn logs_follow_adds_the_flag() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo "$@" >> "$0.args""#);
        let rx = exec(&bin)
            .logs("default", "pod-a", true)
            .await
            .expect("start stream");
        assert!(collect(rx).await.is_empty());
        assert!(
            recorded(&bin).ends_with("--tail 200 -f"),
            "{}",
            recorded(&bin)
        );
    }

    #[tokio::test]
    async fn a_failed_log_stream_ends_with_an_error_line() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, "echo x; echo boom >&2; exit 1");
        let rx = exec(&bin)
            .logs("default", "pod-a", false)
            .await
            .expect("start stream");
        assert_eq!(collect(rx).await, ["x", "error: boom"]);
    }

    #[tokio::test]
    async fn a_log_stream_that_cannot_start_reports_why() {
        let _serial = serial().await;
        let err = exec(Path::new("/nonexistent/kubectl"))
            .logs("default", "pod-a", false)
            .await
            .expect_err("no such binary");
        assert!(err.to_string().starts_with("kubectl not found at"), "{err}");
    }

    #[tokio::test]
    async fn dropping_the_receiver_stops_the_log_stream() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        // Ignoring SIGPIPE means a write to the closed pipe cannot end the
        // stub, so only the adapter killing it can.
        let bin = stub(
            &dir,
            r#"trap '' PIPE; echo $$ > "$0.pid"; while true; do echo tick; sleep 0.05; done"#,
        );
        let mut rx = exec(&bin)
            .logs("default", "pod-a", true)
            .await
            .expect("start stream");
        assert_eq!(rx.recv().await.as_deref(), Some("tick"));
        let pid = pid_of(&bin).await;
        drop(rx);
        assert_gone(&pid).await;
    }

    /// A script that writes a marked first line, over 64 KiB of filler, and a
    /// marked last line to stderr, then fails.
    const CHATTY_FAILURE: &str = r#"echo FIRST-LINE-MARKER >&2
i=0
while [ $i -lt 2000 ]; do
  echo "filler-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" >&2
  i=$((i+1))
done
echo LAST-LINE-MARKER >&2
exit 1"#;

    #[tokio::test]
    async fn a_chatty_stderr_keeps_only_its_tail() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, CHATTY_FAILURE);
        let err = exec(&bin)
            .list_json("", "pods")
            .await
            .expect_err("failed exit");
        let msg = err.to_string();
        assert!(msg.ends_with("LAST-LINE-MARKER"), "{}", tail_of(&msg));
        assert!(!msg.contains("FIRST-LINE-MARKER"));
        assert!(msg.len() <= STDERR_TAIL + 64, "{} bytes", msg.len());
    }

    #[tokio::test]
    async fn a_chatty_log_stderr_keeps_only_its_tail() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, CHATTY_FAILURE);
        let rx = exec(&bin)
            .logs("default", "pod-a", false)
            .await
            .expect("start stream");
        let lines = collect(rx).await;
        let [line] = lines.as_slice() else {
            panic!("expected one error line, got {} lines", lines.len());
        };
        assert!(line.starts_with("error: "), "{}", tail_of(line));
        assert!(line.ends_with("LAST-LINE-MARKER"), "{}", tail_of(line));
        assert!(!line.contains("FIRST-LINE-MARKER"));
        assert!(line.len() <= STDERR_TAIL + 64, "{} bytes", line.len());
    }

    fn tail_of(text: &str) -> &str {
        let mut start = text.len().saturating_sub(80);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        &text[start..]
    }

    fn env(name: &str, context: &str) -> Environment {
        Environment {
            name: name.to_owned(),
            kube_context: context.to_owned(),
            ..Environment::default()
        }
    }

    fn stub_options(binary: &Path) -> Options {
        Options {
            binary: binary.to_path_buf(),
            ..Options::default()
        }
    }

    #[tokio::test]
    async fn no_context_refuses_without_running_kubectl() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo "$@" >> "$0.args"; echo '{}'"#);
        let port = factory_with(stub_options(&bin))(&env("dev", ""), &Vars::new());
        let err = port
            .get_json("menu", "configmap", "menu-config")
            .await
            .expect_err("no context");
        assert_eq!(
            err.to_string(),
            r#"no Kubernetes context for environment "dev": set kube_context in config.yaml or pass --context"#
        );
        assert!(!sidecar(&bin, ".args").exists(), "kubectl was run");
    }

    #[tokio::test]
    async fn a_configured_context_is_passed_to_every_call() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo "$@" >> "$0.args"; echo '{}'"#);
        let port = factory_with(stub_options(&bin))(&env("dev", "dev-cluster"), &Vars::new());
        port.get_json("menu", "configmap", "menu-config")
            .await
            .expect("get");
        port.list_json("menu", "pods").await.expect("list");
        let rx = port.logs("menu", "pod-a", false).await.expect("logs");
        collect(rx).await;
        let calls = std::fs::read_to_string(sidecar(&bin, ".args")).expect("recorded");
        assert_eq!(calls.lines().count(), 3, "{calls}");
        for line in calls.lines() {
            assert!(line.starts_with("--context dev-cluster "), "{line}");
        }
    }

    #[tokio::test]
    async fn an_unavailable_port_fails_every_call() {
        let port = Unavailable::new("nope");
        for err in [
            port.get_json("", "pods", "p").await.expect_err("get"),
            port.list_json("", "pods").await.expect_err("list"),
            port.logs("", "p", false).await.expect_err("logs"),
        ] {
            assert_eq!(err.to_string(), "nope");
            assert!(!err.is_not_found());
        }
    }

    #[tokio::test]
    async fn dropping_a_call_kills_the_child() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo $$ > "$0.pid"; exec sleep 30"#);
        let port = exec(&bin);
        let pid = {
            let call = port.get_json("n", "pods", "p");
            tokio::pin!(call);
            tokio::select! {
                res = &mut call => panic!("the call finished early: {res:?}"),
                pid = pid_of(&bin) => pid,
            }
            // `call` is dropped here, mid-flight.
        };
        assert_gone(&pid).await;
    }
}
