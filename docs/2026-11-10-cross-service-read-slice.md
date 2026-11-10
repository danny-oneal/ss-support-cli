# Cross-Service Read Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a read-only Kubernetes port (kubectl exec adapter: get/list JSON, log streaming, pod and ConfigMap parsing) and the two remaining read operations that span services — `config.drift` (S3 object vs live ConfigMap) and `record.trace` (DynamoDB record → S3 config → previous version, optionally vs a live ConfigMap) — then prepare release `0.1.0-alpha.4`.

**Architecture:** `ss-core` gains `svc::kubectl` (the `KubectlPort` trait, `Pod`, `parse_pods`, `configmap_data`, `FakeKubectl`), a shared body comparison (`ops::compare`, extracted from `s3.version-diff`), and `ops::flow` (helpers plus the two operations); `Deps` carries an optional `Arc<dyn KubectlPort>`. `ss-support` gains `kubectl` (the `KubectlExec` adapter over `tokio::process`) and wires a port into every invocation through a factory on `Services` that refuses to run kubectl unless the environment names a Kubernetes context.

**Tech Stack:** Rust 2024, tokio (`process`, `io-util`, `sync`), async-trait, serde/serde_json, chrono. No new dependencies.

**Spec:** `docs/2026-11-07-rust-port-master-plan.md` — Phase 3 (kubectl port, exec adapter, stub-binary tests, `parse_pods`, `configmap_data`) and Phase 5 (`ops/flow/{trace,drift}`). CLAUDE.md overrides the spec's `rollout_restart`/`scale`: Kubernetes access is read-only by design. User decisions of 2026-11-10: the port includes log streaming and pod parsing now; the adapter is tested with a stub binary only (no cluster); every kubectl call requires a configured Kubernetes context; the plan ends with a release task.

**Reference behavior** (read-only, sibling checkout — read it, never cite it in the Rust tree): `/home/engineer/Code/starbucks/ss-support-cli-go/internal/{kubectl,ops/flowops}/`. Values in this plan were transcribed from there; where the plan and that code disagree, the plan wins (see Rulings).

## Global Constraints

