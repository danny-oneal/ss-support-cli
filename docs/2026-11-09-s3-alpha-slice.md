# S3 Alpha Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `0.1.0-alpha.1`: a read-only S3 slice (port, fake, decorators, AWS adapter, three operations) so the `ss` binary runs against a real non-production AWS account or MinIO.

**Architecture:** `ss-core` gains `svc::s3` (the `S3Port` trait, its types, `FakeS3`, the `Lazy` and `Cached` decorators, and a contract suite) and `ops::s3` (three `Class::Read` operations plus the bucket → key → version suggestion chain); `Deps` carries an optional `Arc<dyn S3Port>`. The `ss` crate gains `aws::{awsx, s3}` (SDK config, STS account guardrail, the `AwsS3` adapter) and wires a lazily built, cached port into every invocation through a factory on `Services`, so commands that never touch S3 never load AWS config.

**Tech Stack:** Rust 2024, tokio, async-trait, chrono, serde/serde_json, aws-config 1.8 / aws-sdk-s3 1.122 / aws-sdk-sts 1.97 (resolved MSRV-compatible by resolver 3), MinIO via docker for the env-gated contract run.

**Spec:** `docs/2026-11-07-rust-port-master-plan.md` — Phase 3 (S3 port, fake, Lazy/Cached, contract suite), Phase 4 (`awsx`, `aws/s3.rs`), Phase 5 (S3 operations), and the risk table (R5 async traits, JSON numbers). The alpha's scope (read-only S3 slice, version `0.1.0-alpha.1`) is the user's decision of 2026-11-08 and overrides those phases wherever they include writes (`put`, `s3.restore-version`).

**Reference behavior** (read-only, sibling checkout — read it, never cite it in the Rust tree): `/home/engineer/Code/starbucks/ss-support-cli-go/internal/{s3svc,s3svc/porttest,ops/s3ops,awsx}/` and `cmd/root.go` (`s3Builder`). Values in this plan were transcribed from there; where the plan and that code disagree, the plan wins (see Rulings).

## Global Constraints

