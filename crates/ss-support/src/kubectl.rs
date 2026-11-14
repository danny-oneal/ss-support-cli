//! The Kubernetes port backed by the `kubectl` binary.
//!
//! Every call runs `kubectl --context <ctx> [-n <ns>] get …` or `… logs …`
//! and nothing else; the adapter never runs a command that changes cluster
//! state. A missing resource becomes [`SvcError::NotFound`]; every other
//! failure is reported with kubectl's own message. Each child is spawned with
//! `kill_on_drop`, so dropping a call or a log receiver never leaves kubectl
//! running, and each is tracked until it ends, so a process that is about to
//! exit can stop the ones still running with [`stop_children`].

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::anyhow;
use async_trait::async_trait;
use ss_core::config::Environment;
use ss_core::svc::SvcError;
use ss_core::svc::kubectl::{KubectlPort, LOG_BUFFER, LOG_LINE_MAX, LOG_TAIL};
use ss_core::vars::Vars;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::services::KubectlFactory;

/// The most output one `get` or `list` call may return: 32 MiB.
pub(crate) const MAX_OUTPUT: usize = 32 << 20;

/// How much of a child's stderr is kept for error messages: the last 64 KiB.
const STDERR_TAIL: usize = 64 << 10;

/// Every kubectl process started and not yet seen to end.
static RUNNING: Mutex<BTreeMap<u64, Child>> = Mutex::new(BTreeMap::new());

/// The key the next process is tracked under.
static NEXT_RUNNING: AtomicU64 = AtomicU64::new(0);

fn running() -> MutexGuard<'static, BTreeMap<u64, Child>> {
    // Nothing done under the lock can leave the map half-changed.
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A kubectl process that is tracked while it runs. Dropping it stops the
/// process.
struct Running(u64);

impl Running {
    fn track(child: Child) -> Self {
        let key = NEXT_RUNNING.fetch_add(1, Ordering::Relaxed);
        running().insert(key, child);
        Running(key)
    }

    /// Waits for the process to end by itself and returns how it ended. It
    /// is no longer tracked while this waits, which is after its output has
    /// ended.
    async fn wait(self) -> io::Result<ExitStatus> {
        let child = running().remove(&self.0);
        match child {
            Some(mut child) => child.wait().await,
            None => Err(io::Error::other("the process is no longer tracked")),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let child = running().remove(&self.0);
        // Dropped outside the lock; this is what kills a process that is
        // still running.
        drop(child);
    }
}

/// Stops every kubectl process started here that is still running. It is
/// for a path that is about to end the process: destructors do not run on
/// exit, and a task that would have stopped its kubectl may never run again.
pub(crate) fn stop_children() {
    for child in running().values_mut() {
        // A process that has already ended has nothing left to stop.
        let _ = child.start_kill();
    }
}

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
    /// Returns the tracked child and its stdout and stderr handles.
    ///
    /// # Errors
    ///
    /// Fails when kubectl cannot be started; a missing binary gets an
    /// actionable message.
    fn spawn(
        &self,
        namespace: &str,
        rest: &[&str],
    ) -> Result<(Running, ChildStdout, ChildStderr), SvcError> {
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
        Ok((Running::track(child), stdout, stderr))
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
        let (child, mut stdout, mut stderr) = self.spawn(namespace, rest)?;
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
        let reason = last_line(&errtext).map_or_else(|| exit_description(status), str::to_owned);
        if is_not_found(&errtext) {
            return Err(SvcError::not_found(format!("kubectl {shown}: {reason}")));
        }
        Err(SvcError::Other(anyhow!("kubectl {shown}: {reason}")))
    }
}

/// The last non-empty line of kubectl's stderr, trimmed. kubectl prints its
/// summary last, after any number of log lines, so this keeps an error
/// message to one line.
fn last_line(stderr: &str) -> Option<&str> {
    stderr.lines().rev().map(str::trim).find(|l| !l.is_empty())
}