- **No Go in the Rust tree.** Comments, doc comments, test names, test messages, manifests, README, CHANGELOG written by these tasks must not mention Go, porting, "ported from", phases, plans, or design narrative. Describe behavior only.
- **Read-only.** `KubectlPort` has only `get_json`, `list_json`, `logs`. The exec adapter only ever runs `kubectl get` and `kubectl logs`. Both operations are `Class::Read`. Task 7 pins this.
- Edition 2024; `rust-version = "1.88"` stays; `make msrv` (`cargo +1.88 check --workspace --all-targets --locked`) passes. No new crates.
- `make check` (rustfmt check, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`) passes at the end of every task.
- `missing_docs` is on: every public item gets a `///` doc; every new module gets a `//!` doc. Every public fallible item documents its failure cases in an `# Errors` section.
- `thiserror` in `ss-core` (`SvcError` is reused); `anyhow` in operations and `ss-support`. `#[async_trait]` only on `KubectlPort`.
- `ss-core` never reads `std::env` and never spawns processes; the adapter lives in `ss-support`.
- No `unwrap()` outside tests; `expect("…")` only for invariants. Anything that reaches output uses `BTreeMap`. Closed sets are enums. Narrowing numeric conversions use `try_from`.
- Operation ids, names, summaries, categories, param names/labels/defaults/help, view titles, summary keys, and verdict strings are exact copy from this plan (they are the `-o json` contract).
- **Exit codes.** A bad input value — unknown domain, malformed `--vars`, missing sort vars, an ambiguous or unknown ConfigMap data key — is `RunnerError::InvalidParam` (exit 2), as in the DynamoDB slice. Data and I/O failures are exit 1.
- Process hygiene: every spawned `kubectl` uses `kill_on_drop(true)`, so a cancelled step (the runner drops the step future on Ctrl-C) never leaves a child running.
- One commit per task, conventional-commit subject with scope, ending with the session's attribution trailer. Never push, tag, or merge (Task 9's push and PR are the controller's, after the final review).

## Review Focus

1. **No Kubernetes context configured** (the environment has no `kube_context` and no `--context` was passed) → any kubectl-backed run exits 1 with one line naming the fix, and kubectl is never executed. Commands that never touch Kubernetes are unaffected. Pinned by Task 3 (`no_context_refuses_without_running_kubectl`) and Task 8 (`an_unconfigured_kubectl_exits_1`).
2. **kubectl missing from PATH, or the cluster unreachable** → one actionable line (`kubectl not found …` / kubectl's own stderr), exit 1, never a raw I/O error or a panic. Pinned by Task 2 (`a_missing_binary_is_actionable`, `stderr_surfaces_in_the_error`).
3. **A ConfigMap with several data keys and no `:key`, an unknown data key, or non-JSON data** → the first two are usage errors listing the available keys (exit 2); non-JSON data names the key (exit 1). Pinned by Task 4 (`configmap_body_*`) and Task 5 (`several_data_keys_without_a_choice_is_a_usage_error`).
4. **A record pointing at the oldest S3 version, or a history with delete markers between versions** → the summary says the recorded version is the oldest; the previous version skips delete markers. Pinned by Task 4 (`previous_version_skips_delete_markers`) and Task 6 (`the_oldest_version_has_no_history_diff`).
5. **Ctrl-C during a kubectl call or a log stream** → the kubectl child is killed (no orphan). Pinned by Task 2 (`dropping_a_call_kills_the_child`, `dropping_the_receiver_stops_the_log_stream`).

## Rulings

Decisions made while writing this plan, each with its reason. Executors do not re-open these.

1. **The port is read-only and has three methods.** `rollout_restart` and `scale` are absent (CLAUDE.md). `logs` and `parse_pods` are included although no operation uses them yet — the user's decision; they serve the upcoming TUI screens.
2. **A Kubernetes context is required** (the user's decision). The factory builds a `KubectlExec` only when the resolved environment's `kube_context` (config, `SS_SUPPORT_*` precedence, or `--context`) is non-empty; otherwise it returns an `Unavailable` port whose every call fails with `no Kubernetes context for environment "{env}": set kube_context in config.yaml or pass --context`. Every kubectl call passes `--context <ctx>`; kubectl's current context is never used. Exit 1, like the unconfigured S3 and DynamoDB ports.
3. **No `Lazy` for kubectl.** Building `KubectlExec` does no I/O, so there is nothing to defer.
4. **Output is capped at 32 MiB** per `get`/`list` call (`MAX_OUTPUT`); more is an error naming the limit. — The same bound S3 bodies have; a runaway `list` must not exhaust memory.
5. **Error mapping.** Stderr containing `notfound` or `not found` (case-insensitive) → `SvcError::NotFound`. A spawn failure with `io::ErrorKind::NotFound` → `kubectl not found at "{binary}" (install kubectl or put it on PATH)`. Any other non-zero exit → `kubectl {args}: {stderr}`, or `kubectl {args}: exit status {code}` when stderr is empty. `{args}` excludes the `--context`/`-n` prefix.
6. **`Pod` stores `created: Option<DateTime<Utc>>`, not an age.** The reference computed age at parse time, which makes parsing time-dependent; callers compute age. `restarts` is the sum over containers, `ready` is `"{ready}/{total}"`.
7. **`logs`** runs `kubectl … logs <pod> --tail 200 [-f]` and returns a bounded `tokio::sync::mpsc::Receiver<String>` (capacity 64) fed by a spawned task. When the stream ends with a failure, the task sends one final `error: {stderr}` line. When the receiver is dropped, the task ends and the child is killed.
8. **Empty namespace omits `-n`** (kubectl then uses the context's default namespace). `record.trace` takes `--namespace`, else the domain's `namespace`, else empty.
9. **Operation ids and copy match the reference:** `config.drift` and `record.trace`, category `Cross-service`.
10. **`config.drift` gains bucket/key/version suggestions** (the S3 suggestion functions); the reference had none.
11. **`--vars` is strict.** The reference skipped malformed `k=v` parts silently; here a malformed part is a usage error naming it. The first `=` splits; both sides are trimmed.
12. **The JSON-or-line-diff body comparison is shared.** It moves from `s3.version-diff` into `ops::compare`; `s3.version-diff`'s output is unchanged (its tests are the guard). `record.trace` uses it for the history diff.
13. **Release (Task 9)** bumps to `0.1.0-alpha.4` on this branch as the last commit. After the final review the controller pushes the branch and opens the PR; the annotated tag and GitHub pre-release follow only after the user merges.

---

## File Structure

`ss-core`:
- `src/svc/mod.rs` — `pub mod kubectl;`.
- `src/svc/kubectl/mod.rs` — `KubectlPort`, `Pod`, `parse_pods`, `configmap_data`, constants; re-exports.
- `src/svc/kubectl/fake.rs` — `FakeKubectl` (test-support).
- `src/ops/operation.rs` — `Deps::kubectl`.
- `src/ops/compare.rs` — `Comparison`, `compare_bodies`.
- `src/ops/s3/version_diff.rs` — uses `ops::compare`.
- `src/ops/flow/mod.rs` — `kube_port`, ConfigMap/vars/URI/version helpers.
- `src/ops/flow/fixture.rs` — `#[cfg(test)]` three-fake scenario.
- `src/ops/flow/{drift,trace}.rs` — the operations.
- `src/ops/mod.rs` — `pub mod compare; pub mod flow;`, registrations.

`ss-support`:
- `src/kubectl.rs` — `KubectlExec`, `Options`, `Unavailable`, `factory`, `factory_with`; stub-binary tests.
- `src/services.rs`, `src/run.rs`, `src/lib.rs` — factory wiring.
- `tests/flow_guards.rs`, `tests/flow.rs` — guard and e2e tests through `run_with`.

Root: `README.md`, `CHANGELOG.md`; Task 9: `Cargo.toml`, `crates/ss-support/Cargo.toml`, `Cargo.lock`.

---

### Task 1: Kubernetes port, parsers, and `FakeKubectl`

**Files:**
- Modify: `crates/ss-core/src/svc/mod.rs`
- Create: `crates/ss-core/src/svc/kubectl/mod.rs`, `crates/ss-core/src/svc/kubectl/fake.rs`

**Interfaces:**
- Produces (`ss_core::svc::kubectl`):
  ```rust
  pub const LOG_TAIL: u32 = 200;
  pub const LOG_BUFFER: usize = 64;

  #[async_trait]
  pub trait KubectlPort: Send + Sync {
      /// `kubectl get <resource> <name> -o json` in `namespace` ("" = the context's default).
      async fn get_json(&self, namespace: &str, resource: &str, name: &str) -> Result<Vec<u8>, SvcError>;
      /// `kubectl get <resource> -o json` in `namespace`.
      async fn list_json(&self, namespace: &str, resource: &str) -> Result<Vec<u8>, SvcError>;
      /// The last `LOG_TAIL` lines of a pod's log, then new lines while `follow`; the channel closes when the stream ends.
      async fn logs(&self, namespace: &str, pod: &str, follow: bool) -> Result<tokio::sync::mpsc::Receiver<String>, SvcError>;
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Pod { pub name: String, pub phase: String, pub ready: String, pub restarts: u32, pub created: Option<DateTime<Utc>> }
  pub fn parse_pods(raw: &[u8]) -> Result<Vec<Pod>, SvcError>;                       // malformed JSON → Other("parse pods: …")
  pub fn configmap_data(raw: &[u8]) -> Result<BTreeMap<String, String>, SvcError>;   // no `data` → empty map; malformed → Other("parse configmap: …")
  ```
- Produces (`FakeKubectl`, test-support; `#[derive(Clone, Default)]`, data in `Arc<Mutex<…>>` so clones share it; the guard is dropped before any `.await`):
  ```rust
  pub fn new() -> Self;
  pub fn insert(&self, namespace: &str, resource: &str, name: &str, json: impl Into<Vec<u8>>);
  pub fn insert_list(&self, namespace: &str, resource: &str, json: impl Into<Vec<u8>>);
  pub fn set_log_lines(&self, lines: Vec<String>);   // `logs` replays them, then closes
  ```
  A missing entry is `SvcError::not_found("{namespace}/{resource}/{name}")` (or `"{namespace}/{resource}"` for lists).

- [ ] **Step 1: Write the failing tests.**

```rust
// svc/kubectl/mod.rs
#[test] fn parse_pods_sums_restarts_counts_ready_and_keeps_the_creation_time() {
    // items: api-7f9 (creationTimestamp 2026-08-01T00:00:00Z, Running, containers ready+2 restarts, not-ready+0)
    //        worker-1 (no creationTimestamp, Pending, no containerStatuses)
    // → [Pod{name:"api-7f9", phase:"Running", ready:"1/2", restarts:2, created:Some(2026-08-01T00:00:00Z)},
    //    Pod{name:"worker-1", phase:"Pending", ready:"0/0", restarts:0, created:None}]
}
#[test] fn parse_pods_rejects_malformed_json() { /* b"{" → Err, !is_not_found, message starts "parse pods: " */ }
#[test] fn configmap_data_returns_the_data_map() { /* {"data":{"menu.json":"{\"a\":1}","other":"x"}} → BTreeMap with both, "menu.json" == r#"{"a":1}"# */ }
#[test] fn configmap_without_data_is_empty() { /* {"metadata":{}} → empty map */ }

// svc/kubectl/fake.rs
#[tokio::test] async fn fake_serves_inserted_resources_and_reports_missing_ones() {
    // insert("menu","configmap","menu-config", b"{}") → get_json returns it; get_json for "x" → is_not_found with "menu/configmap/x";
    // insert_list("default","pods", b"{\"items\":[]}") → list_json returns it; list for "svc" → is_not_found
}
#[tokio::test] async fn fake_replays_log_lines_then_closes() { /* set_log_lines(["a","b"]) → recv a, b, then None */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core svc::kubectl` → compile error.

- [ ] **Step 3: Implement** per the interface. `restarts` and container counts deserialize as `u32`; `created` as `Option<DateTime<Utc>>`. Declare `#[cfg(any(test, feature = "test-support"))] mod fake;` with `pub use fake::FakeKubectl;` under the same cfg. Add `pub mod kubectl;` to `svc/mod.rs` and extend its `//!` doc.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core svc::kubectl` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(svc): read-only Kubernetes port with pod and ConfigMap parsing`

---

### Task 2: `KubectlExec` adapter

**Files:**
- Create: `crates/ss-support/src/kubectl.rs`
- Modify: `crates/ss-support/src/lib.rs` (`mod kubectl;`)

**Interfaces:**
- Consumes: `ss_core::svc::{SvcError, kubectl::{KubectlPort, LOG_TAIL, LOG_BUFFER}}`.
- Produces:
  ```rust
  pub(crate) const MAX_OUTPUT: usize = 32 << 20;
  #[derive(Debug, Clone)]
  pub(crate) struct Options { pub binary: PathBuf, pub max_output: usize }   // Default: "kubectl", MAX_OUTPUT
  #[derive(Debug, Clone)]
  pub(crate) struct KubectlExec { context: String, opts: Options }
  impl KubectlExec { pub(crate) fn new(context: impl Into<String>, opts: Options) -> Self }
  #[async_trait] impl KubectlPort for KubectlExec
  fn args(&self, namespace: &str, rest: &[&str]) -> Vec<String>;   // ["--context", ctx, ("-n", ns)?, rest…]
  fn is_not_found(stderr: &str) -> bool;
  ```

- [ ] **Step 1: Write the failing tests** (`#[cfg(all(test, unix))]`; a `stub(dir, script) -> PathBuf` helper writes `#!/bin/sh\n{script}` with mode `0o755` into a `tempfile::TempDir`; the stub records its arguments with `echo "$@" >> "$0.args"`):

```rust
#[test] fn classify_not_found() {
    assert!(is_not_found(r#"Error from server (NotFound): configmaps "x" not found"#));
    assert!(is_not_found("error: pods \"p\" NOT FOUND"));
    assert!(!is_not_found("Unable to connect to the server: dial tcp: i/o timeout"));
}
#[tokio::test] async fn passes_context_namespace_and_args() {
    // stub prints {"ok":true}; KubectlExec::new("dev-cluster", stub opts).get_json("menu","configmap","menu-config")
    // → stdout bytes; recorded args == "--context dev-cluster -n menu get configmap menu-config -o json"
}
#[tokio::test] async fn an_empty_namespace_omits_the_flag() { /* list_json("", "pods") → args "--context c get pods -o json" */ }
#[tokio::test] async fn not_found_stderr_maps_to_not_found() { /* stderr NotFound, exit 1 → Err(is_not_found) */ }
#[tokio::test] async fn stderr_surfaces_in_the_error() {
    // stderr "Unable to connect to the server: dial tcp: timeout", exit 1 → message starts "kubectl get pods -o json: " and contains "Unable to connect"
}
#[tokio::test] async fn an_empty_stderr_names_the_exit_status() { /* exit 3, no stderr → message contains "exit status 3" */ }
#[tokio::test] async fn a_missing_binary_is_actionable() {
    // binary "/nonexistent/kubectl" → message == r#"kubectl not found at "/nonexistent/kubectl" (install kubectl or put it on PATH)"#
}
#[tokio::test] async fn output_over_the_cap_is_refused() { /* max_output 10, stub prints 20 bytes → message contains "exceeds the 10 byte limit" */ }
#[tokio::test] async fn logs_stream_lines_in_order() { /* stub echoes line-1..line-3 → recv all three then None; args "--context c -n default logs pod-a --tail 200" */ }
#[tokio::test] async fn logs_follow_adds_the_flag() { /* follow=true → args end with "--tail 200 -f" */ }
#[tokio::test] async fn a_failed_log_stream_ends_with_an_error_line() { /* stub echoes "x", then stderr "boom", exit 1 → lines ["x", "error: boom"] */ }
#[tokio::test] async fn dropping_the_receiver_stops_the_log_stream() {
    // stub writes $$ to "$0.pid" then loops `while true; do echo tick; sleep 0.05; done`; read one line, drop the receiver;
    // within 2 s `kill -0 <pid>` fails (the child is gone)
}
#[tokio::test] async fn dropping_a_call_kills_the_child() {
    // stub writes $$ to "$0.pid" then `sleep 30`; tokio::time::timeout(200ms, get_json(..)) → Err (elapsed);
    // within 2 s `kill -0 <pid>` fails
}
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-support kubectl` → compile error.

- [ ] **Step 3: Implement.** `get_json`/`list_json`: `tokio::process::Command` with `kill_on_drop(true)`, stdin null, stdout and stderr piped; read stdout incrementally and fail with `Other("kubectl {args}: output exceeds the {max} byte limit")` as soon as it passes `max_output`; read stderr fully; map failures per Ruling 5. `logs`: spawn the same way, then spawn a task that reads stdout with `BufReader::lines()` and `send`s each line on an `mpsc::channel(LOG_BUFFER)`; a failed `send` (receiver dropped) ends the task, dropping (and so killing) the child; at the end `wait()`s and, on failure with non-empty stderr, sends `error: {stderr}`. A spawn failure in `logs` is returned directly. The task owns the child.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-support kubectl` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(cli): kubectl adapter for read-only get and log streaming`

---

### Task 3: `Deps::kubectl`, the context-checking factory, and wiring

**Files:**
- Modify: `crates/ss-core/src/ops/operation.rs`, `crates/ss-support/src/kubectl.rs`, `crates/ss-support/src/services.rs`, `crates/ss-support/src/run.rs`, `crates/ss-support/src/lib.rs`

**Interfaces:**
- Consumes: `KubectlPort`, `KubectlExec`, `Options`, the S3/DynamoDB factory pattern in `services.rs`.
- Produces:
  ```rust
  // Deps gains: pub kubectl: Option<Arc<dyn KubectlPort>>   (Default: None)
  // ss-support kubectl.rs
  pub(crate) struct Unavailable { message: String }                   // every call → SvcError::Other(anyhow!(message))
  impl Unavailable { pub(crate) fn new(message: impl Into<String>) -> Self }
  #[async_trait] impl KubectlPort for Unavailable
  pub(crate) fn factory() -> KubectlFactory;                           // factory_with(Options::default())
  pub(crate) fn factory_with(opts: Options) -> KubectlFactory;         // Ruling 2
  // services.rs (re-exported from lib.rs)
  pub type KubectlFactory = Arc<dyn Fn(&Environment, &Vars) -> Arc<dyn KubectlPort> + Send + Sync>;
  // Services gains `pub kubectl: KubectlFactory`; from_process → kubectl::factory();
  // isolated → Unavailable("Kubernetes is not configured"); `pub fn with_kubectl(self, port: Arc<dyn KubectlPort>) -> Self`.
  ```

- [ ] **Step 1: Write the failing tests.**

```rust
// ss-support kubectl.rs (unix)
#[tokio::test] async fn no_context_refuses_without_running_kubectl() {
    // factory_with(stub opts) for Environment { name: "dev", kube_context: "" } → get_json fails with exactly
    // r#"no Kubernetes context for environment "dev": set kube_context in config.yaml or pass --context"#; the stub's .args file does not exist
}
#[tokio::test] async fn a_configured_context_is_passed_to_every_call() { /* kube_context "dev-cluster" → args start "--context dev-cluster" */ }
// services.rs
#[tokio::test] async fn isolated_kubectl_is_unconfigured() { /* list_json errors with "Kubernetes is not configured" */ }
#[tokio::test] async fn with_kubectl_serves_the_given_port() { /* FakeKubectl with one configmap → get_json returns it */ }
// operation.rs
#[test] fn default_deps_have_no_kubectl() { assert!(Deps::default().kubectl.is_none()); }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-support kubectl services` and `cargo test -p ss-core default_deps_have_no_kubectl` → compile errors.

- [ ] **Step 3: Implement.** `App::build` takes the kubectl factory beside the S3 and DynamoDB ones and sets `deps.kubectl = Some((factory)(&env, vars))`. The resolved `env.kube_context` already reflects `--context` (see `cli.rs`).

- [ ] **Step 4: Run the tests.** `cargo test --workspace` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(cli): a Kubernetes port on every invocation, pinned to the configured context`

---

### Task 4: Shared body comparison and cross-service helpers

**Files:**
- Create: `crates/ss-core/src/ops/compare.rs`, `crates/ss-core/src/ops/flow/mod.rs`, `crates/ss-core/src/ops/flow/fixture.rs`
- Modify: `crates/ss-core/src/ops/mod.rs`, `crates/ss-core/src/ops/s3/version_diff.rs`

**Interfaces:**
- Consumes: `diff::{json, text, Opts, Delta}`, `jsonutil::parse`, `svc::s3::{Version, is_current}`, `svc::kubectl::{KubectlPort, configmap_data}`, `RunnerError::InvalidParam`, `FakeS3`, `FakeDdb`, `FakeKubectl`.
- Produces:
  ```rust
  // ops::compare
  pub(crate) enum Comparison { Json(Delta), Lines(String) }
  pub(crate) fn compare_bodies(a: &[u8], b: &[u8], opts: &Opts) -> Comparison;   // both parse → Json; otherwise a line diff of the lossy UTF-8 text
  // ops::flow
  pub(crate) fn kube_port(deps: &Deps) -> anyhow::Result<Arc<dyn KubectlPort>>;    // "Kubernetes is not configured"
  pub(crate) fn split_configmap_spec(spec: &str) -> anyhow::Result<(&str, Option<&str>)>;   // "name" | "name:key"; splits on the first ':'; empty name → error
  pub(crate) fn parse_vars(spec: &str) -> anyhow::Result<BTreeMap<String, String>>;         // Ruling 11; blank → empty map
  pub(crate) fn split_s3_uri<'a>(value: &'a str, configured_bucket: &'a str) -> (&'a str, &'a str);   // (bucket, key)
  pub(crate) fn previous_version_id<'a>(versions: &'a [Version], current: &str) -> Option<&'a str>;
  pub(crate) async fn configmap_body(k8s: &dyn KubectlPort, namespace: &str, spec: &str) -> anyhow::Result<(String, String)>;   // (data key, body)
  pub(crate) fn json_delta(s3_body: &[u8], data_key: &str, body: &str, opts: &Opts) -> anyhow::Result<Delta>;
  ```
- `fixture` (test-only): `deps() -> Deps` with env `dev`, diff ignore `**.updatedAt`, `array_match {"$.items[*]": "id"}`; a `FakeS3` holding `ss-config-dev/menu/12345.json` versions `ver-1` (`{"items":[{"id":"latte","price":5}],"updatedAt":"a"}`, 2026-08-01T10:00:00Z) and `ver-2` (`{"items":[{"id":"latte","price":6},{"id":"mocha","price":7}],"updatedAt":"b"}`, one hour later); a `FakeDdb` table `ss-menu-dev` (hash `storeNumber` S, sort `localeChannel` S, no GSIs) holding `{storeNumber:"12345", localeChannel:"en-US#mobile", version:"V0", key:"menu/12345.json", versionId:"ver-2"}`; a `FakeKubectl` with `menu/configmap/menu-config` = `{"data":{"menu.json":"{\"items\":[{\"id\":\"latte\",\"price\":5},{\"id\":\"mocha\",\"price\":7}],\"updatedAt\":\"b\"}"}}`; trace domain `menu` = `{table:"ss-menu-{env}", bucket:"ss-config-{env}", hash_attr:"storeNumber", sort_attr:"localeChannel", sort_format:"{locale}#{channel}", labels:["storeNumber","version"], namespace:"menu"}`. Also export the three fakes so tests can alter them.

`configmap_body` errors: several data keys and no `:key` → `RunnerError::InvalidParam { param: "configmap", value: spec, source: "configmap {name} has {n} data keys ({k1, k2}) — specify one as name:key" }`; a named key that is absent → `InvalidParam` with `configmap {name} has no data key "{key}" (have: {k1, k2})`; no data at all → `configmap {name} has no data`; a missing ConfigMap propagates the port's `NotFound`. `json_delta` errors: `S3 config is not JSON: …`, `configmap data "{key}" is not JSON: …`.

`previous_version_id`: versions arrive newest first; find `current` (when `is_current(current)`, the entry with `is_latest`), then return the next older entry that is not a delete marker; `None` when there is none or `current` is absent.

- [ ] **Step 1: Write the failing tests.**

```rust
// ops/compare.rs
#[test] fn json_bodies_compare_structurally() { /* {"a":1} vs {"a":2} → Json with one change at "a" */ }
#[test] fn a_non_json_side_falls_back_to_lines() { /* "x\ny" vs {"a":1} → Lines containing "-x" */ }
// ops/flow/mod.rs
#[test] fn split_configmap_spec_cases() {
    // "menu-config" → ("menu-config", None); "menu-config:menu.json" → (.., Some("menu.json")); "a:b:c" → ("a", Some("b:c")); ":x" and "" → Err
}
#[test] fn parse_vars_is_strict() {
    // "" → {}; " locale = en-US , channel=mobile " → {channel: mobile, locale: en-US}; "a=b=c" → {a: "b=c"};
    // "locale=en-US,oops" → Err containing r#"vars part "oops" is not name=value"#
}
#[test] fn split_s3_uri_cases() {
    // ("s3://other-bucket/some/key.json","cfg") → ("other-bucket","some/key.json"); ("plain/key.json","cfg") → ("cfg","plain/key.json"); ("s3://only","cfg") → ("only","")
}
#[test] fn previous_version_skips_delete_markers() {
    // newest first: v3(latest), del(marker), v2, v1 → previous("v3") == Some("v2"); previous("null") == Some("v2");
    // previous("v1") == None; previous("zzz") == None
}
#[tokio::test] async fn configmap_body_picks_the_only_key() { /* fixture menu-config, spec "menu-config" → ("menu.json", body) */ }
#[tokio::test] async fn configmap_body_with_several_keys_needs_a_choice() {
    // a second key "other" → Err; downcast finds RunnerError::InvalidParam { param == "configmap" }; message lists "menu.json, other"
}
#[tokio::test] async fn configmap_body_names_an_unknown_key() { /* "menu-config:nope" → InvalidParam containing r#"no data key "nope" (have: menu.json)"# */ }
#[tokio::test] async fn configmap_body_reports_a_missing_configmap_as_not_found() { /* "absent" → find_cause SvcError is_not_found */ }
#[test] fn json_delta_names_the_side_that_is_not_json() { /* S3 "x" → "S3 config is not JSON"; data "x" → r#"configmap data "menu.json" is not JSON"# */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core ops::compare ops::flow` → compile errors.