- **No Go in the Rust tree.** Comments, doc comments, test names, test messages, manifests, README, CHANGELOG, and `docs/` text written by these tasks must not mention Go, porting, "ported from", phases, plans, or design narrative. Describe behavior only.
- Edition 2024; `rust-version = "1.88"` stays. `make msrv` (`cargo +1.88 check --workspace --all-targets --locked`) must pass; if an SDK release would raise the floor, pin an older one rather than raising MSRV.
- `make check` (rustfmt check, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`) passes at the end of every task.
- `missing_docs` is on: every public item gets a `///` doc; every new module gets a `//!` doc.
- `thiserror` in `ss-core`; `anyhow` in operations and the `ss` crate. `#[async_trait]` only on `S3Port` (dyn-dispatched); native `async fn` everywhere else.
- `ss-core` never reads the process environment (`std::env`); values come from the `Vars` snapshot. `ss-core` gets no AWS dependency.
- No `unwrap()` outside tests; `expect("…")` only for invariants.
- Operation ids, names, summaries, param names/labels/defaults/help, view titles, column names, and summary keys are exact copy from this plan (they are the `-o json` contract).
- Timestamps render in UTC with these chrono formats: version label `%Y-%m-%d %H:%M`; tables and suggestion labels `%Y-%m-%d %H:%M:%S`; inspect's Modified field `%Y-%m-%d %H:%M:%S %Z` (prints `UTC`).
- One commit per task, conventional-commit subject, ending with the session's attribution trailer. Never push, tag, or merge.

## Review Focus

1. **Expired SSO session or missing credentials on an S3 run** → one actionable line naming `aws sso login --profile <p>`, exit 1. Pinned by Task 8 (`friendly_*` tests).
2. **Bucket with versioning disabled** → the `InvalidArgument … version id` answer must stay a real error (not "not found") when no version id was sent. Pinned by Task 9 (`classify_*` table).
3. **Commands that never touch S3** (`ops list`, `presets`, demo ops) → must not resolve AWS config or need credentials. Pinned by Task 3 (`lazy_does_not_build_until_used`) and Task 10 (`demo_and_ops_list_run_without_s3`).
4. **Object larger than the body cap** → an error naming the limit, never an unbounded read. Pinned by Task 9 (`max_body_is_enforced`, MinIO run).
5. **Corrupt or unwritable cache** → a corrupt entry falls through to the network and is overwritten; a failed cache write never fails the fetch. Pinned by Task 3 (`corrupt_entry_falls_through_and_is_rewritten`, `failed_cache_write_does_not_fail_the_fetch`).

## Rulings

Decisions made while writing this plan, each with its reason. Executors do not re-open these.

1. **The port is read-only.** `S3Port` has no `put`; no restore operation; the contract suite has no Put section. — The alpha is read-only by the user's decision.
2. **`FakeS3` rolls up common prefixes (delimiter `/`) and models delete markers.** — The reference fake did neither, so prefix-audit's recursion and the inspect "delete marker" row were only exercised against real S3. With both modelled, the contract suite asserts the same facts against the fake and MinIO with no divergence flags. Cost: the key-suggestion test expects one `tenants/` prefix choice instead of two flat keys.
3. **`s3.prefix-audit` honours `--max` before descending into sub-prefixes.** — The reference passes a remaining budget of `0` to the sub-scan once the limit is hit mid-page, and `0` means "unlimited", so a sub-tree is drained completely. Pinned by `max_stops_before_descending_into_prefixes`.
4. **The cache entry is a JSON header line plus the raw body**, not JSON with a base64 or number-array body. — The format is private to `Cached`; raw bytes keep big configs compact. A corrupt header is a miss.
5. **Timestamps are `Option<DateTime<Utc>>`.** A missing time renders as `-` in tables, labels, and fields, and the age as `unknown`; it sorts as oldest. — Real S3 always sends one; a sentinel date in output would be worse than `-`.
6. **`s3.object-inspect` trims a body preview cut mid-character** back to the last whole character instead of reporting it as binary. — A 4096-byte cut through a multi-byte character is not binary.
7. **`s3.version-diff` always uses the line-diff views when either body is not JSON**, even when the line diff is empty. — The reference fell through to an empty structural diff in that case; the line-diff mode is the truthful label.
8. **The contract suite and `FakeS3` ship behind an `ss-core` feature `test-support`** (`#[cfg(any(test, feature = "test-support"))]`), enabled only by the `ss` crate's dev-dependency. — The `ss` crate's MinIO test and e2e tests need them; release builds do not.
9. **The S3 port reaches the app through `Services::s3: S3Factory`.** — `Services::isolated` gets a factory whose port fails with `S3 is not configured`, and `Services::with_s3` injects a `FakeS3`, so the e2e suite drives `ss run s3.*` through the real command path without AWS.
10. **SDK crates use their default features** (rustls-based HTTPS, tokio). — No OpenSSL/native-tls in the static binary; trimming `sigv4a` is not worth the risk for an alpha.
11. **`SS_S3_ENDPOINT` skips the STS identity check**, as S3-compatible endpoints do not serve STS. — Matches the reference and the MinIO workflow.
12. **`Lazy` caches the build error as its message string** (`{e:#}`) and returns it as `SvcError::Other` on every call. — `anyhow::Error` is not `Clone`; the operator needs the message, and the build runs at most once.
13. **`--max` reached exactly still reports `(stopped at --max N)`** even if nothing was left. — Knowing would cost another listing call; output stays comparable with the reference.

---

## File Structure

`ss-core`:
- `Cargo.toml` — `[features] test-support = []`.
- `src/lib.rs` — `pub mod svc;` and one doc line.
- `src/svc/mod.rs` — `SvcError`; `pub mod s3;`.
- `src/svc/s3/mod.rs` — types, `S3Port`, `is_current`, `CURRENT`; re-exports.
- `src/svc/s3/fake.rs` — `FakeS3` (test-support).
- `src/svc/s3/contract.rs` — `Fixture`, `Paging`, `run` (test-support).
- `src/svc/s3/lazy.rs` — `Lazy`.
- `src/svc/s3/cached.rs` — `Cached`.
- `src/ops/operation.rs` — `Deps::s3`.
- `src/ops/s3/mod.rs` — `port()`, suggestion chain, formatting helpers.
- `src/ops/s3/fixture.rs` — `#[cfg(test)]` shared fake deps for op tests.
- `src/ops/s3/{version_diff,object_inspect,prefix_audit}.rs` — the operations.
- `src/ops/mod.rs` — `pub mod s3;`, registrations in `builtin()`.

`ss`:
- `Cargo.toml` — AWS deps; dev-dep `ss-core` with `test-support`.
- `src/aws/mod.rs` — `s3_factory()`.
- `src/aws/awsx.rs` — config load, identity, SSO message, account guardrail.
- `src/aws/s3.rs` — `AwsS3` + MinIO contract test.
- `src/services.rs`, `src/run.rs`, `src/lib.rs` — factory wiring.
- `tests/s3.rs` — e2e through `run_with`.

Root: `Cargo.toml` (versions, workspace deps), `Makefile` (`contract`), `.github/workflows/ci.yml` (`contract` job), `CHANGELOG.md`, `README.md`, `docs/README.md`.

---

### Task 1: S3 port, types, and `FakeS3`

**Files:**
- Modify: `crates/ss-core/Cargo.toml`, `crates/ss-core/src/lib.rs`
- Create: `crates/ss-core/src/svc/mod.rs`, `crates/ss-core/src/svc/s3/mod.rs`, `crates/ss-core/src/svc/s3/fake.rs`

**Interfaces:**
- Produces (`ss_core::svc`):
  ```rust
  #[derive(Debug, thiserror::Error)]
  pub enum SvcError {
      #[error("not found: {what}")]
      NotFound { what: String, #[source] source: Option<Box<dyn std::error::Error + Send + Sync>> },
      #[error(transparent)]
      Other(#[from] anyhow::Error),
  }
  impl SvcError {
      pub fn not_found(what: impl Into<String>) -> Self;                       // source: None
      pub fn not_found_from(what: impl Into<String>, source: impl std::error::Error + Send + Sync + 'static) -> Self;
      pub fn is_not_found(&self) -> bool;
  }
  ```
- Produces (`ss_core::svc::s3`), all types `#[derive(Debug, Clone, Default, PartialEq, Eq)]`, all fields `pub`:
  ```rust
  pub const CURRENT: &str = "null";
  pub fn is_current(version_id: &str) -> bool;            // "" or "null"
  pub struct Bucket { name: String, created: Option<DateTime<Utc>> }
  pub struct ObjectMeta { bucket: String, key: String, size: u64, modified: Option<DateTime<Utc>>, etag: String }
  pub struct Page { objects: Vec<ObjectMeta>, prefixes: Vec<String>, next: Option<String> }
  pub struct Version { id: String, modified: Option<DateTime<Utc>>, size: u64, etag: String, is_latest: bool, is_delete: bool }
  pub struct Object { meta: ObjectMeta, version_id: String, body: Vec<u8> }
  impl Object { pub fn version_label(&self) -> String }

  #[async_trait]
  pub trait S3Port: Send + Sync {
      async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError>;
      async fn list(&self, bucket: &str, prefix: &str, token: Option<&str>) -> Result<Page, SvcError>;
      async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError>;
      async fn get_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<Object, SvcError>;
      async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError>;
  }
  ```
- Produces (`ss_core::svc::s3::FakeS3`, test-support), `#[derive(Clone, Default)]`, data in `Arc<Mutex<…>>` so clones share it:
  ```rust
  pub fn new() -> Self;
  pub fn paged(&self, page_size: usize) -> FakeS3;            // same data, `list` returns at most page_size entries
  pub fn create_bucket(&self, bucket: &str);
  pub fn insert(&self, bucket: &str, key: &str, version_id: &str, body: impl Into<Vec<u8>>, modified: DateTime<Utc>); // etag "{version_id}-etag"; creates the bucket
  pub fn put(&self, bucket: &str, key: &str, body: impl Into<Vec<u8>>) -> String;    // id "put-{n}", modified Utc::now()
  pub fn delete(&self, bucket: &str, key: &str) -> String;                            // delete marker id "del-{n}", modified Utc::now()
  ```
  `put` and `delete` share one counter starting at 1.

- [ ] **Step 1: Scaffold.** Add `[features] test-support = []` to `crates/ss-core/Cargo.toml`. Add `pub mod svc;` to `lib.rs` and extend its `//!` list with "the [`svc`] service ports and their in-memory fakes". In `svc/s3/mod.rs` declare `#[cfg(any(test, feature = "test-support"))] mod fake;` and re-export `FakeS3` under the same cfg.

- [ ] **Step 2: Write the failing tests** (in `svc/s3/mod.rs` and `svc/s3/fake.rs` test modules):

```rust
// svc/s3/mod.rs
fn at(s: &str) -> DateTime<Utc> { s.parse().expect("rfc3339") }

#[test]
fn version_label_names_current_short_ids_and_dates() {
    let obj = |id: &str, modified| Object { version_id: id.into(), meta: ObjectMeta { modified, ..Default::default() }, ..Default::default() };
    assert_eq!(obj("", None).version_label(), "current");
    assert_eq!(obj("null", Some(at("2026-07-01T10:00:00Z"))).version_label(), "current");
    assert_eq!(obj("abc", None).version_label(), "abc");
    assert_eq!(obj("abcdefghijk", None).version_label(), "abcdefgh");
    assert_eq!(obj("abcdefghijk", Some(at("2026-07-01T10:00:59Z"))).version_label(), "abcdefgh (2026-07-01 10:00)");
}

#[test]
fn not_found_is_recognised_and_keeps_its_source() {
    let e = SvcError::not_found_from("b/k", std::io::Error::other("NoSuchKey"));
    assert!(e.is_not_found());
    assert_eq!(e.to_string(), "not found: b/k");
    assert_eq!(std::error::Error::source(&e).map(ToString::to_string).as_deref(), Some("NoSuchKey"));
    assert!(!SvcError::from(anyhow::anyhow!("boom")).is_not_found());
}

// svc/s3/fake.rs
#[tokio::test]
async fn list_rolls_up_nested_keys_into_prefixes() {
    let s3 = FakeS3::new();
    for k in ["a.json", "dir/b.json", "dir/sub/c.json", "dir/sub/d.json"] { s3.put("b", k, "{}"); }
    let root = s3.list("b", "", None).await.expect("list");
    assert_eq!(keys(&root), ["a.json"]);
    assert_eq!(root.prefixes, ["dir/"]);
    let dir = s3.list("b", "dir/", None).await.expect("list");
    assert_eq!(keys(&dir), ["dir/b.json"]);
    assert_eq!(dir.prefixes, ["dir/sub/"]);
}

#[tokio::test]
async fn paged_listing_shares_data_and_counts_prefixes_toward_the_page() {
    let s3 = FakeS3::new();
    for k in ["p/1", "p/2", "p/sub/3"] { s3.put("b", k, "x"); }
    let paged = s3.paged(2);
    let first = paged.list("b", "p/", None).await.expect("page 1");
    assert_eq!((keys(&first), first.prefixes.clone(), first.next.as_deref()), (vec!["p/1", "p/2"], vec![], Some("p/2")));
    let second = paged.list("b", "p/", first.next.as_deref()).await.expect("page 2");
    assert_eq!((keys(&second), second.prefixes.clone(), second.next), (vec![], vec!["p/sub/".to_owned()], None));
}

#[tokio::test]
async fn a_deleted_key_leaves_history_but_not_the_listing() {
    let s3 = FakeS3::new();
    let v1 = s3.put("b", "k", "one");
    let marker = s3.delete("b", "k");
    assert!(keys(&s3.list("b", "", None).await.expect("list")).is_empty());
    assert!(s3.get_version("b", "k", "null").await.expect_err("current is gone").is_not_found());
    assert!(s3.head("b", "k").await.expect_err("head").is_not_found());
    let err = s3.get_version("b", "k", &marker).await.expect_err("marker has no body");
    assert!(!err.is_not_found(), "{err}");
    assert_eq!(s3.get_version("b", "k", &v1).await.expect("old version").body, b"one");
    let history = s3.list_versions("b", "k").await.expect("history");
    assert_eq!(history.iter().map(|v| (v.id.as_str(), v.is_latest, v.is_delete)).collect::<Vec<_>>(),
               [(marker.as_str(), true, true), (v1.as_str(), false, false)]);
}
```
(`keys(&Page) -> Vec<&str>` is a test helper.)

- [ ] **Step 3: Run to verify they fail.** `cargo test -p ss-core svc::` → compile errors (types missing).

- [ ] **Step 4: Implement** the types, `S3Port`, `SvcError`, and `FakeS3`. `version_label`: `"current"` when `is_current`, else the first 8 characters of the id, plus ` ({modified:%Y-%m-%d %H:%M})` when `modified` is set. `FakeS3` semantics:
  - Storage: bucket → key → versions oldest first, each an `Object` or a delete marker (id, modified).
  - `list_buckets`: every bucket, sorted by name, `created: None`.
  - `list`: missing bucket → `NotFound("{bucket}")`. Consider keys starting with `prefix` whose newest entry is not a delete marker. With `rest = key[prefix.len()..]`: if `rest` contains `/`, the entry is the prefix `prefix + rest[..=first '/']` (deduplicated); else it is the object (newest version's meta with `bucket`, `key`, `size = body.len()`). Order all entries by name. A `token` must equal the name of an entry in this listing, else `Other("continuation token {token:?} was not issued by this listing")`; resume after it. With `page_size > 0` and more entries left than `page_size`, return the first `page_size` and `next = Some(last returned name)`.
  - `list_versions`: missing bucket/key or empty history → `NotFound("{bucket}/{key}")`. Newest first; `is_latest` on the newest entry (marker or not); `size = body.len()` (0 for markers); markers have empty etag.
  - `get_version`: missing → `NotFound("{bucket}/{key}")`. Current alias: newest entry; a marker there → `NotFound("{bucket}/{key}")`. Explicit id: object → clone; marker → `Other("{bucket}/{key}@{id} is a delete marker")`; absent → `NotFound("{bucket}/{key}@{id}")`.
  - `head`: `get_version(current)`'s meta with `bucket`, `key`, `size = body.len()`.

- [ ] **Step 5: Run the tests.** `cargo test -p ss-core svc::` → PASS. Then `make check` → PASS.

- [ ] **Step 6: Commit.** `feat(s3): read-only S3 port, types, and in-memory fake`

---

### Task 2: S3 contract suite

**Files:**
- Create: `crates/ss-core/src/svc/s3/contract.rs`
- Modify: `crates/ss-core/src/svc/s3/mod.rs` (`#[cfg(any(test, feature = "test-support"))] pub mod contract;`)

**Interfaces:**
- Consumes: Task 1 types, `S3Port`, `FakeS3`.
- Produces:
  ```rust
  pub struct Fixture<'a> {
      pub port: &'a dyn S3Port,
      pub bucket: String,
      pub key: String,                 // nested, e.g. "cfg/tenants/acme.json"
      pub bodies: Vec<String>,         // oldest first, at least two
      pub version_ids: Vec<String>,    // ids of `bodies`, same order
      pub delete_marker_ids: Vec<String>, // markers in `key`'s history
      pub paging: Paging<'a>,
  }
  pub struct Paging<'a> { pub port: &'a dyn S3Port, pub prefix: String, pub want_keys: Vec<String>, pub page_size: usize }
  pub async fn run(fx: &Fixture<'_>);   // panics with a message naming the failed check
  ```
  The fixture's history must end with a put (the newest entry is an object, not a marker).
- Produces for later test modules (module level in `contract.rs`, `#[cfg(test)] pub(crate)`):
  ```rust
  pub(crate) const BODIES: [&str; 3];
  pub(crate) fn seeded_fake() -> (FakeS3, Vec<String>, String);   // (fake, put ids, marker id)
  pub(crate) fn fixture<'a>(port: &'a dyn S3Port, paged: &'a dyn S3Port, ids: Vec<String>, marker: String) -> Fixture<'a>;
  ```
  Task 3 runs the suite over its decorators with these.

- [ ] **Step 1: Write the failing test.** The helpers live at module level (see Interfaces); the test sits in `contract.rs`'s test module. Seed the fake exactly like the MinIO test in Task 9 will:

```rust
const BODIES: [&str; 3] = [
    r#"{"tier":"gold","rules":[{"id":"r1","limit":10}]}"#,
    r#"{"tier":"platinum","rules":[{"id":"r1","limit":10}]}"#,
    r#"{"tier":"platinum","rules":[{"id":"r0","limit":5},{"id":"r1","limit":10}]}"#,
];

/// A fake seeded with put, put, delete, put on one key plus five keys under `cfg/many/`.
pub(crate) fn seeded_fake() -> (FakeS3, Vec<String>, String) { … }   // (fake, put ids, marker id)

#[tokio::test]
async fn fake_satisfies_the_contract() {
    let (fake, ids, marker) = seeded_fake();
    let paged = fake.paged(2);
    run(&Fixture {
        port: &fake, bucket: "ss-config".into(), key: "cfg/tenants/acme.json".into(),
        bodies: BODIES.map(String::from).to_vec(), version_ids: ids, delete_marker_ids: vec![marker],
        paging: Paging { port: &paged, prefix: "cfg/many/".into(),
            want_keys: ["a", "b", "c", "d", "e"].map(|n| format!("cfg/many/{n}.json")).to_vec(), page_size: 2 },
    }).await;
}
```

- [ ] **Step 2: Run to verify it fails.** `cargo test -p ss-core contract` → compile error (`run` missing).

- [ ] **Step 3: Implement `run`** as these checks, in this order, each failing with a message that names the check and the observed values:
  1. **list_buckets** includes `bucket`.
  2. **list** at `parent(key)` (`key` up to and including its last `/`) returns `key` with `size == newest body len` and `bucket` set. **list** at `""` rolls up: `prefixes` is non-empty and `key` is not among `objects`.
  3. **pagination** on `paging.port` at `paging.prefix`: require `want_keys.len() > page_size` (fixture error otherwise). Drain with at most `want_keys.len() + 2` calls (fail "did not terminate" past that); no key on two pages; `next` never equals the token just sent; at least 2 calls; drained keys == `want_keys`. Then `list(.., Some("zzzz-not-a-real-token"))` must return `Err` — if it returns `Ok` whose first object is `want_keys[0]`, fail with "an unknown token restarted the listing"; any other `Ok` fails with "an unknown token was accepted".
  4. **list_versions**: length == bodies + markers; exactly one `is_latest` and its id is the last put id; `is_delete` ids == `delete_marker_ids` (as sets); non-delete ids == `version_ids` reversed; `modified` never increases from one entry to the next (ties allowed).
  5. **get_version** for each put id returns its body and echoes the id. For `""` and `"null"`: newest body, and `version_id` is concrete (not `""`, not `"null"`). For each marker id: `Err` (any kind).
  6. **head** of `key`: size of the newest body, `bucket` and `key` set.
  7. **not found** (`is_not_found()` must hold): `head`, `get_version(.., "")`, and `list_versions` of `does/not/exist.json`; `get_version(key, "definitely-not-a-version-id")`; `list` and `head` on bucket `"{bucket}-nope"`.

- [ ] **Step 4: Run the test.** `cargo test -p ss-core contract` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `test(s3): contract suite for every S3 port, run against the fake`

---

### Task 3: `Lazy` and `Cached` decorators

**Files:**
- Create: `crates/ss-core/src/svc/s3/lazy.rs`, `crates/ss-core/src/svc/s3/cached.rs`
- Modify: `crates/ss-core/src/svc/s3/mod.rs` (modules + `pub use lazy::Lazy; pub use cached::Cached;`)

**Interfaces:**
- Consumes: `S3Port`, `SvcError`, `is_current`, `crate::cache::Store`, `crate::ops::BoxFuture`, Task 2's `contract::{run, Fixture, Paging}` and `seeded_fake()`.
- Produces:
  ```rust
  pub struct Lazy { /* build fn + tokio::sync::OnceCell<Result<Arc<dyn S3Port>, String>> */ }
  impl Lazy { pub fn new(build: impl Fn() -> BoxFuture<anyhow::Result<Arc<dyn S3Port>>> + Send + Sync + 'static) -> Self }
  #[async_trait] impl S3Port for Lazy   // every call resolves the port first

  pub struct Cached { inner: Arc<dyn S3Port>, store: Arc<dyn Store> }
  impl Cached { pub fn new(inner: Arc<dyn S3Port>, store: Arc<dyn Store>) -> Self }
  #[async_trait] impl S3Port for Cached // only get_version touches the store; the rest delegate
  ```

- [ ] **Step 1: Write the failing tests.** `Lazy` (builder counts calls in an `Arc<AtomicUsize>`):

```rust
#[tokio::test] async fn lazy_does_not_build_until_used() { /* new(); assert count == 0 */ }
#[tokio::test] async fn lazy_builds_once_across_calls() { /* list_buckets ×3 → count == 1 */ }
#[tokio::test]
async fn lazy_build_failure_is_reported_on_every_call_and_built_once() {
    // build returns Err(anyhow!("no credentials").context("load AWS config"))
    // two calls: each Err whose to_string() == "load AWS config: no credentials"; count == 1
}
#[tokio::test] async fn lazy_satisfies_the_contract() { /* Lazy over the seeded fake (and over its paged view for Paging) */ }
```

  `Cached` (a `Counting` test port wraps `FakeS3` and counts `get_version` calls; the store is `cache::Dir` on a tempdir unless stated):

```rust
#[tokio::test] async fn explicit_versions_are_fetched_once() {
    // fake.insert("b","k","v1", r#"{"a":1}"#, t0); insert v2 r#"{"a":2}"#
    // get_version("b","k","v1") ×3 → each equals the fake's object exactly (meta, id, body); calls == 1
}
#[tokio::test] async fn current_is_never_served_from_cache_but_seeds_its_concrete_id() {
    // get_version("null") ×2 → version_id "v2", calls == 2; then get_version("v2") → calls still 2
}
#[tokio::test] async fn errors_pass_through() { /* get_version("nope") → is_not_found() */ }
#[tokio::test] async fn corrupt_entry_falls_through_and_is_rewritten() {
    // store.put("s3/b/k@v1", b"not json\nbody") first; get_version("v1") → correct object, calls == 1;
    // get_version("v1") again → calls == 1 (the rewritten entry is a hit)
}
#[tokio::test] async fn failed_cache_write_does_not_fail_the_fetch() {
    // a store whose put returns Err and get returns None; get_version("v1") → Ok
}
#[tokio::test] async fn cached_lazy_satisfies_the_contract() { /* Cached::new(Arc::new(Lazy over seeded fake), Dir) */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core svc::s3` → compile errors.

- [ ] **Step 3: Implement.** `Lazy`: `OnceCell::get_or_init` runs the build at most once even under concurrent callers; a build error is stored as `format!("{e:#}")` and every call returns `SvcError::Other(anyhow!(msg))`. `Cached::get_version`: the cache key is `s3/{bucket}/{key}@{version_id}`; for a non-current id, a decodable entry is returned without calling `inner`; after any successful fetch whose `version_id` is not current, write the entry under the concrete id, ignoring a write error. Entry format: one line of JSON `{"meta":{"bucket":…,"key":…,"size":…,"modified":<RFC 3339 or null>,"etag":…},"version_id":…}`, then `\n`, then the raw body; anything that fails to split or parse is a miss. The header serde struct is private to `cached.rs`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core svc::s3` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(s3): lazy construction and a version cache for S3 ports`

---

### Task 4: `Deps::s3`, the suggestion chain, and S3 formatting helpers

**Files:**
- Modify: `crates/ss-core/src/ops/operation.rs` (`Deps`), `crates/ss-core/tests/runner.rs:317` and `crates/ss/src/run.rs:119` (add `s3: None` to the `Deps` literals), `crates/ss-core/src/ops/mod.rs` (`pub mod s3;`)
- Create: `crates/ss-core/src/ops/s3/mod.rs`, `crates/ss-core/src/ops/s3/fixture.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `S3Port`, `FakeS3`, `Choice`, `SuggestFn`, `Input`, `Deps`, `config::{DiffDefaults, Environment}`.
- Produces:
  ```rust
  // Deps
  /// The S3 port, when this invocation has one.
  pub s3: Option<Arc<dyn S3Port>>,        // Default: None

  // ops::s3 (crate-visible helpers used by the three operations)
  pub(crate) fn port(deps: &Deps) -> anyhow::Result<Arc<dyn S3Port>>;   // None → Err "S3 is not configured"
  pub(crate) fn suggest_buckets() -> SuggestFn;
  pub(crate) fn suggest_keys() -> SuggestFn;
  pub(crate) fn suggest_versions() -> SuggestFn;
  pub(crate) fn short_id(id: &str) -> String;
  pub(crate) fn human_size(n: u64) -> String;
  pub(crate) fn age(then: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String;
  pub(crate) fn timestamp(t: Option<DateTime<Utc>>, fmt: &str) -> String;   // "-" for None

  // ops::s3::fixture (#[cfg(test)])
  pub(crate) const BUCKET: &str = "cfg-nonprod";
  pub(crate) fn t0() -> DateTime<Utc>;      // 2026-07-01T10:00:00Z
  pub(crate) fn fake() -> FakeS3;           // the objects below
  pub(crate) fn deps(s3: FakeS3) -> Deps;   // env name "dev", diff ignore ["**.updatedAt"], array_match {"$.rules[*]": "id"}
  ```
  `fake()` holds, in bucket `cfg-nonprod`:
  - `tenants/acme.json`: `v1` = `{"tier":"gold","rules":[{"id":"r1","limit":10}],"updatedAt":"2026-07-01"}` at `t0`; `v2` = `{"tier":"platinum","rules":[{"id":"r0","limit":5},{"id":"r1","limit":10}],"updatedAt":"2026-07-02"}` at `t0 + 24h`.
  - `tenants/notes.txt`: `t1` = `"hello\nworld\n"` at `t0`; `t2` = `"hello\nthere\n"` at `t0 + 1h`.

- [ ] **Step 1: Write the failing tests** in `ops/s3/mod.rs`:

```rust
#[test] fn short_id_truncates_past_twelve() {
    assert_eq!(short_id("abc"), "abc");
    assert_eq!(short_id("abcdefghijkl"), "abcdefghijkl");
    assert_eq!(short_id("abcdefghijklm"), "abcdefghijkl…");
}
#[test] fn human_size_uses_binary_units() {
    for (n, want) in [(0, "0 B"), (1023, "1023 B"), (1024, "1.0 KiB"), (1536, "1.5 KiB"),
                      (1 << 20, "1.0 MiB"), (5 << 30, "5.0 GiB")] { assert_eq!(human_size(n), want); }
}
#[test] fn age_is_minutes_hours_or_days() {
    let now = fixture::t0();
    for (ago, want) in [(Duration::minutes(30), "30m ago"), (Duration::seconds(3599), "59m ago"),
                        (Duration::hours(1), "1h ago"), (Duration::hours(23), "23h ago"),
                        (Duration::hours(24), "1d ago"), (Duration::hours(49), "2d ago"),
                        (Duration::minutes(-5), "0m ago")] { assert_eq!(age(Some(now - ago), now), want); }
    assert_eq!(age(None, now), "unknown");
}
#[tokio::test] async fn suggestions_chain_bucket_key_version() {
    let d = fixture::deps(fixture::fake());
    let call = |f: SuggestFn, pairs: &[(&str, &str)]| f(d.clone(), pairs.iter().copied().collect());
    assert_eq!(call(suggest_buckets(), &[]).await?, [Choice { value: "cfg-nonprod".into(), label: None }]);
    assert_eq!(call(suggest_keys(), &[("bucket", "cfg-nonprod")]).await?,
               [Choice { value: "tenants/".into(), label: Some("tenants/ …".into()) }]);
    assert_eq!(call(suggest_keys(), &[("bucket", "cfg-nonprod"), ("key", "tenants/")]).await?.len(), 2);
    let versions = call(suggest_versions(), &[("bucket", "cfg-nonprod"), ("key", "tenants/acme.json")]).await?;
    // labels: "null — current version",
    //         "v2 — 2026-07-02 10:00:00, {len(v2)} B (latest)",
    //         "v1 — 2026-07-01 10:00:00, {len(v1)} B"
}
#[tokio::test] async fn suggestions_are_empty_without_a_port_or_a_parent_value() { /* Deps::default(); and keys without bucket */ }
#[tokio::test] async fn suggestion_errors_propagate() { /* suggest_keys with bucket "missing" → Err */ }
```
(The test fn returns `anyhow::Result<()>`.)

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core ops::s3` → compile errors.