/// Refuses a resource, resource name, or pod name that is empty or starts
/// with `-`, which kubectl would read as a flag and so could override the
/// pinned context.
fn check_name(value: &str) -> Result<(), SvcError> {
    if value.is_empty() || value.starts_with('-') {
        return Err(SvcError::Other(anyhow!(
            "refusing kubectl argument {value:?}: names cannot be empty or start with \"-\""
        )));
    }
    Ok(())
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
        check_name(resource)?;
        check_name(name)?;
        self.run_get(namespace, &["get", resource, name, "-o", "json"])
            .await
    }

    async fn list_json(&self, namespace: &str, resource: &str) -> Result<Vec<u8>, SvcError> {
        check_name(resource)?;
        self.run_get(namespace, &["get", resource, "-o", "json"])
            .await
    }

    async fn logs(
        &self,
        namespace: &str,
        pod: &str,
        follow: bool,
    ) -> Result<mpsc::Receiver<String>, SvcError> {
        check_name(pod)?;
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
/// with a failure and wrote to stderr, the last line sent is `error: {line}`,
/// where `line` is the last non-empty line of stderr.
async fn pump_logs(
    child: Running,
    stdout: ChildStdout,
    stderr: ChildStderr,
    tx: mpsc::Sender<String>,
) {
    // Drain stderr concurrently so a chatty kubectl never blocks on a full pipe.
    let errors = tokio::spawn(read_tail(stderr, STDERR_TAIL));
    let mut lines = Lines::new(BufReader::new(stdout), LOG_LINE_MAX);
    loop {
        let next = tokio::select! {
            () = tx.closed() => {
                errors.abort();
                return;
            }
            next = lines.next() => next,
        };
        let Ok(Some(line)) = next else {
            break;
        };
        if tx.send(line).await.is_err() {
            errors.abort();
            return;
        }
    }
    let failed = !child.wait().await.is_ok_and(|status| status.success());
    let errtext = errors.await.unwrap_or_default();
    let errtext = String::from_utf8_lossy(&errtext);
    if let Some(summary) = last_line(&errtext).filter(|_| failed) {
        // The receiver may already be gone; there is nobody left to tell.
        let _ = tx.send(format!("error: {summary}")).await;
    }
}

/// Splits what a reader produces into lines of at most `max` bytes, so that
/// output with no line break in it is never held in memory whole.
struct Lines<R> {
    reader: R,
    max: usize,
    /// The start of a character whose other bytes had not been read when a
    /// line was cut.
    carry: Vec<u8>,
    /// Whether the last line returned was cut before its line break.
    cut: bool,
}

impl<R: AsyncBufRead + Unpin> Lines<R> {
    fn new(reader: R, max: usize) -> Self {
        Lines {
            reader,
            max,
            carry: Vec::new(),
            cut: false,
        }
    }

    /// The next line without its line break, or `None` at the end of the
    /// input. A line longer than the limit is returned in pieces of at most
    /// that many bytes, none ending inside a UTF-8 character. Invalid UTF-8
    /// is replaced.
    ///
    /// # Errors
    ///
    /// A failure to read.
    async fn next(&mut self) -> io::Result<Option<String>> {
        let mut line = std::mem::take(&mut self.carry);
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                return Ok((!line.is_empty()).then(|| text_of(&line)));
            }
            let room = self.max.saturating_sub(line.len()).max(1);
            let window = &available[..available.len().min(room)];
            if let Some(end) = window.iter().position(|&byte| byte == b'\n') {
                line.extend_from_slice(&window[..end]);
                self.reader.consume(end + 1);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                // The line break of a line cut exactly at the limit ends
                // that line; it is not an empty line after it.
                if std::mem::take(&mut self.cut) && line.is_empty() {
                    continue;
                }
                return Ok(Some(text_of(&line)));
            }
            let taken = window.len();
            line.extend_from_slice(window);
            self.reader.consume(taken);
            if line.len() >= self.max {
                let whole = line.len() - incomplete_tail(&line);
                // A piece that would be empty is returned as it is, so the
                // split always moves on.
                if whole > 0 {
                    self.carry = line.split_off(whole);
                }
                self.cut = true;
                return Ok(Some(text_of(&line)));
            }
        }
    }
}