- [ ] **Step 3: Implement** per the interfaces. Move the comparison out of `s3/version_diff.rs` into `ops::compare` and make `version_diff` call `compare_bodies`; its tests must pass unchanged. Pub(crate) helpers not yet used by an operation may carry a narrow `#[allow(dead_code)] // used by the cross-service operations` that Tasks 5–6 remove.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS (including every `version_diff` test). `make check` → PASS.

- [ ] **Step 5: Commit.** `refactor(ops): share body comparison; add cross-service helpers`

---

### Task 5: `config.drift`

**Files:**
- Create: `crates/ss-core/src/ops/flow/drift.rs`
- Modify: `crates/ss-core/src/ops/flow/mod.rs`, `crates/ss-core/src/ops/mod.rs` (register; extend `builtin_registers_every_operation`)

**Interfaces:**
- Consumes: Task 4 helpers and fixture; `ops::s3::{port, suggest_buckets, suggest_keys, suggest_versions}`.
- Produces: `pub struct ConfigDrift;` (`#[derive(Clone, Copy, Debug, Default)]`).

Meta: id `config.drift`, name `Config drift check`, summary `Diff an S3 config object against a live Kubernetes ConfigMap`, category `Cross-service`, class `Read`.
Params, in order:

| name | label | type | required | default | help | suggest |
|---|---|---|---|---|---|---|
| bucket | Bucket | String | yes | | | buckets |
| key | Key | String | yes | | | keys |
| version | S3 version | String | no | `null` | `version to compare; "null" = current` | versions |
| configmap | ConfigMap | String | yes | | `name, or name:data-key when the map holds several files` | |
| namespace | Namespace | String | no | `default` | | |
| ignore | Ignore paths | Json | no | | `extra ignore globs, e.g. "**.updatedAt"` | |