- [ ] **Step 3: Implement.** Suggestion semantics (each returns `Ok(vec![])` when `deps.s3` is `None`):
  - buckets: every bucket name, no label.
  - keys: empty unless `bucket` is set; one `list(bucket, input.str("key"), None)` page; prefixes first (label `"{p} …"`), then object keys (no label).
  - versions: empty unless `bucket` and `key` are set; first `Choice { value: "null", label: "null — current version" }`; then per version `"{short_id} — {timestamp %Y-%m-%d %H:%M:%S}, {size} B"` plus ` (latest)` and/or ` (delete marker)`.
  - `human_size`: below 1024 → `"{n} B"`; else divide by the largest power of 1024 not above `n` and print `"{:.1} {K|M|G|T|P|E}iB"`.
  - `age`: `now - then` clamped at zero; under 1h → `"{minutes}m ago"`, under 24h → `"{hours}h ago"`, else `"{days}d ago"` (whole units, truncated).

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core ops::s3` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): S3 dependency and the bucket, key, version suggestion chain`

---

### Task 5: `s3.version-diff`

**Files:**
- Create: `crates/ss-core/src/ops/s3/version_diff.rs`
- Modify: `crates/ss-core/src/ops/s3/mod.rs` (module + `pub use`), `crates/ss-core/src/ops/mod.rs` (register; update `builtin_registers_the_demo_ops` → `builtin_registers_every_operation`)