/// How many bytes at the end of `bytes` start a UTF-8 character whose other
/// bytes are not there.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for (index, byte) in bytes.iter().rev().take(3).enumerate() {
        let needed = match byte {
            // A byte from the middle of a character: look further back.
            0x80..=0xBF => continue,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        let have = index + 1;
        return if needed > have { have } else { 0 };
    }
    0
}

/// `raw` as text; invalid UTF-8 is replaced.
fn text_of(raw: &[u8]) -> String {
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

    /// A stub that writes klog noise, then kubectl's summary line, to stderr.
    const KLOG_THEN_SUMMARY: &str = r#"echo 'E1110 10:00:00.000000   12345 memcache.go:265] couldn'"'"'t get current server API group list: Get "https://10.0.0.1/api?timeout=32s": dial tcp 10.0.0.1:443: i/o timeout' >&2
echo 'E1110 10:00:32.000000   12345 memcache.go:265] couldn'"'"'t get current server API group list: Get "https://10.0.0.1/api?timeout=32s": dial tcp 10.0.0.1:443: i/o timeout' >&2
echo 'E1110 10:01:04.000000   12345 memcache.go:265] couldn'"'"'t get current server API group list: Get "https://10.0.0.1/api?timeout=32s": dial tcp 10.0.0.1:443: i/o timeout' >&2
echo 'Unable to connect to the server: dial tcp 10.0.0.1:443: i/o timeout' >&2
exit 1"#;

    #[tokio::test]
    async fn multi_line_stderr_reports_only_its_last_line() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, KLOG_THEN_SUMMARY);
        let err = exec(&bin)
            .list_json("", "pods")
            .await
            .expect_err("unreachable cluster");
        assert!(!err.is_not_found(), "{err}");
        assert_eq!(
            err.to_string(),
            "kubectl get pods -o json: Unable to connect to the server: dial tcp 10.0.0.1:443: i/o timeout"
        );
    }

    #[tokio::test]
    async fn not_found_on_an_earlier_stderr_line_still_maps_to_not_found() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(
            &dir,
            r#"echo 'Error from server (NotFound): configmaps "x" not found' >&2; echo 'trailing note' >&2; exit 1"#,
        );
        let err = exec(&bin)
            .get_json("n", "configmap", "x")
            .await
            .expect_err("missing resource");
        assert!(err.is_not_found(), "{err}");
        assert!(!err.to_string().contains('\n'), "{err}");
    }

    #[tokio::test]
    async fn a_failed_log_stream_reports_only_the_last_stderr_line() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, KLOG_THEN_SUMMARY);
        let rx = exec(&bin)
            .logs("default", "pod-a", false)
            .await
            .expect("start stream");
        assert_eq!(
            collect(rx).await,
            ["error: Unable to connect to the server: dial tcp 10.0.0.1:443: i/o timeout"]
        );
    }

    #[tokio::test]
    async fn names_that_are_empty_or_look_like_flags_never_reach_kubectl() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo "$@" >> "$0.args"; echo '{}'"#);
        let port = exec(&bin);
        let refusal = |value: &str| {
            format!(
                r#"refusing kubectl argument "{value}": names cannot be empty or start with "-""#
            )
        };

        let err = port
            .get_json("menu", "configmap", "--context=x")
            .await
            .expect_err("flag-like name");
        assert_eq!(err.to_string(), refusal("--context=x"));
        assert!(!err.is_not_found());
        let err = port
            .get_json("menu", "configmap", "")
            .await
            .expect_err("empty name");
        assert_eq!(err.to_string(), refusal(""));
        let err = port
            .get_json("menu", "-o", "x")
            .await
            .expect_err("flag-like resource");
        assert_eq!(err.to_string(), refusal("-o"));
        let err = port
            .list_json("menu", "")
            .await
            .expect_err("empty resource");
        assert_eq!(err.to_string(), refusal(""));
        let err = port
            .list_json("menu", "--all-namespaces")
            .await
            .expect_err("flag-like resource");
        assert_eq!(err.to_string(), refusal("--all-namespaces"));
        let err = port
            .logs("menu", "--context=x", false)
            .await
            .expect_err("flag-like pod");
        assert_eq!(err.to_string(), refusal("--context=x"));
        let err = port.logs("menu", "", true).await.expect_err("empty pod");
        assert_eq!(err.to_string(), refusal(""));

        assert!(!sidecar(&bin, ".args").exists(), "kubectl was run");
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

    // Nothing else is spawned here, so only the adapter itself can have
    // collected the process it killed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_stopped_log_stream_is_reaped_without_another_kubectl_call() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
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

        let entry = PathBuf::from(format!("/proc/{pid}"));
        for _ in 0..100 {
            if !entry.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("process {pid} was stopped but is still in the process table");
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
    async fn every_invocation_is_get_or_logs() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo "$@" >> "$0.args"; echo '{}'"#);
        let port = exec(&bin);
        port.get_json("menu", "configmap", "menu-config")
            .await
            .expect("get");
        port.get_json("", "configmap", "menu-config")
            .await
            .expect("get without a namespace");
        port.list_json("menu", "pods").await.expect("list");
        collect(port.logs("menu", "pod-a", false).await.expect("logs")).await;
        collect(port.logs("", "pod-a", true).await.expect("follow")).await;

        let calls = std::fs::read_to_string(sidecar(&bin, ".args")).expect("recorded");
        assert_eq!(calls.lines().count(), 5, "{calls}");
        for line in calls.lines() {
            let rest = line
                .strip_prefix("--context c ")
                .unwrap_or_else(|| panic!("no pinned context: {line}"));
            let rest = match rest.strip_prefix("-n ") {
                Some(after) => {
                    after
                        .split_once(' ')
                        .unwrap_or_else(|| panic!("namespace without a verb: {line}"))
                        .1
                }
                None => rest,
            };
            assert!(
                rest.starts_with("get ") || rest.starts_with("logs "),
                "not a get or logs call: {line}"
            );
        }
    }

    #[test]
    fn the_adapter_names_no_write_verb() {
        // The test module is the only gated item, so the production half is
        // everything before its attribute.
        let production = include_str!("kubectl.rs")
            .split(concat!("#[cfg(all(", "test, unix))]"))
            .next()
            .expect("split yields at least one part");
        assert!(
            production.contains("impl KubectlPort for KubectlExec"),
            "the production half of the adapter must hold the port implementation"
        );
        assert!(
            production.contains("fn run_get") && production.contains("\"logs\""),
            "the production half of the adapter must hold its get and logs calls"
        );
        assert!(
            !production.contains("mod tests"),
            "the production half must end before the test module"
        );
        // Quoted, so prose and identifiers (`exec`, `Options`) do not match;
        // assembled with `concat!` so this list cannot match itself.
        let literals = [
            concat!("\"", "rollout", "\""),
            concat!("\"", "scale", "\""),
            concat!("\"", "apply", "\""),
            concat!("\"", "patch", "\""),
            concat!("\"", "delete", "\""),
            concat!("\"", "edit", "\""),
            concat!("\"", "create", "\""),
            concat!("\"", "replace", "\""),
            concat!("\"", "annotate", "\""),
            concat!("\"", "label", "\""),
            concat!("\"", "exec", "\""),
            concat!("\"", "cp", "\""),
            concat!("\"", "port-forward", "\""),
            concat!("\"", "drain", "\""),
            concat!("\"", "cordon", "\""),
            concat!("\"", "taint", "\""),
        ];
        for literal in literals {
            assert!(
                !production.contains(literal),
                "the adapter names the verb {literal}"
            );
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

    /// Every line `input` splits into when no line may exceed `max` bytes.
    async fn split(input: &[u8], max: usize) -> Vec<String> {
        let mut lines = Lines::new(input, max);
        let mut out = Vec::new();
        while let Some(line) = lines.next().await.expect("reading from memory") {
            out.push(line);
        }
        out
    }

    #[tokio::test]
    async fn a_line_longer_than_the_limit_arrives_in_pieces() {
        assert_eq!(
            split(b"abcdefghij\nkl\n", 4).await,
            ["abcd", "efgh", "ij", "kl"]
        );
    }

    #[tokio::test]
    async fn a_line_of_exactly_the_limit_is_one_line() {
        assert_eq!(
            split(b"abcd\nefgh\r\n\nij", 4).await,
            ["abcd", "efgh", "", "ij"]
        );
    }

    #[tokio::test]
    async fn line_breaks_of_either_kind_are_removed() {
        assert_eq!(split(b"a\r\nb\n\nc", 16).await, ["a", "b", "", "c"]);
    }

    #[tokio::test]
    async fn a_piece_never_ends_inside_a_character() {
        assert_eq!(split("aé€😀b".as_bytes(), 4).await, ["aé", "€", "😀", "b"]);
    }

    #[tokio::test]
    async fn bytes_that_are_not_text_are_split_at_the_limit_all_the_same() {
        let pieces = split(&[0xFF; 10], 4).await;

        assert_eq!(
            pieces.iter().map(|p| p.chars().count()).collect::<Vec<_>>(),
            [4, 4, 2]
        );
        // A character that never completes does not hold the split back.
        assert_eq!(split(&[0xE2, 0x82, 0xE2, 0x82], 2).await.len(), 2);
    }

    #[tokio::test]
    async fn a_log_without_line_breaks_arrives_in_bounded_lines() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r"head -c 100000 /dev/zero | tr '\0' x");
        let rx = exec(&bin)
            .logs("default", "pod-a", false)
            .await
            .expect("start stream");

        let lines = collect(rx).await;

        assert!(lines.len() > 1, "{} lines", lines.len());
        assert!(
            lines.iter().all(|line| line.len() <= LOG_LINE_MAX),
            "{:?}",
            lines.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(lines.iter().map(String::len).sum::<usize>(), 100_000);
    }

    #[tokio::test]
    async fn stopping_the_children_ends_a_log_that_is_still_being_read() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
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

        stop_children();

        // The receiver is still held: only the stop can have ended it.
        assert_gone(&pid).await;
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(drained.is_ok(), "the stream ends once kubectl has gone");
    }

    #[tokio::test]
    async fn stopping_the_children_ends_a_call_that_is_still_waiting() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, r#"echo $$ > "$0.pid"; exec sleep 30"#);
        let port = exec(&bin);
        let call = port.get_json("n", "pods", "p");
        tokio::pin!(call);
        let pid = tokio::select! {
            res = &mut call => panic!("the call finished early: {res:?}"),
            pid = pid_of(&bin) => pid,
        };

        stop_children();

        let err = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("the call returns once kubectl has gone")
            .expect_err("a stopped kubectl is a failure");
        assert!(err.to_string().contains("terminated by a signal"), "{err}");
        assert_gone(&pid).await;
    }

    #[tokio::test]
    async fn a_child_that_has_ended_is_no_longer_tracked() {
        let _serial = serial().await;
        let dir = TempDir::new().expect("tempdir");
        let bin = stub(&dir, "echo '{}'");
        let port = exec(&bin);

        port.list_json("", "pods").await.expect("list");
        collect(port.logs("", "pod-a", false).await.expect("logs")).await;

        // The log's kubectl is collected by its own task, a moment later.
        for _ in 0..100 {
            if running().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{} children are still tracked", running().len());
    }
}