Steps: `fetch S3 config`, `diff live ConfigMap`. Summary: `S3 object` = `{bucket}/{key} @ {version_label}`, `ConfigMap` = `{namespace}/{name}`, `Result` = `in sync — the cluster is running this config` or `DRIFT ({stats})`. On drift only, `View::Diff { left: "S3 {version_label}", right: "ConfigMap {name}", delta }`. Diff options: `deps.diff.ignore ++ input.globs("ignore")`, `array_match` from deps.

- [ ] **Step 1: Write the failing tests** (fixture; namespace `menu`):

```rust
#[tokio::test] async fn a_stale_configmap_is_reported_as_drift() {
    // configmap "menu-config:menu.json" → Result starts "DRIFT ("; views[1] Diff with exactly one change at "items[id=latte].price";
    // S3 object "ss-config-dev/menu/12345.json @ ver-2 (2026-08-01 11:00)"; ConfigMap "menu/menu-config"
}
#[tokio::test] async fn a_matching_configmap_is_in_sync() { /* set the ConfigMap to ver-2's body → Result "in sync — the cluster is running this config"; one view */ }
#[tokio::test] async fn an_older_version_can_be_compared() { /* version ver-1 → DRIFT with one Added change (mocha) */ }
#[tokio::test] async fn several_data_keys_without_a_choice_is_a_usage_error() { /* second data key, configmap "menu-config" → RunFailure kind Usage */ }
#[tokio::test] async fn a_missing_configmap_fails_as_not_found() { /* "absent" → not found; "{:#}" starts "diff live ConfigMap: " */ }
#[tokio::test] async fn a_non_json_s3_body_is_reported() { /* object body "plain text" → error contains "S3 config is not JSON" */ }
#[tokio::test] async fn without_kubernetes_the_run_fails_clearly() { /* deps.kubectl = None → "Kubernetes is not configured" */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core drift` → compile error.