**Interfaces:**
- Consumes: Task 4 helpers and fixture; `diff::{json, text, Opts}`, `jsonutil::parse`, `Runner::step`, `find_cause`.
- Produces: `pub struct VersionDiff;` (`#[derive(Clone, Copy, Debug, Default)]`).

Meta: id `s3.version-diff`, name `Compare object versions`, summary `Semantic diff between two versions of one S3 object`, category `S3`, class `Read`.
Params, in order:
| name | label | type | required | default | help | suggest |
|---|---|---|---|---|---|---|
| bucket | Bucket | String | yes | | | buckets |
| key | Key | String | yes | | | keys |
| a | Version A | String | yes | | `older side; a version ID from the key's history` | versions |
| b | Version B | String | no | `null` | `newer side; "null" = current` | versions |
| ignore | Ignore paths | Json | no | | `extra ignore globs, e.g. "**.updatedAt,**.etag"` | |

- [ ] **Step 1: Write the failing tests** (`Runner::new(fixture::deps(fixture::fake())).execute(&VersionDiff, input)`):

```rust
#[tokio::test] async fn keyed_and_ignored_diff() {
    // a=v1 b=v2 → views[0] == View::summary([("Object","cfg-nonprod/tenants/acme.json"),
    //   ("A","v1 (2026-07-01 10:00)"),("B","v2 (2026-07-02 10:00)"),("Changes","+1 ~1")])
    // views[1] is View::Diff { left: "v1 (2026-07-01 10:00)", right: "v2 (2026-07-02 10:00)", delta }
    //   with delta.stats added 1, changed 1, total 2; change paths include "rules[id=r0]" and "tier"
}
#[tokio::test] async fn b_defaults_to_current() { /* only a=v1 → B label "v2 (2026-07-02 10:00)" */ }
#[tokio::test] async fn ignore_param_adds_to_config_globs() { /* ignore="**.tier" → Changes "+1" */ }
#[tokio::test] async fn non_json_bodies_fall_back_to_a_line_diff() {
    // notes.txt a=t1 b=t2 → summary ends with ("Mode","line diff (body is not valid JSON)");
    // views[1] == Doc { title: "Line diff: t1 (2026-07-01 10:00) → t2 (2026-07-01 11:00)", body contains "-world" and "+there", lang: "diff" }
}
#[tokio::test] async fn a_missing_version_fails_as_not_found() {
    // a=nope → Err(RunFailure); find_cause::<SvcError>(&f.error).is_some_and(SvcError::is_not_found);
    // format!("{:#}", f.error) starts with "fetch both versions: "
}
#[tokio::test] async fn without_s3_the_run_fails_clearly() { /* Deps::default() → error chain contains "S3 is not configured" */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core version_diff` → compile error.