- [ ] **Step 3: Implement.** Take `kube_port` and the S3 port first. Register in `builtin()`; the builtin test's `(category, id)` order becomes `["config.drift", "demo.hello", "demo.json-diff", "ddb.gsi-consistency", "ddb.item-compare", "ddb.item-history", "s3.object-inspect", "s3.prefix-audit", "s3.version-diff"]` (category `Cross-service` < `Demo` < `DynamoDB` < `S3`). Remove the `dead_code` allows on helpers this operation now uses.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): config.drift compares an S3 config with a live ConfigMap`

---

### Task 6: `record.trace`

**Files:**
- Create: `crates/ss-core/src/ops/flow/trace.rs`
- Modify: `crates/ss-core/src/ops/flow/mod.rs`, `crates/ss-core/src/ops/mod.rs`

**Interfaces:**
- Consumes: Task 4 helpers and fixture; `config::TraceDomain::{with_defaults, resolve_table, resolve_bucket, build_sort}`; `ops::ddb::{port, coerce}`; `ops::s3::port`; `ops::compare::compare_bodies`.
- Produces: `pub struct RecordTrace;` (`#[derive(Clone, Copy, Debug, Default)]`).

Meta: id `record.trace`, name `Trace record to config`, summary `Follow a domain record to its S3 config and diff against the previous version`, category `Cross-service`, class `Read`.
Params, in order:

| name | label | type | required | default | help | suggest |
|---|---|---|---|---|---|---|
| domain | Domain | Enum | yes | | `record shape from config.yaml trace.domains (menu, …)` | domains |
| id | Record ID | String | yes | | `partition key value (e.g. store number for menu)` | |
| vars | Sort key vars | String | no | | `k=v pairs for the domain's sort format, e.g. locale=en-US,channel=mobile` | |
| version | Record version | String | no | `V0` | `record version label; V0 = latest` | |
| configmap | ConfigMap | String | no | | `optionally diff the live ConfigMap against the S3 config (name[:data-key])` | |
| namespace | Namespace | String | no | | `kubernetes namespace for --configmap; defaults to the domain's` | |
| ignore | Ignore paths | Json | no | | `extra ignore globs, e.g. "**.updatedAt"` | |

The domain suggestion lists each configured domain (sorted) with label `"{name} — {resolved table}"`, plus `" · sort {sort_format}"` when the domain has one (after `with_defaults`).

Run, in order:
1. Usage checks (all `InvalidParam`): unknown domain → `unknown trace domain "{d}" (configure trace.domains in config.yaml)` (param `domain`); no table for the env → `domain "{d}" has no table for env "{env}"` (param `domain`); `parse_vars` errors and `build_sort` `MissingVars` (with `vars[version_attr] = version` inserted first) → param `vars`.
2. Step `fetch record`: `describe_table`; refuse a binary hash key (`binary key attributes are not supported: {attr}`); key = `{hash_attr: coerce(id, hash type)}` plus `{sort_attr: sort}` when both are non-empty; `get_item`.
3. The record's `s3_key_attr` must be a non-empty string, else `record has no "{attr}" attribute — wrong domain mapping?`; `split_s3_uri` against the resolved bucket; an empty bucket → `no bucket: set trace.domains.{d}.bucket or store s3:// URIs in "{attr}"`. The `s3_version_attr` value (string, else empty → current) is the version to fetch.
4. Step `fetch recorded S3 version`; step `resolve previous version` (`list_versions`, `previous_version_id(…, &recorded.version_id)`, then `get_version` of it).
5. Summary, in order: `Domain`; `Record` = `{table} {hash_attr}={id}`; `Sort key` (when non-empty); each label present on the record (strings raw, other values compact JSON); `S3 object` = `{bucket}/{key}`; `Recorded version` = version label; then `Change vs previous` = the stats, or `none — recorded version is the oldest in history` when there is no previous version. When either body is not JSON, the history comparison is a `View::Doc { title: "Line diff: {prev} → {recorded}", body, lang: "diff" }` and no `Change vs previous` field is added; otherwise a `View::Diff { left: prev label, right: recorded label, delta }`.
6. When `--configmap` is set: `kube_port`; namespace = `--namespace`, else the domain's, else empty; step `diff live ConfigMap` (`configmap_body` + `json_delta` against the recorded body); summary field `ConfigMap {name}` = `in sync with the recorded S3 config` or `DRIFT vs recorded S3 config ({stats})`; on drift a `View::Diff { left: "S3 {recorded label}", right: "ConfigMap {name}", delta }`.

Views: summary first, then the history view, then the ConfigMap view.