- [ ] **Step 3: Implement.** Take `port(&r.deps)?` and clone what the steps need before calling `r.step` (its closure cannot borrow the runner). Step `fetch both versions`: `tokio::try_join!` the two `get_version` calls (errors converted to `anyhow`). Step `parse and diff`: if either body fails `jsonutil::parse`, the comparison is `diff::text` over `String::from_utf8_lossy` of both bodies (always — see Ruling 7); else `diff::json` with `Opts { ignore: deps.diff.ignore ++ input.globs("ignore"), array_match: deps.diff.array_match.clone(), ..Default::default() }`. Views: summary `Object` (`{bucket}/{key}`), `A`, `B` (version labels), then either `Mode` = `line diff (body is not valid JSON)` + `View::Doc { title: "Line diff: {A} → {B}", body, lang: "diff" }`, or `Changes` = `stats.to_string()` + `View::Diff { left: A, right: B, delta }`. Register in `builtin()`; the builtin test expects `["demo.hello", "demo.json-diff", "s3.version-diff"]`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): s3.version-diff compares two versions of one object`

---

### Task 6: `s3.object-inspect`

**Files:**
- Create: `crates/ss-core/src/ops/s3/object_inspect.rs`
- Modify: `crates/ss-core/src/ops/s3/mod.rs`, `crates/ss-core/src/ops/mod.rs` (register; builtin test list)

**Interfaces:**
- Consumes: Task 4 helpers and fixture; `jsonutil::{parse, pretty}`.
- Produces: `pub struct ObjectInspect;`

Meta: id `s3.object-inspect`, name `Inspect object`, summary `Metadata, version history, and body preview for one object`, category `S3`, class `Read`.
Params: `bucket` (Bucket, required, buckets), `key` (Key, required, keys), `version` (label `Version`, default `null`, help `version to preview; "null" = current`, versions).

- [ ] **Step 1: Write the failing tests:**

```rust
#[tokio::test] async fn inspects_metadata_history_and_json_body() {
    // key=tenants/acme.json → exactly 3 views:
    // View::summary([("Object","cfg-nonprod/tenants/acme.json"),("Version","v2 (2026-07-02 10:00)"),
    //   ("Size", format!("{} B", V2.len())),("Modified","2026-07-02 10:00:00 UTC"),("ETag","v2-etag"),("Versions","2")])
    // View::Table { title: "Version history", cols: ["ID","MODIFIED","SIZE",""],
    //   rows: [["v2","2026-07-02 10:00:00",len(v2),"latest"],["v1","2026-07-01 10:00:00",len(v1),""]] }
    // View::Doc { title: "Body", body: jsonutil::pretty(&parse(V2)), lang: "json" }  (body contains "\"tier\": \"platinum\"")
}
#[tokio::test] async fn text_body_preview() { /* notes.txt → Doc { title: "Body (first 12 bytes)", body: "hello\nthere\n", lang: "" } */ }
#[tokio::test] async fn long_text_is_cut_at_4096_bytes() { /* 5000 × 'a' → title "Body (first 4096 bytes)", body len 4096 */ }
#[tokio::test] async fn a_cut_through_a_character_keeps_whole_characters() { /* 4095 × 'a' + "é" + "tail" → title "Body (first 4095 bytes)" */ }
#[tokio::test] async fn binary_body_is_a_placeholder() { /* [0xff, 0xfe, 0x00] → Doc { title: "Body", body: "(binary, 3 bytes)", lang: "" } */ }
#[tokio::test] async fn history_marks_latest_and_delete_markers() {
    // fake.put v1; fake.delete; fake.put v3; inspect → table kind column ["latest", "delete marker", ""]
}
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core object_inspect` → compile error.

- [ ] **Step 3: Implement.** Steps `list versions` then `fetch body` (`get_version(bucket, key, input.str("version"))`). Row: `[short_id(id), timestamp(%Y-%m-%d %H:%M:%S), size, kind]`, kind `latest` when `is_latest`, `delete marker` when `is_delete` (wins over latest), else empty. Summary as in the test (`Size` is `"{size} B"` of the fetched object, `Modified` uses `%Y-%m-%d %H:%M:%S %Z`, `Versions` the history length). Body preview: JSON → pretty; else take the first 4096 bytes, and if they are not UTF-8 only because the cut split the last character, shorten to the last whole character; still not UTF-8 → `(binary, {total len} bytes)` titled `Body`; else titled `Body (first {n} bytes)` with empty lang. Register; builtin list becomes `["demo.hello", "demo.json-diff", "s3.object-inspect", "s3.version-diff"]`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): s3.object-inspect shows metadata, history, and a body preview`

---

### Task 7: `s3.prefix-audit`

**Files:**
- Create: `crates/ss-core/src/ops/s3/prefix_audit.rs`
- Modify: `crates/ss-core/src/ops/s3/mod.rs`, `crates/ss-core/src/ops/mod.rs` (register; builtin list)

**Interfaces:**
- Consumes: Task 4 helpers and fixture; `FakeS3::{insert, paged}`.
- Produces: `pub struct PrefixAudit;`

Meta: id `s3.prefix-audit`, name `Audit prefix`, summary `Object count, size, age spread, and largest objects under a prefix`, category `S3`, class `Read`.
Params: `bucket` (Bucket, required, buckets); `prefix` (label `Prefix`, help `empty = whole bucket`, keys); `max` (label `Max objects`, Int, default `10000`, help `stop scanning after this many objects`); `top` (label `Top N largest`, Int, default `10`).

- [ ] **Step 1: Write the failing tests.** A local helper `logs(n, page_size) -> Deps` builds `fixture::fake()`, inserts `logs/{i:03}.json` (version `v1`, body `{"n":i}`, modified `t0 + i hours`) for `i in 0..n`, and passes `fake.paged(page_size)` to `fixture::deps` when `page_size > 0`, else the fake itself; `field(&views, "Objects")` reads a summary value.

```rust
#[tokio::test] async fn audits_the_fixture_prefix() { /* prefix tenants/ → Objects "2" */ }
#[tokio::test] async fn drains_every_page() { /* logs(7, 2), max 10000 → Objects "7" */ }
#[tokio::test] async fn page_size_does_not_change_the_answer() {
    // for page in [1,2,3,5,6,7,100]: logs(6, page) → Objects "6" and Total size equal to the unpaged run's
}
#[tokio::test] async fn max_stops_within_one_page() {
    // logs(20, 3), max 5 → Objects "{n} (stopped at --max 5)" with 5 <= n <= 7
}
#[tokio::test] async fn descends_into_nested_prefixes() {
    // keys logs/a.json, logs/2026/b.json, logs/2026/07/c.json; prefix logs/ → Objects "3"
}
#[tokio::test] async fn max_stops_before_descending_into_prefixes() {
    // page size 3; keys p/1, p/2, p/sub/3 … p/sub/9 (prefix listing page 1 = [p/1, p/2, p/sub/]); max 2
    // → Objects "2 (stopped at --max 2)"
}
#[tokio::test] async fn summary_and_top_table() {
    // insert logs/000.json, logs/001.json, logs/002.json with bodies of 10, 20, 30 bytes at t0, t0+1h, t0+2h; top 2 →
    // summary keys in order Prefix "cfg-nonprod/logs/", Objects "3", Total size human_size(sum),
    //   Newest starts "logs/002.json (" and ends " ago)", Oldest starts "logs/000.json (";
    // Table { title: "Top 2 by size", cols ["KEY","SIZE","MODIFIED"], rows[0][0] == "logs/002.json", rows[0][2] == "2026-07-01 12:00" }
}
#[tokio::test] async fn empty_prefix_reports_zero() { /* prefix none/ → only View::summary([("Prefix","cfg-nonprod/none/"),("Objects","0")]) */ }
#[tokio::test] async fn non_positive_top_means_ten() { /* logs(12, 0), top 0 → title "Top 10 by size" */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core prefix_audit` → compile error.

- [ ] **Step 3: Implement.** `limit = (max > 0).then_some(max)`; `top = if top <= 0 { 10 } else { top }`. Step `scan prefix` walks depth-first: for each page of the current prefix, append its objects; if the limit is reached, stop the whole scan and mark it truncated; otherwise scan each of the page's prefixes completely by the same rule (stopping everything as soon as the limit is reached), then follow `next`. Recursion needs `Box::pin` or an explicit stack; keep the visiting order above. Results: empty → only `Prefix` and `Objects "0"`. Else `total` = sum of sizes; oldest = first object with the strictly smallest `modified`, newest = first with the strictly largest (`None` sorts oldest); rows = objects stably sorted by size descending, first `top`, each `[key, human_size(size), timestamp(%Y-%m-%d %H:%M)]`; `Objects` = count, plus ` (stopped at --max {max})` when truncated; `Newest`/`Oldest` = `"{key} ({age(modified, Utc::now())})"`. Table title `Top {rows.len()} by size`. Register; builtin list becomes `["demo.hello", "demo.json-diff", "s3.object-inspect", "s3.prefix-audit", "s3.version-diff"]`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): s3.prefix-audit summarizes what lives under a prefix`

---