- [ ] **Step 1: Write the failing tests** (fixture):

```rust
#[tokio::test] async fn a_trace_spans_three_services() {
    // domain menu, id 12345, vars "locale=en-US,channel=mobile", configmap "menu-config:menu.json"
    // Record contains "ss-menu-dev"; S3 object "ss-config-dev/menu/12345.json"; Sort key "en-US#mobile"; storeNumber "12345"; version "V0"
    // two Diff views: history (ver-1 → ver-2) stats added 1, changed 1; drift: exactly one change at "items[id=latte].price";
    // "ConfigMap menu-config" starts "DRIFT vs recorded S3 config ("
}
#[tokio::test] async fn without_a_configmap_kubernetes_is_not_touched() { /* deps.kubectl = None, no --configmap → Ok with exactly one Diff view */ }
#[tokio::test] async fn missing_sort_vars_are_a_usage_error_naming_them() { /* no vars → kind Usage; message contains "locale" and "channel" */ }
#[tokio::test] async fn an_unknown_domain_is_a_usage_error() { /* domain nope → Usage, contains "unknown trace domain" */ }
#[tokio::test] async fn malformed_vars_are_a_usage_error() { /* vars "locale=en-US,oops" → Usage */ }
#[tokio::test] async fn the_oldest_version_has_no_history_diff() {
    // record versionId ver-1 → no Diff/Doc history view; Change vs previous "none — recorded version is the oldest in history"
}
#[tokio::test] async fn delete_markers_are_skipped_when_finding_the_previous_version() {
    // history ver-1, delete marker, ver-2 (record → ver-2) → history diff is ver-1 → ver-2
}
#[tokio::test] async fn an_s3_uri_on_the_record_overrides_the_bucket() { /* key "s3://other-bucket/menu/12345.json" with that object present → S3 object "other-bucket/menu/12345.json" */ }
#[tokio::test] async fn a_record_without_the_key_attribute_is_reported() { /* remove "key" → error r#"record has no "key" attribute — wrong domain mapping?"# */ }
#[tokio::test] async fn a_numeric_partition_key_is_coerced() { /* table hash type N, item storeNumber 12345 (number), id "12345" → found */ }
#[tokio::test] async fn non_json_bodies_get_a_line_diff() { /* both versions plain text → View::Doc titled "Line diff: … → …", no "Change vs previous" */ }
#[tokio::test] async fn the_configmap_namespace_defaults_to_the_domain() { /* no --namespace → reads menu/configmap/menu-config; --namespace other → not found */ }
#[test] fn domain_suggestions_show_the_table_and_sort_format() { /* → [Choice { value: "menu", label: Some("menu — ss-menu-dev · sort {locale}#{channel}") }] */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core trace` → compile error.

- [ ] **Step 3: Implement** per the run order above. Register in `builtin()`; the builtin test's order adds `record.trace` after `config.drift`. Remove the remaining `dead_code` allows in `ops/flow/mod.rs`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): record.trace follows a record to its S3 config and live ConfigMap`

---

### Task 7: Read-only guard tests

**Files:**
- Modify: `crates/ss-core/src/svc/kubectl/mod.rs` (test module), `crates/ss-support/src/kubectl.rs` (test module)
- Create: `crates/ss-support/tests/flow_guards.rs`

**Interfaces:** tests only; no product code.

- [ ] **Step 1: Write the guard tests.**