### Task 8: AWS config, identity, and the account guardrail

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`), `crates/ss/Cargo.toml`, `crates/ss/src/lib.rs` (`mod aws;`), `Cargo.lock`
- Create: `crates/ss/src/aws/mod.rs`, `crates/ss/src/aws/awsx.rs`

**Interfaces:**
- Produces (`crate::aws::awsx`, all `pub(crate)`):
  ```rust
  pub(crate) async fn load(profile: &str, region: &str) -> aws_config::SdkConfig;   // empty = SDK default chain
  pub(crate) struct Identity { pub account: String, pub arn: String, pub user_id: String }
  pub(crate) async fn check_identity(cfg: &SdkConfig, profile: &str) -> anyhow::Result<Identity>; // errors pass through friendly()
  pub(crate) fn is_expired_sso(chain: &str) -> bool;
  pub(crate) fn friendly(err: anyhow::Error, profile: &str) -> anyhow::Error;
  pub(crate) fn check_account(id: &Identity, env: &Environment) -> anyhow::Result<()>;
  ```

- [ ] **Step 1: Add dependencies.** Workspace: `aws-config = { version = "1.8", features = ["behavior-version-latest"] }`, `aws-sdk-s3 = "1.122"`, `aws-sdk-sts = "1.97"`. `ss`: those three as `.workspace = true`; dev-dependency `ss-core = { path = "../ss-core", features = ["test-support"] }`. Run `cargo update -p aws-config -p aws-sdk-s3 -p aws-sdk-sts` only if needed; then `make msrv` → PASS (resolver 3 picks 1.88-compatible releases; if it does not, pin `=` versions that are).

- [ ] **Step 2: Write the failing tests** in `awsx.rs`:

```rust
#[test] fn expired_sso_shapes_are_recognised() {
    for needle in ["SSOProviderInvalidToken", "token has expired", "InvalidGrantException",
                   "ExpiredToken", "failed to refresh cached credentials"] {
        assert!(is_expired_sso(&format!("dispatch failure: {needle}: details")), "{needle}");
    }
    assert!(!is_expired_sso("AccessDenied: not authorized"));
}
#[test] fn friendly_names_the_login_command() {
    let e = friendly(anyhow!("ExpiredToken: the token is expired"), "dev");
    assert_eq!(format!("{e:#}"), "SSO session expired — run `aws sso login --profile dev`: ExpiredToken: the token is expired");
    let e = friendly(anyhow!("ExpiredToken"), "");
    assert!(e.to_string().contains("--profile <profile>"));
    assert_eq!(format!("{:#}", friendly(anyhow!("AccessDenied"), "dev")), "AccessDenied");
}
#[test] fn account_guardrail() {
    let env = Environment { name: "dev".into(), profile: "dev-sso".into(), account_id: "111111111111".into(), ..Default::default() };
    let id = |a: &str| Identity { account: a.into(), arn: String::new(), user_id: String::new() };
    assert!(check_account(&id("111111111111"), &env).is_ok());
    assert!(check_account(&id("222222222222"), &Environment { account_id: String::new(), ..env.clone() }).is_ok());
    assert_eq!(check_account(&id("222222222222"), &env).expect_err("mismatch").to_string(),
        r#"account mismatch: profile "dev-sso" resolves to account 222222222222, but env "dev" expects 111111111111"#);
}
```

- [ ] **Step 3: Run to verify they fail.** `cargo test -p ss aws::awsx` → compile errors.

- [ ] **Step 4: Implement.** `load`: `aws_config::defaults(BehaviorVersion::latest())`, `.profile_name(profile)` and `.region(Region::new(region.to_owned()))` only when non-empty, `.load().await`. `check_identity`: `aws_sdk_sts::Client::new(cfg).get_caller_identity().send()`; map an `SdkError` with `anyhow::Error::from` then `friendly`. `friendly`: when `is_expired_sso(&format!("{err:#}"))`, return `err.context(format!("SSO session expired — run `aws sso login --profile {p}`"))` with `p` = profile or `<profile>`; else `err` unchanged.

- [ ] **Step 5: Run the tests and builds.** `cargo test -p ss aws::awsx` → PASS. `make check` → PASS. `make msrv` → PASS. `cargo build --release` → succeeds; `ldd target/release/ss` shows no `libssl`.

- [ ] **Step 6: Commit.** `feat(aws): config loading, identity check, and the account guardrail`

---

### Task 9: `AwsS3` adapter and the MinIO contract run

**Files:**
- Create: `crates/ss/src/aws/s3.rs`
- Modify: `crates/ss/src/aws/mod.rs`, `Makefile` (`contract` target), `.github/workflows/ci.yml` (`contract` job)

**Interfaces:**
- Consumes: `ss_core::svc::{SvcError, s3::*}`, `contract::{run, Fixture, Paging}` (dev-dep feature), `awsx::load`.
- Produces:
  ```rust
  pub(crate) const MAX_BODY: u64 = 32 << 20;
  pub(crate) struct Options { pub endpoint: Option<String>, pub max_body: u64, pub page_size: Option<i32> } // Default: None, MAX_BODY, None
  pub(crate) struct AwsS3 { client: aws_sdk_s3::Client, max_body: u64, page_size: Option<i32> }
  impl AwsS3 { pub(crate) fn new(cfg: &SdkConfig, opts: Options) -> Self }   // endpoint → endpoint_url + force_path_style(true)
  #[async_trait] impl S3Port for AwsS3
  fn is_not_found(code: Option<&str>, message: Option<&str>, status: Option<u16>, had_version_id: bool) -> bool;
  ```

- [ ] **Step 1: Write the failing unit test:**

```rust
#[test] fn classify_not_found_shapes() {
    let cases = [
        (Some("NoSuchKey"), None, Some(404), false, true),
        (Some("NoSuchBucket"), None, Some(404), false, true),
        (Some("NoSuchVersion"), None, Some(404), true, true),
        (Some("NotFound"), None, Some(404), false, true),
        (None, None, Some(404), false, true),
        (Some("AccessDenied"), None, Some(403), false, false),
        (Some("InvalidArgument"), Some("Invalid version id specified"), Some(400), true, true),
        (Some("InvalidArgument"), Some("Invalid version id specified"), Some(400), false, false),
        (Some("InvalidArgument"), Some("Invalid continuation token"), Some(400), true, false),
    ];
    for (code, msg, status, had, want) in cases {
        assert_eq!(is_not_found(code, msg, status, had), want, "{code:?} {msg:?} {status:?} {had}");
    }
}
```

- [ ] **Step 2: Write the env-gated contract test** (`#[cfg(test)] mod minio`). `SS_S3_TEST_ENDPOINT` unset → `eprintln!("skipping: SS_S3_TEST_ENDPOINT is not set")` and return. Credentials `SS_S3_TEST_KEY` / `SS_S3_TEST_SECRET` (default `minioadmin` both), region `us-east-1`, static credentials provider. With a raw `aws_sdk_s3::Client` (endpoint + path style): create bucket `ss-it-{unix nanos}`, enable versioning, put the three contract bodies on `cfg/tenants/acme.json` in the order put, put, **delete** (keep the marker's `version_id`), put; put `{}` at `cfg/many/{a..e}.json`. Then:
  - `contract_holds_against_s3`: `contract::run` with `port` = `AwsS3::new(&cfg, Options { endpoint, ..Default })`, paging port with `page_size: Some(2)`, `delete_marker_ids: vec![marker]`.
  - `max_body_is_enforced`: `AwsS3` with `max_body: 10`; `get_version` of the key → `Err` whose text contains `exceeds the 10 byte limit`.

- [ ] **Step 3: Run to verify failure.** `cargo test -p ss aws::s3` → compile errors.

- [ ] **Step 4: Implement `AwsS3`.**
  - `list_buckets`: sorted by name; `created` from `creation_date`.
  - `list`: `ListObjectsV2` with `prefix`, `delimiter "/"`, `continuation_token`, `max_keys` = `page_size`; objects get `bucket` set; `next` = `next_continuation_token` only when `is_truncated`.
  - `list_versions`: `ListObjectVersions` with `prefix = key`, drained via `key_marker`/`version_id_marker` while `is_truncated`; keep only entries whose key equals `key`; versions and delete markers (`is_delete`, size 0); stable sort by `modified` descending; empty → `SvcError::not_found("{bucket}/{key}")`.
  - `get_version`: send `version_id` only when not current; `content_length > max_body` → `Other("{bucket}/{key}: object is {n} bytes, exceeds the {max} byte limit")`; read the body chunk by chunk and fail with `Other("{bucket}/{key}: body exceeds the {max} byte limit")` as soon as it passes `max_body`; `version_id` = the response's id when non-empty, else the requested one; `size` = body length.
  - `head`: `HeadObject` → meta.
  - Errors: one helper turns an `SdkError<E, HttpResponse>` (`E: ProvideErrorMetadata + Error + Send + Sync + 'static`) into `SvcError` using `err.code()`, `err.message()`, `err.raw_response().map(|r| r.status().as_u16())`, and `is_not_found(…, had_version_id)` → `SvcError::not_found_from(what, err)`; otherwise `SvcError::Other(anyhow::Error::from(err))`. `had_version_id` is true only for `get_version` with an explicit id. `is_not_found`: code in `NoSuchKey | NoSuchBucket | NoSuchVersion | NotFound`, or status 404, or (`had_version_id` and code `InvalidArgument` and the lowercased message contains `version id`).

- [ ] **Step 5: Add the contract entry points.** `Makefile`:
  ```make
  ## contract: S3 contract suite against MinIO (needs SS_S3_TEST_ENDPOINT, e.g. http://127.0.0.1:9000)
  .PHONY: contract
  contract:
  	@test -n "$(SS_S3_TEST_ENDPOINT)" || { echo "set SS_S3_TEST_ENDPOINT"; exit 2; }
  	cargo test -p ss aws::s3::minio -- --nocapture
  ```
  CI: a `contract` job on `ubuntu-latest` that runs `docker run -d -p 9000:9000 -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin minio/minio server /data`, waits for `curl -sf http://127.0.0.1:9000/minio/health/live`, then `make contract` with `SS_S3_TEST_ENDPOINT: http://127.0.0.1:9000`.

- [ ] **Step 6: Run.** `cargo test -p ss aws::s3` → PASS (MinIO tests print the skip line). Then against MinIO:
  ```bash
  docker run -d --rm --name ss-minio -p 9000:9000 -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin minio/minio server /data
  until curl -sf http://127.0.0.1:9000/minio/health/live; do sleep 1; done
  SS_S3_TEST_ENDPOINT=http://127.0.0.1:9000 make contract     # → both minio tests PASS
  ```
  Leave the container running for Task 11. If docker or the image is unavailable, report DONE_WITH_CONCERNS stating the MinIO run did not happen. `make check` → PASS.

- [ ] **Step 7: Commit.** `feat(aws): S3 adapter with not-found normalization and a MinIO contract run`

---

### Task 10: Wire S3 into every invocation

**Files:**
- Modify: `crates/ss/src/services.rs`, `crates/ss/src/run.rs`, `crates/ss/src/aws/mod.rs`, `crates/ss/src/lib.rs`
- Create: `crates/ss/tests/s3.rs`

**Interfaces:**
- Consumes: `Lazy`, `Cached`, `AwsS3`, `Options`, `awsx::{load, check_identity, check_account}`.
- Produces:
  ```rust
  // services.rs (re-exported from lib.rs)
  pub type S3Factory = Arc<dyn Fn(&Environment, &Vars) -> Arc<dyn S3Port> + Send + Sync>;
  pub struct Services { pub registry: Registry, pub vars: Vars, /// Builds the S3 port for the resolved environment.
                        pub s3: S3Factory }
  impl Services {
      pub fn from_process() -> Self;                          // s3: aws::s3_factory()
      pub fn isolated(root: &Path) -> Self;                   // s3: a port whose every call fails "S3 is not configured"
      pub fn with_s3(self, port: Arc<dyn S3Port>) -> Self;    // every invocation uses `port`
  }
  // aws/mod.rs
  pub(crate) fn s3_factory() -> S3Factory;
  ```

- [ ] **Step 1: Write the failing e2e tests** in `crates/ss/tests/s3.rs` (same `ss_with` shape as `tests/e2e.rs`; a `fake()` seeds `cfg-nonprod/tenants/acme.json` v1/v2 with the Task 4 bodies and times):

```rust
#[tokio::test] async fn ops_list_shows_the_s3_operations() { /* out contains s3.version-diff, s3.object-inspect, s3.prefix-audit */ }
#[tokio::test] async fn demo_and_ops_list_run_without_s3() {
    // Services::isolated (unconfigured S3): `ss ops list` and `ss run demo.hello --name x` both exit 0, stderr empty
}
#[tokio::test] async fn version_diff_through_the_cli_as_json() {
    // with_s3(fake): ss run s3.version-diff --bucket cfg-nonprod --key tenants/acme.json --a v1 -o json
    // → exit 0; parsed stdout's first view is the summary with Object "cfg-nonprod/tenants/acme.json"
}
#[tokio::test] async fn object_inspect_through_the_cli_as_text() { /* exit 0; stdout contains "Version history" and "v2" */ }
#[tokio::test] async fn a_missing_key_exits_1_with_one_line() {
    // --key nope.json → Exit::Failed; stderr is one line starting "ss run s3.object-inspect: " containing "not found"
}
#[tokio::test] async fn an_unconfigured_s3_exits_1() { /* isolated: ss run s3.prefix-audit --bucket b → Exit::Failed, stderr contains "S3 is not configured" */ }
```
(Read `render::json` for the exact JSON shape of a summary view before writing the assertion.)

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss --test s3` → compile errors.

- [ ] **Step 3: Implement.** `App::build` gains the factory argument (`pipeline` passes `&services.s3`) and sets `deps.s3 = Some(Arc::new(Cached::new((factory)(&env, vars), Arc::clone(&cache))))` — the same store `--no-cache` already swaps for `cache::Nop`. `s3_factory()` returns a closure that captures `env.clone()` and `vars::get(vars, "SS_S3_ENDPOINT")` and returns `Arc::new(Lazy::new(…))` whose build: `awsx::load(&env.profile, &env.region)`; with a non-empty endpoint → `AwsS3::new(&cfg, Options { endpoint: Some(ep), ..Default::default() })` (no STS call); otherwise `check_identity`, then `check_account`, then `AwsS3::new(&cfg, Options::default())`. `isolated`'s factory returns a `Lazy` whose build fails with `anyhow!("S3 is not configured")`. Remove the `s3: None` placeholder Task 4 put in `run.rs`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(cli): every invocation gets a lazily built, cached S3 port`

---

### Task 11: Release `0.1.0-alpha.1`

**Files:**
- Modify: `Cargo.toml` (`version = "0.1.0-alpha.1"`), `crates/ss/Cargo.toml` (`ss-core` pin `=0.1.0-alpha.1`, both the dependency and the dev-dependency if it carries a version), `Cargo.lock`, `CHANGELOG.md`, `README.md`, `docs/README.md`, `docs/config.example.yaml` (only if it lacks `account_id`)

- [ ] **Step 1: Bump the version.** `cargo test -p ss --test e2e version_is_the_workspace_version` → PASS; `cargo run -p ss -- --version` prints `ss 0.1.0-alpha.1`.

- [ ] **Step 2: CHANGELOG.** Rename `## [Unreleased]` to `## [0.1.0-alpha.1] - 2026-11-09` (keep its entries) and add a fresh empty `## [Unreleased]` above it. Under the release's `### Added`, add:
  - Read-only S3 operations `s3.version-diff`, `s3.object-inspect`, and `s3.prefix-audit`, with bucket → key → version suggestions.
  - AWS access through the named profile and region of the selected environment; an environment's `account_id` is checked against the caller identity before the first S3 call; an expired SSO session names the `aws sso login` command.
  - `SS_S3_ENDPOINT` targets an S3-compatible endpoint such as MinIO (path-style, no identity check).
  - Fetched object versions are cached locally (`--no-cache` bypasses); bodies over 32 MiB are refused.
  - `make contract` runs the S3 contract suite against MinIO.

- [ ] **Step 3: README and docs.** In `README.md`: change "the binary: CLI, TUI, AWS adapters" to "the binary: CLI and AWS adapters"; add an `## S3` section with the three operations (one example each), how the environment, `--profile`, and `--region` pick the AWS identity, the `account_id` guardrail, that every operation is read-only, `SS_S3_ENDPOINT`, and a MinIO quick start (the `docker run` line from Task 9 plus `AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin SS_REGION=us-east-1 SS_S3_ENDPOINT=http://127.0.0.1:9000 ss run s3.prefix-audit --bucket <bucket>`). Update `docs/README.md` wherever it says S3 is not available yet. No Go, porting, or plan references (Global Constraints).

- [ ] **Step 4: Verify.** `make check` → PASS; `make msrv` → PASS; `cargo build --release` → succeeds.

- [ ] **Step 5: Smoke the release binary against MinIO** (skip with a stated reason if Task 9's container is not running). Pick the `ss-it-*` bucket the contract run left (`docker exec ss-minio ls /data`), then with the env from Step 3:
  ```bash
  target/release/ss run s3.prefix-audit --bucket $B --prefix cfg/            # exit 0, Objects 6
  target/release/ss run s3.object-inspect --bucket $B --key cfg/tenants/acme.json   # exit 0, history has a "delete marker" row
  target/release/ss run s3.version-diff --bucket $B --key cfg/tenants/acme.json --a <first put id> -o json   # exit 0
  target/release/ss run s3.object-inspect --bucket $B --key nope.json; echo $?      # one "not found" line, 1
  ```
  Then `docker stop ss-minio`. Record the outputs in the task report.

- [ ] **Step 6: Commit.** `chore(release): 0.1.0-alpha.1`

---

## Task index

| # | Title | Main files | Depends on |
|---|---|---|---|
| 1 | S3 port, types, `FakeS3` | `ss-core/src/svc/**` | — |
| 2 | Contract suite | `svc/s3/contract.rs` | 1 |
| 3 | `Lazy` + `Cached` | `svc/s3/{lazy,cached}.rs` | 1, 2 |
| 4 | `Deps::s3`, suggestions, helpers | `ops/operation.rs`, `ops/s3/mod.rs` | 1 |
| 5 | `s3.version-diff` | `ops/s3/version_diff.rs` | 4 |
| 6 | `s3.object-inspect` | `ops/s3/object_inspect.rs` | 4 |
| 7 | `s3.prefix-audit` | `ops/s3/prefix_audit.rs` | 4 |
| 8 | AWS config + guardrail | `ss/src/aws/awsx.rs`, manifests | — |
| 9 | `AwsS3` + MinIO run | `ss/src/aws/s3.rs`, `Makefile`, CI | 2, 8 |
| 10 | Wiring + e2e | `ss/src/{services,run}.rs`, `ss/tests/s3.rs` | 3, 5–7, 9 |
| 11 | Release `0.1.0-alpha.1` | manifests, CHANGELOG, README, docs | 10 |