```rust
// ss-core svc/kubectl/mod.rs
#[test] fn the_port_has_no_write_method() {
    // `struct ReadOnly;` implementing KubectlPort with exactly get_json, list_json, logs compiles;
    // the production half (before the first literal `#[cfg(test)]`) is non-empty, contains "trait KubectlPort",
    // and declares no method named for a write: none of "fn rollout", "fn scale", "fn apply", "fn patch", "fn delete",
    // "fn edit", "fn replace", "fn annotate", "fn label", "fn drain", "fn cordon", "fn taint" appears as a prefix
    // (needles assembled with concat! so the test cannot match itself; prose in doc comments cannot trip it)
}
// ss-support kubectl.rs (unix)
#[tokio::test] async fn every_invocation_is_get_or_logs() {
    // one stub records each invocation's args on its own line; call get_json, list_json, logs(false), logs(true);
    // every recorded line, after "--context <c>" and an optional "-n <ns>", starts with "get " or "logs "
}
#[test] fn the_adapter_names_no_write_verb() {
    // production half of kubectl.rs is non-empty, contains "impl KubectlPort for KubectlExec", and contains none of the quoted
    // string literals "rollout", "scale", "apply", "patch", "delete", "edit", "create", "replace", "annotate", "label",
    // "exec", "cp", "port-forward", "drain", "cordon", "taint" (assembled with concat!)
}
// ss-support tests/flow_guards.rs
#[test] fn every_cross_service_operation_is_a_read() {
    // builtin().all() filtered to category "Cross-service" → ids exactly ["config.drift", "record.trace"], each Class::Read
}
#[tokio::test] async fn cross_service_operations_run_where_writes_are_refused() {
    // isolated services with FakeS3 + FakeDdb + FakeKubectl (fixture data rebuilt in the test) and a trace domain in config.yaml;
    // run both operations with valid arguments and empty stdin → Exit::Ok, empty stderr, non-empty stdout, and no audit log
    // exists at ss_core::audit::default_path(&vars) (reads are not audited; a change attempt would have written one)
}
```

- [ ] **Step 2: Run them.** `cargo test -p ss-core the_port_has_no_write_method`, `cargo test -p ss-support kubectl`, `cargo test -p ss-support --test flow_guards` → PASS. Prove each can fail: temporarily add a `rollout` method to the trait, a `"scale"` literal to the adapter, and `Class::Mutate` to one operation; see the matching test fail; revert. Record the evidence in the report.

- [ ] **Step 3: Run the suite.** `make check` → PASS.

- [ ] **Step 4: Commit.** `test(flow): guards that keep Kubernetes access read-only`

---

### Task 8: End-to-end tests, README, CHANGELOG

**Files:**
- Create: `crates/ss-support/tests/flow.rs`
- Modify: `README.md`, `CHANGELOG.md`, `docs/config.example.yaml` (only the `kube_context` comment, see Step 3)

**Interfaces:**
- Consumes: `Services::{isolated, with_s3, with_ddb, with_kubectl}`, the fakes, the `ss_with` helper shape from `tests/s3.rs`. A trace domain reaches the run through a `config.yaml` written into the isolated home.

- [ ] **Step 1: Write the e2e tests:**

```rust
#[tokio::test] async fn ops_list_shows_the_cross_service_operations() { /* contains config.drift and record.trace */ }
#[tokio::test] async fn config_drift_through_the_cli_as_json() {
    // run config.drift --bucket ss-config-dev --key menu/12345.json --configmap menu-config:menu.json --namespace menu -o json
    // → Exit::Ok; first view's Result starts "DRIFT ("  (read render::json for the exact shape first)
}
#[tokio::test] async fn record_trace_through_the_cli_as_text() {
    // config.yaml with the menu domain; run record.trace --domain menu --id 12345 --vars locale=en-US,channel=mobile
    // → Exit::Ok; stdout contains "Recorded version" and "ver-2"; stderr empty
}
#[tokio::test] async fn a_missing_configmap_exits_1_with_one_line() {
    // --configmap absent → Exit::Failed; stderr one line starting "ss-support run config.drift: diff live ConfigMap: " containing "not found"
}
#[tokio::test] async fn an_ambiguous_configmap_exits_2() { /* a second data key, --configmap menu-config → Exit::Usage, one stderr line listing both keys */ }
#[tokio::test] async fn an_unknown_domain_exits_2() { /* --domain nope → Exit::Usage */ }
#[tokio::test] async fn an_unconfigured_kubectl_exits_1() { /* isolated (no with_kubectl): config.drift → Exit::Failed, stderr contains "Kubernetes is not configured" */ }
#[tokio::test] async fn a_missing_required_parameter_exits_2() { /* config.drift --bucket b → Exit::Usage */ }
```

- [ ] **Step 2: Run them.** `cargo test -p ss-support --test flow` → PASS (a failure here is a wiring bug to fix, never a reason to weaken a test).

- [ ] **Step 3: Docs.** CHANGELOG `[Unreleased]` → `### Added`:
  - Read-only cross-service operations `config.drift` (an S3 config against a live Kubernetes ConfigMap) and `record.trace` (a DynamoDB record followed to its S3 config, its previous version, and optionally a live ConfigMap).
  - Kubernetes access through kubectl, pinned to the environment's `kube_context` (or `--context`); without one, Kubernetes-backed operations refuse to run. Only `kubectl get` and `kubectl logs` are ever executed.

  README: add a `## Cross-service` section after `## DynamoDB`: both operations with one example each (`--configmap name[:data-key]`, `--namespace`, `--vars`, `--version`); the `trace.domains` config (point to `docs/config.example.yaml`, list the defaulted attribute names); the context requirement and the exact refusal message; that kubectl must be on `PATH` and is only ever asked to `get` or `logs`; exit codes (bad domain, `--vars`, or ConfigMap key → 2; missing record, object, or ConfigMap → 1). Update the operations list and any flag/env tables so they match behavior. In `docs/config.example.yaml`, change the `kube_context` comment to say every kubectl call is pinned to it (it is no longer only checked before writes). No Go, porting, or plan references.

- [ ] **Step 4: Verify.** `make check` → PASS; `make msrv` → PASS; `cargo build --release` → succeeds.

- [ ] **Step 5: Commit.** `feat(flow): end-to-end tests and documentation for the cross-service operations`

---

### Task 9: Prepare release `0.1.0-alpha.4`

**Files:**
- Modify: `Cargo.toml` (`version = "0.1.0-alpha.4"`), `crates/ss-support/Cargo.toml` (`ss-core` pin `=0.1.0-alpha.4`), `Cargo.lock`, `CHANGELOG.md`

- [ ] **Step 1: Bump.** Edit both manifests; run `cargo update --workspace` (moves only the workspace members). `git diff --stat` shows exactly these four files.

- [ ] **Step 2: CHANGELOG.** Rename `## [Unreleased]` to `## [0.1.0-alpha.4] - YYYY-MM-DD` (today, `date -u +%F`), keep its entries, and add a fresh empty `## [Unreleased]` above it.

- [ ] **Step 3: Verify.** `make check` → PASS; `make msrv` → PASS; `cargo run -p ss-support -- --version` prints `ss-support 0.1.0-alpha.4`.

- [ ] **Step 4: Commit.** `chore(release): 0.1.0-alpha.4`

- [ ] **Step 5 (controller, after the final review): publish for merge.** Push the branch and open the PR (title `feat(flow): read-only cross-service operations (0.1.0-alpha.4)`). After the user merges: create an annotated tag `0.1.0-alpha.4` (message `0.1.0-alpha.4`) on the merge commit, after confirming `Cargo.toml` there reads `0.1.0-alpha.4`; push the tag; publish a GitHub pre-release titled `0.1.0-alpha.4` whose notes are the CHANGELOG section.

---

## Task index

| # | Title | Main files | Depends on |
|---|---|---|---|
| 1 | Kubernetes port, parsers, `FakeKubectl` | `ss-core/src/svc/kubectl/**` | — |
| 2 | `KubectlExec` adapter | `ss-support/src/kubectl.rs` | 1 |
| 3 | `Deps::kubectl`, context-checking factory | `ops/operation.rs`, `ss-support/src/{kubectl,services,run}.rs` | 1, 2 |
| 4 | Shared comparison + cross-service helpers | `ops/compare.rs`, `ops/flow/{mod,fixture}.rs`, `ops/s3/version_diff.rs` | 1, 3 |
| 5 | `config.drift` | `ops/flow/drift.rs` | 4 |
| 6 | `record.trace` | `ops/flow/trace.rs` | 4 |
| 7 | Read-only guard tests | `svc/kubectl/mod.rs`, `ss-support/src/kubectl.rs`, `tests/flow_guards.rs` | 2, 3, 5, 6 |
| 8 | e2e, README, CHANGELOG | `tests/flow.rs`, `README.md`, `CHANGELOG.md` | 3–7 |
| 9 | Release `0.1.0-alpha.4` | manifests, `Cargo.lock`, `CHANGELOG.md` | 8 |
