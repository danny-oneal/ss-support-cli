# DynamoDB Read-Only Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a read-only DynamoDB slice (port, fake, lazy decorator, AWS adapter, three operations) so `ss-support` can compare, version-trace, and index-check items in a non-production account or a local DynamoDB-compatible endpoint.

**Architecture:** `ss-core` gains `svc::ddb` (the `DdbPort` trait, its types, `FakeDdb`, the `Lazy` decorator, and a contract suite) and `ops::ddb` (three `Class::Read` operations plus table, index, and version suggestions); `Deps` carries an optional `Arc<dyn DdbPort>`. The `ss-support` crate gains `aws::ddb` (the `AwsDdb` adapter) and wires a lazily built port into every invocation through a factory on `Services`, exactly as S3 is wired, so commands that never touch DynamoDB never load AWS config.

**Tech Stack:** Rust 2024, tokio, async-trait, serde_json, aws-sdk-dynamodb 1.x with serde_dynamo 4.x (newest releases that build under the 1.88 MSRV with resolver 3), dynalite for the env-gated contract run.

**Spec:** `docs/2026-11-07-rust-port-master-plan.md` — Phase 3 (DDB port, fake, `Lazy`, contract suite), Phase 4 (`aws/ddb.rs`), Phase 5 (DDB operations). The scope (read-only; no `put_item`, no `ddb.item-edit`) is the user's decision of 2026-11-10 and overrides those phases wherever they include writes. The slice follows `docs/2026-11-09-s3-alpha-slice.md` for structure and conventions.

**Reference behavior** (read-only, sibling checkout — read it, never cite it in the Rust tree): `/home/engineer/Code/starbucks/ss-support-cli-go/internal/{ddbsvc,ops/ddbops}/`. Values in this plan were transcribed from there; where the plan and that code disagree, the plan wins (see Rulings).

## Global Constraints

- **No Go in the Rust tree.** Comments, doc comments, test names, test messages, manifests, README, CHANGELOG written by these tasks must not mention Go, porting, "ported from", phases, plans, or design narrative. Describe behavior only. (This file lives in `docs/`, where such reasoning belongs.)
- **Read-only.** `DdbPort` has no write method; every `ddb.*` operation is `Class::Read`; nothing in the tree calls `put_item`, `update_item`, `delete_item`, `batch_write_item`, or `transact_write_items`. Task 10 pins this.
- Edition 2024; `rust-version = "1.88"` stays. `make msrv` (`cargo +1.88 check --workspace --all-targets --locked`) must pass; if an SDK release would raise the floor, pin an older one (`cargo update -p <crate> --precise <ver>`) rather than raising MSRV.
- `make check` (rustfmt check, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`) passes at the end of every task.
- `missing_docs` is on: every public item gets a `///` doc; every new module gets a `//!` doc.
- `thiserror` in `ss-core` (`SvcError` is reused); `anyhow` in operations and `ss-support`. `#[async_trait]` only on `DdbPort` (dyn-dispatched); native `async fn` elsewhere.
- `ss-core` never reads `std::env`; values come from the `Vars` snapshot. `ss-core` gets no AWS dependency.
- No `unwrap()` outside tests; `expect("…")` only for invariants. Anything that reaches output uses `BTreeMap`.
- Typed over stringly: closed sets are enums (`Projection`, `AttrType`), never raw strings. Narrowing numeric conversions use `try_from` (the `--limit` clamp is converted with `i32::try_from`), never `as`.
- Every public fallible item documents its failure cases in an `# Errors` section. `FakeDdb` takes its `std::sync::Mutex` guard, copies what it needs out, and drops the guard before any `.await`.
- Operation ids, names, summaries, param names/labels/defaults/help, view titles, column names, and summary keys are exact copy from this plan (they are the `-o json` contract).
- One commit per task, conventional-commit subject with scope, ending with the session's attribution trailer. Never push, tag, or merge. Confirm with the user that committing is permitted before the first commit (CLAUDE.md).

## Review Focus

1. **A version label that is not `V<digits>`** (`V+1`, `V`, `v-3`, `V99999999999`, `weird`) → sorts after every real version, never panics, never parses as a number. Pinned by Task 6 (`version_num_*`, `sort_by_version_*`).
2. **A key spec with `=` inside a value, a blank spec, or a number given for a numeric key attribute** (`Id=42` on a table whose `Id` is `N`) → the first `=` splits, blank is an error naming the problem, and `42` reaches DynamoDB as a number so the item is found. Pinned by Task 6 (`parse_key_*`) and Task 5 (`numeric_keys_are_looked_up_as_numbers`).
3. **A hot partition** (thousands of versions under one key, or thousands of entries under one GSI hash value) → `item-history` returns at most `--limit` versions and `gsi-consistency` stops after 10 000 candidates with an `INCONCLUSIVE` answer, never an unbounded loop. Pinned by Task 8 (`limit_caps_the_total`) and Task 9 (`scan_stops_after_the_candidate_cap`).
4. **Commands that never touch DynamoDB** (`ops list`, `presets`, demo and S3 operations) → must not resolve AWS config or need credentials; a DynamoDB run with nothing configured exits 1 with `DynamoDB is not configured`. Pinned by Task 3 (`lazy_does_not_build_until_used`) and Task 11 (`demo_and_ops_list_run_without_ddb`, `an_unconfigured_ddb_exits_1`).
5. **A missing table, a missing item, or a GSI the table does not have** → exit 1 with one actionable line naming the step and the thing that was not found, never an empty diff. Pinned by Task 7 (`a_missing_item_fails_as_not_found`), Task 9 (`an_unknown_index_is_reported`), and Task 11 (`a_missing_item_exits_1_with_one_line`).

## Rulings

Decisions made while writing this plan, each with its reason. Executors do not re-open these.

1. **The port is read-only.** `DdbPort` has no `put_item`; no `ddb.item-edit`; the contract suite seeds through the raw SDK client, not the port. — The user's decision.
2. **`Item` and `Key` are `serde_json::Map<String, Value>`** (keys sorted, no `preserve_order`). The adapter passes every unmarshalled item through `jsonutil::normalize` so numbers are f64 and diff output matches S3 JSON. Sets and binary attributes are not pinned by any test. — One shape for DynamoDB items and S3 bodies is what makes cross-service comparison cheap.
3. **`query` is page-based and `QuerySpec.limit` is the per-request page size** (DynamoDB's `Limit`); callers drain with `ItemPage.next` → `QuerySpec.exclusive_start`. `QueryIndex` is `query` with an index name; its `next` key carries the base-table key attributes plus the index keys.
4. **`FakeDdb` models index entries separately from table items.** A `put` derives each index's entry (`ALL` projects the whole item; `KEYS_ONLY` and `INCLUDE` project the base and index key attributes only); `set_index_entry` and `remove_index_entry` then make an index stale or incomplete, so drift and `MISSING` are exercised against a fake instead of a hand-rolled override. `paged(n)` caps every response at `n` items. The fake evaluates only `<name> = <placeholder>` key conditions and errors on anything else. — The reference fake had no index model and tests overrode `QueryIndex`.
5. **`Lazy` is a second copy, not a generic.** No `Cached` decorator for DynamoDB. — The two ports' delegating methods differ, so a generic saves nothing; items are mutable and `GetItem` is read with `ConsistentRead`, so caching them would hide the truth the operator is asking for.
6. **The shared SDK-config-and-guardrail logic moves into one helper** used by both factories (Task 4). — The two factories would otherwise carry identical copies.
7. **dynalite is the contract-run backend** (`npx dynalite@3.2.2`), as it was for the reference. — Pure Node, no container, supports GSIs.
8. **`gsi-consistency` scans at most 10 000 candidates** under the index hash value and then answers `INCONCLUSIVE`. — The reference drained the partition without bound.
9. **`item-history --limit` truncates the result to `limit` items** after draining, clamped to 1..=10000 (default 50). — The reference only stopped fetching at the first page that reached the limit, so the table could exceed it.
10. **Version labels are parsed without a regex dependency**: `V` or `v` followed by one or more ASCII digits that fit a `u32`; nothing else parses. Labels that do not parse sort after those that do, alphabetically.
11. **Environment variables.** `SS_SUPPORT_DDB_ENDPOINT` points the adapter at a DynamoDB-compatible endpoint and skips the STS identity check (credentials are still resolved). Tests use `SS_SUPPORT_DDB_TEST_ENDPOINT`, `SS_SUPPORT_DDB_TEST_KEY`, `SS_SUPPORT_DDB_TEST_SECRET` (both default `test`). Region comes from the environment, as for S3.
12. **No version bump, tag, or release is part of this plan.** The result lands under `[Unreleased]`.
13. **`ddb.item-compare` and the history diff ignore nothing by default** beyond the config's diff ignore globs and the `ignore` param; `ddb.item-history` additionally ignores the version attribute itself.

---

## File Structure

`ss-core`:
- `src/svc/mod.rs` — add `pub mod ddb;`.
- `src/svc/ddb/mod.rs` — types, `DdbPort`, `key_string`; re-exports.
- `src/svc/ddb/fake.rs` — `FakeDdb` (test-support).
- `src/svc/ddb/contract.rs` — `table_info`, `seed_items`, `run` (test-support).
- `src/svc/ddb/lazy.rs` — `Lazy`.
- `src/ops/operation.rs` — `Deps::ddb`.
- `src/ops/ddb/mod.rs` — `port()`, suggestion chain, key/version/preview helpers.
- `src/ops/ddb/fixture.rs` — `#[cfg(test)]` shared fake deps for op tests.
- `src/ops/ddb/{item_compare,item_history,gsi_consistency}.rs` — the operations.
- `src/ops/mod.rs` — `pub mod ddb;`, registrations in `builtin()`.

`ss-support`:
- `Cargo.toml` — `aws-sdk-dynamodb`, `serde_dynamo`.
- `src/aws/awsx.rs` — the shared `sdk_config` helper (Task 4).
- `src/aws/ddb.rs` — `AwsDdb` + dynalite contract test.
- `src/aws/mod.rs` — `ddb_factory()`; `s3_factory()` uses the shared helper.
- `src/services.rs`, `src/run.rs`, `src/lib.rs` — factory wiring.
- `tests/ddb_guards.rs`, `tests/ddb.rs` — guard and e2e tests through `run_with`.

Root: `Cargo.toml` (workspace deps), `Makefile` (`contract-ddb`), `.github/workflows/ci.yml` (`contract-ddb` job), `CHANGELOG.md`, `README.md`.

---

### Task 1: DDB port and types

**Files:**
- Modify: `crates/ss-core/src/svc/mod.rs`
- Create: `crates/ss-core/src/svc/ddb/mod.rs`

**Interfaces:**
- Produces (`ss_core::svc::ddb`):
  ```rust
  pub type Item = serde_json::Map<String, serde_json::Value>;
  pub type Key = Item;

  #[derive(Debug, Clone, Default, PartialEq)]
  pub struct QuerySpec {
      pub table: String,                              // ignored by `query_index`, which takes the table as an argument
      pub key_condition: String,                      // "#pk = :pk"
      pub names: BTreeMap<String, String>,            // "#pk" -> "TenantId"
      pub values: BTreeMap<String, serde_json::Value>,// ":pk" -> "acme"
      pub limit: Option<i32>,                         // per-request page size; None = service default
      pub descending: bool,                           // reverse sort-key order
      pub exclusive_start: Option<Key>,
  }
  #[derive(Debug, Clone, Default, PartialEq)] pub struct ItemPage { pub items: Vec<Item>, pub next: Option<Key> }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]   pub enum Projection { All, KeysOnly, Include }   // Display: ALL | KEYS_ONLY | INCLUDE
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]   pub enum AttrType { S, N, B }                      // Display: S | N | B
  #[derive(Debug, Clone, Default, PartialEq)]
  pub struct IndexInfo { pub name: String, pub hash_key: String, pub sort_key: Option<String>, pub projection: Option<Projection> }
  #[derive(Debug, Clone, Default, PartialEq)]
  pub struct TableInfo {
      pub name: String, pub hash_key: String, pub sort_key: Option<String>,
      pub attr_types: BTreeMap<String, AttrType>,    // key attribute -> its declared scalar type
      pub gsis: Vec<IndexInfo>,
  }
  impl TableInfo { pub fn index(&self, name: &str) -> Option<&IndexInfo> }
  pub fn key_string(key: &Key) -> String;            // "TenantId=acme|Version=V0": names sorted, strings unquoted, other values compact JSON

  #[async_trait]
  pub trait DdbPort: Send + Sync {
      async fn list_tables(&self) -> Result<Vec<String>, SvcError>;                       // sorted
      async fn describe_table(&self, table: &str) -> Result<TableInfo, SvcError>;
      async fn get_item(&self, table: &str, key: &Key) -> Result<Item, SvcError>;         // missing item -> NotFound
      async fn query(&self, q: &QuerySpec) -> Result<ItemPage, SvcError>;
      async fn query_index(&self, table: &str, index: &str, q: &QuerySpec) -> Result<ItemPage, SvcError>;
  }
  ```

- [ ] **Step 1: Write the failing tests** (`svc/ddb/mod.rs` test module; `item(json!({...}))` helper unwraps the object):

```rust
#[test] fn key_string_sorts_names_and_unquotes_strings() {
    assert_eq!(key_string(&item(json!({"Version":"V0","TenantId":"acme"}))), "TenantId=acme|Version=V0");
    assert_eq!(key_string(&item(json!({"Id":42}))), "Id=42");
}
#[test] fn index_lookup_is_by_exact_name() {
    // TableInfo with gsis [TierIndex]; index("TierIndex").is_some(); index("tierindex").is_none()
}
#[test] fn projection_and_attribute_types_display_the_service_names() {
    assert_eq!(Projection::All.to_string(), "ALL");
    assert_eq!(Projection::KeysOnly.to_string(), "KEYS_ONLY");
    assert_eq!(Projection::Include.to_string(), "INCLUDE");
    assert_eq!([AttrType::S, AttrType::N, AttrType::B].map(|t| t.to_string()), ["S", "N", "B"]);
}
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core svc::ddb` → compile error (module missing).

- [ ] **Step 3: Implement** the types and trait in `svc/ddb/mod.rs`; add `pub mod ddb;` to `svc/mod.rs` and extend its `//!` doc. `key_string` renders non-string values with `jsonutil::compact`. `fake`, `contract`, `lazy` modules are declared by their own tasks.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core svc::ddb` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(svc): read-only DynamoDB port and types`

---

### Task 2: `FakeDdb` and the DDB contract suite

**Files:**
- Create: `crates/ss-core/src/svc/ddb/fake.rs`, `crates/ss-core/src/svc/ddb/contract.rs`
- Modify: `crates/ss-core/src/svc/ddb/mod.rs` (`#[cfg(any(test, feature = "test-support"))] mod fake; pub mod contract;` and `pub use fake::FakeDdb;` under the same cfg)

**Interfaces:**
- Consumes: Task 1 types and trait.
- Produces (`ss_core::svc::ddb`, test-support):
  ```rust
  #[derive(Clone, Default)] pub struct FakeDdb { /* Arc<Mutex<state>>, page_size: Option<usize> */ }
  impl FakeDdb {
      pub fn new() -> Self;
      pub fn paged(&self, page_size: usize) -> FakeDdb;                               // same data, every response holds at most page_size items
      pub fn create_table(&self, info: TableInfo);
      pub fn put(&self, table: &str, item: Item);                                     // panics if the table is missing; replaces the item with the same key; refreshes every index entry derived from it
      pub fn set_index_entry(&self, table: &str, index: &str, entry: Item);           // replaces (or adds) the index entry whose base key equals entry's; leaves the table item alone
      pub fn remove_index_entry(&self, table: &str, index: &str, key: &Key);
  }
  #[async_trait] impl DdbPort for FakeDdb
  // contract (test-support)
  pub fn table_info(name: &str) -> TableInfo;       // hash TenantId (S), sort Version (S), TierIndex: hash Tier, projection ALL
  pub fn seed_items() -> Vec<Item>;                 // acme V0 platinum rps 200; acme V1 gold 100; acme V2 gold 50 — each {TenantId, Version, Tier, limits:{rps, burst: rps*2}}
  pub async fn run(port: &dyn DdbPort, table: &str, missing_table: &str);   // `table` was created from table_info and seeded with seed_items
  ```
  Ordering: items sort by hash value then sort value (strings lexicographically, numbers numerically). `query` matches items whose attribute equals the bound value; `descending` reverses; the effective page size is the smaller of `limit` and the fake's page size; `next` is the base key (hash and sort attributes) of the last item returned when more remain. A start key the query did not return is an `Other` error, so a drain loop that restarts at zero cannot spin forever.

- [ ] **Step 1: Write the failing tests** (`fake.rs`; `tenants()` = `contract::table_info("tenants")` seeded with `contract::seed_items()`):

```rust
#[tokio::test] async fn get_item_and_missing_shapes() {
    // get_item(tenants, {TenantId:acme, Version:V0}) → Tier == "platinum"
    // missing key → is_not_found(); missing table → is_not_found(); list_tables() == ["tenants"]
}
#[tokio::test] async fn query_orders_by_sort_key_and_honours_descending_and_limit() {
    // pk=acme → [V0, V1, V2]; descending + limit 1 → [V2] with next == Some({TenantId:acme, Version:V2})
}
#[tokio::test] async fn paging_resumes_after_the_cursor_and_rejects_foreign_cursors() {
    // limit 2 → [V0, V1], next Some; resume → [V2], next None
    // exclusive_start {TenantId:acme, Version:V7} → Err that is not NotFound
}
#[tokio::test] async fn paged_fake_caps_every_response() { /* tenants().paged(1): three calls drain acme in order */ }
#[tokio::test] async fn index_entries_follow_puts_and_can_be_made_stale() {
    // query_index(tenants, TierIndex, Tier=gold) → V1 and V2
    // set_index_entry(TierIndex, V1 with rps 1) → the index serves rps 1 while get_item still serves 100
    // remove_index_entry(TierIndex, V2 key) → index returns V1 only; table still has V2
}
#[tokio::test] async fn keys_only_index_projects_only_key_attributes() {
    // a table whose index projection is KeysOnly: entries contain TenantId, Version, Tier and nothing else
}
#[tokio::test] async fn unsupported_key_conditions_are_rejected() { /* "#pk = :pk AND begins_with(#sk, :p)" → Err */ }
#[tokio::test] async fn fake_satisfies_the_contract() { contract::run(&tenants(), "tenants", "nope").await }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core svc::ddb::fake` → compile error.

- [ ] **Step 3: Implement `FakeDdb`** per the interface and Ruling 4. Parse a condition by splitting on the first `=`, resolving a `#alias` through `names` and a `:placeholder` through `values`.

- [ ] **Step 4: Implement `contract::run`.** It panics, naming the failed check and the observed value, on the first violation. Checks, in order: `list_tables` contains `table`; `describe_table` reports hash `TenantId`, sort `Version`, `attr_types["TenantId"] == AttrType::S`, and the `TierIndex` GSI (hash `Tier`, projection `Some(All)`); `get_item` returns `limits.rps` with `as_f64() == Some(200.0)` (numbers arrive as f64); missing item and `missing_table` are `is_not_found` (for `get_item`, `describe_table`, and `query`); `query` is ascending `V0,V1,V2`; descending with `limit: Some(1)` is `[V2]`; draining with `limit: Some(2)` over `exclusive_start` yields two pages (2 then 1 item) whose concatenation is `V0,V1,V2` and whose last `next` is `None`; `query_index` for `Tier = gold` returns exactly `V1` and `V2` (compare sorted by `Version`); `query_index` for an unused tier returns an empty page.

- [ ] **Step 5: Run the tests.** `cargo test -p ss-core svc::ddb` → PASS. `make check` → PASS.

- [ ] **Step 6: Commit.** `feat(svc): in-memory DynamoDB fake and contract suite`

---

### Task 3: `Lazy`, `Deps::ddb`, and the `Services` factory

**Files:**
- Create: `crates/ss-core/src/svc/ddb/lazy.rs`
- Modify: `crates/ss-core/src/svc/ddb/mod.rs` (`mod lazy; pub use lazy::Lazy;`), `crates/ss-core/src/ops/operation.rs`, `crates/ss-support/src/services.rs`, `crates/ss-support/src/run.rs`, `crates/ss-support/src/lib.rs`

**Interfaces:**
- Consumes: `DdbPort`, `SvcError`, `crate::ops::BoxFuture`, the S3 `Lazy` as the model, Task 2's `contract` and `FakeDdb`.
- Produces:
  ```rust
  pub struct Lazy { /* build fn + tokio::sync::OnceCell<Result<Arc<dyn DdbPort>, String>> */ }
  impl Lazy { pub fn new(build: impl Fn() -> BoxFuture<anyhow::Result<Arc<dyn DdbPort>>> + Send + Sync + 'static) -> Self }
  #[async_trait] impl DdbPort for Lazy          // every call resolves the port first; a failed build is remembered as its `{e:#}` message → SvcError::Other

  // Deps gains:  pub ddb: Option<Arc<dyn DdbPort>>   (Default: None)
  // services.rs (re-exported from lib.rs):
  pub type DdbFactory = Arc<dyn Fn(&Environment, &Vars) -> Arc<dyn DdbPort> + Send + Sync>;
  // Services gains `pub ddb: DdbFactory`; `Services::isolated` → a Lazy whose build fails "DynamoDB is not configured";
  // `Services::from_process` uses the same unconfigured factory until Task 4; `Services::with_ddb(self, port: Arc<dyn DdbPort>) -> Self`.
  ```

- [ ] **Step 1: Write the failing tests.** `Lazy` (builder counts calls in an `Arc<AtomicUsize>`), mirroring the S3 `Lazy` tests:

```rust
#[tokio::test] async fn lazy_does_not_build_until_used() { /* new(); count == 0 */ }
#[tokio::test] async fn lazy_builds_once_across_calls() { /* list_tables ×3 → count == 1 */ }
#[tokio::test] async fn lazy_build_failure_is_reported_on_every_call_and_built_once() {
    // build → Err(anyhow!("no credentials").context("load AWS config")); two calls each Err with to_string() == "load AWS config: no credentials"; count == 1
}
#[tokio::test] async fn lazy_satisfies_the_contract() { /* Lazy over the seeded FakeDdb → contract::run */ }
```
  `ss-support` (`services.rs` tests): `isolated_ddb_is_unconfigured` (the port's `list_tables` errors with a message containing `DynamoDB is not configured`) and `with_ddb_serves_the_given_port` (seeded `FakeDdb` → `list_tables` returns its tables). Run assertion for `Deps`: `Deps::default().ddb.is_none()`.

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core svc::ddb::lazy` and `cargo test -p ss-support services` → compile errors.

- [ ] **Step 3: Implement.** `Lazy` is the S3 `Lazy` with `DdbPort`'s five methods. Add `ddb` to `Deps` (and its `Default`). `App::build` takes the DDB factory beside the S3 one and sets `deps.ddb = Some((factory)(&env, vars))` — no cache wrapper (Ruling 5). Fix every `Deps { … }` literal the compiler flags with `ddb: None`.

- [ ] **Step 4: Run the tests.** `cargo test --workspace` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(cli): a lazily built DynamoDB port on every invocation`

---

### Task 4: `AwsDdb` adapter

**Files:**
- Create: `crates/ss-support/src/aws/ddb.rs`
- Modify: root `Cargo.toml` (workspace deps), `crates/ss-support/Cargo.toml`, `crates/ss-support/src/aws/mod.rs`, `crates/ss-support/src/aws/awsx.rs`, `crates/ss-support/src/services.rs`

**Interfaces:**
- Consumes: `ss_core::svc::ddb::*`, `ss_core::svc::SvcError`, `ss_core::jsonutil::normalize`, `awsx::{load, check_identity, check_credentials, check_account}`.
- Produces:
  ```rust
  // awsx.rs — the body both factories share:
  pub(crate) async fn sdk_config(env: &Environment, endpoint: Option<&str>, profile: &str) -> anyhow::Result<SdkConfig>;
  //   load(env.profile, env.region); with an endpoint → check_credentials only; otherwise check_identity + check_account.

  // aws/ddb.rs
  pub(crate) struct Options { pub endpoint: Option<String> }          // Default: None
  pub(crate) struct AwsDdb { client: aws_sdk_dynamodb::Client }
  impl AwsDdb { pub(crate) fn new(cfg: &SdkConfig, opts: Options) -> Self }   // endpoint → endpoint_url
  #[async_trait] impl DdbPort for AwsDdb
  fn is_not_found(code: Option<&str>) -> bool;                                  // "ResourceNotFoundException"
  fn item_from_av(av: HashMap<String, AttributeValue>) -> Result<Item, SvcError>;   // serde_dynamo; then jsonutil::normalize on every value
  fn key_to_av(key: &Key) -> Result<HashMap<String, AttributeValue>, SvcError>;
  fn table_info_from(table: &TableDescription) -> TableInfo;
  fn query_input(q: &QuerySpec, index: Option<&str>) -> Result<QueryInput, SvcError>;
  // aws/mod.rs
  pub(crate) fn ddb_factory() -> DdbFactory;
  ```

- [ ] **Step 1: Add dependencies.** Root `Cargo.toml` `[workspace.dependencies]`: `aws-sdk-dynamodb = "1"`, `serde_dynamo = { version = "4", features = ["aws-sdk-dynamodb+1"] }`; reference both from `ss-support/Cargo.toml`. If the resolved releases fail `make msrv`, pin older ones with `cargo update -p … --precise …`. Run `cargo build -p ss-support` → succeeds.

- [ ] **Step 2: Write the failing unit tests** (`aws/ddb.rs`):

```rust
#[test] fn classify_not_found() {
    assert!(is_not_found(Some("ResourceNotFoundException")));
    for code in [Some("AccessDeniedException"), Some("ValidationException"), None] { assert!(!is_not_found(code), "{code:?}"); }
}
#[test] fn items_unmarshal_to_plain_json_with_f64_numbers() {
    // AV map {S: "acme", N: "200", BOOL: true, NULL: true, L: [N "1", S "x"], M: {n: N "2.5"}}
    // → {"TenantId":"acme","rps":200.0,"on":true,"gone":null,"list":[1.0,"x"],"nested":{"n":2.5}}; assert rps.as_f64() == Some(200.0) and that Value::Number(200.0) (not an integer) is what diff sees
}
#[test] fn keys_marshal_numbers_as_numbers_and_strings_as_strings() {
    // {Id: 42.0, Name: "n"} → Id is AttributeValue::N("42"), Name is AttributeValue::S("n")
}
#[test] fn table_info_maps_key_schema_attribute_types_and_gsis() {
    // TableDescription with HASH TenantId, RANGE Version, attribute definitions S/S/S, GSI TierIndex (HASH Tier, projection ALL)
    // → TableInfo { hash_key "TenantId", sort_key Some("Version"), attr_types{TenantId/Version/Tier → AttrType::S}, gsis[TierIndex: hash Tier, sort None, Some(All)] }
}
#[test] fn query_input_maps_the_spec() {
    // spec: table "tenants", "#pk = :pk", names {#pk: TenantId}, values {:pk: "acme"}, limit 2, descending, start key
    // query_input(&spec, Some("TierIndex")) → table_name, index_name Some("TierIndex"), scan_index_forward Some(false), limit Some(2),
    //   key_condition_expression, expression_attribute_names/values, exclusive_start_key all set; with None → index_name None; names empty → None
}
```

- [ ] **Step 3: Run to verify they fail.** `cargo test -p ss-support aws::ddb` → compile errors.

- [ ] **Step 4: Implement `AwsDdb`.**
  - `list_tables`: paginate `ListTables`, sorted.
  - `describe_table`: `table_info_from` (a scalar type the SDK reports that is not `S`, `N`, or `B` is left out of `attr_types`); `Table` missing in the output → `SvcError::Other`.
  - `get_item`: `consistent_read(true)`; an empty `item` → `SvcError::not_found("{table} {key_string}")`.
  - `query` / `query_index`: `query_input`, one `Query` call, `next` from `last_evaluated_key` (an empty one is `None`); items through `item_from_av`.
  - Errors: a helper turns an `SdkError<E, HttpResponse>` (`E: ProvideErrorMetadata + Error + Send + Sync + 'static`) into `SvcError`: `is_not_found(err.code())` → `SvcError::not_found_from(what, err)`, otherwise `SvcError::Other(anyhow::Error::from(err))`. `what` is the table name (or `{table} {key}` for items).
  - `new`: `aws_sdk_dynamodb::config::Builder::from(cfg)` with `endpoint_url(endpoint)` when set.

- [ ] **Step 5: Share the factory body and add `ddb_factory`.** Move the load-and-guardrail branch of `s3_factory` into `awsx::sdk_config`; `s3_factory` and `ddb_factory` both call it. `ddb_factory()` reads `SS_SUPPORT_DDB_ENDPOINT`, returns an `Arc<Lazy>` whose build is `sdk_config(...)` then `AwsDdb::new`. `Services::from_process` uses `aws::ddb_factory()` in place of Task 3's placeholder. Existing S3 factory behavior is unchanged: the whole `aws` test module and `cargo test -p ss-support --test s3` stay green.

- [ ] **Step 6: Run the tests.** `cargo test -p ss-support` → PASS. `make check` → PASS. `make msrv` → PASS.

- [ ] **Step 7: Commit.** `feat(aws): DynamoDB adapter with not-found normalization`

---

### Task 5: dynalite contract run and CI job

**Files:**
- Modify: `crates/ss-support/src/aws/ddb.rs` (`#[cfg(test)] mod dynalite`), `Makefile`, `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: Task 2's `contract::{table_info, seed_items, run}`, Task 4's `AwsDdb`/`Options`.
- Produces: `make contract-ddb`; CI job `contract-ddb`.

- [ ] **Step 1: Write the env-gated tests** (`mod dynalite`). `SS_SUPPORT_DDB_TEST_ENDPOINT` unset → `eprintln!("skipping: SS_SUPPORT_DDB_TEST_ENDPOINT is not set")` and return. Credentials from `SS_SUPPORT_DDB_TEST_KEY` / `SS_SUPPORT_DDB_TEST_SECRET` (default `test`), region `us-east-1`, static credentials provider. With a raw `aws_sdk_dynamodb::Client` (endpoint set): create `ss-it-{unix nanos}` from `contract::table_info` (key schema, `TierIndex` GSI with `Projection ALL`, provisioned 5/5), wait until `ACTIVE`, put every `contract::seed_items()` item (via `serde_dynamo::to_item`). Then:
  - `contract_holds_against_dynalite`: `contract::run(&AwsDdb::new(&cfg, Options { endpoint }), &table, "ss-it-missing-{nanos}")`.
  - `numeric_keys_are_looked_up_as_numbers`: create a second table with hash `Id` of type `N`, put `{Id: 42, name: "n"}` raw, then `get_item(table, &{Id: 42.0})` finds it and `describe_table(...).attr_types["Id"] == AttrType::N`.

- [ ] **Step 2: Run to verify the unit run is green and the gated tests skip.** `cargo test -p ss-support aws::ddb -- --nocapture` → PASS (skip lines printed).

- [ ] **Step 3: Add the entry points.** `Makefile`:
  ```make
  ## contract-ddb: DynamoDB contract suite against dynalite (needs SS_SUPPORT_DDB_TEST_ENDPOINT, e.g. http://127.0.0.1:8000)
  .PHONY: contract-ddb
  contract-ddb:
  	@test -n "$(SS_SUPPORT_DDB_TEST_ENDPOINT)" || { echo "set SS_SUPPORT_DDB_TEST_ENDPOINT"; exit 2; }
  	cargo test --locked -p ss-support aws::ddb::dynalite -- --nocapture
  ```
  CI: a `contract-ddb` job on `ubuntu-latest` that checks out, installs the Rust toolchain and restores the cargo cache exactly as the `contract` job does, runs `actions/setup-node@v4` (Node 22), starts `npx --yes dynalite@3.2.2 --port 8000 &`, waits for `curl -s http://127.0.0.1:8000/` to answer (any HTTP status), then `make contract-ddb` with `SS_SUPPORT_DDB_TEST_ENDPOINT: http://127.0.0.1:8000`.

- [ ] **Step 4: Run against dynalite** (skip with a stated reason if Node or the network is unavailable, and report DONE_WITH_CONCERNS):
  ```bash
  npx --yes dynalite@3.2.2 --port 8000 &
  until curl -s http://127.0.0.1:8000/ >/dev/null; do sleep 1; done
  SS_SUPPORT_DDB_TEST_ENDPOINT=http://127.0.0.1:8000 make contract-ddb     # → both tests PASS
  ```
  Leave dynalite running for Task 11. `make check` → PASS.

- [ ] **Step 5: Commit.** `test(aws): DynamoDB contract run against dynalite`

---

### Task 6: DDB operation helpers and the suggestion chain

**Files:**
- Create: `crates/ss-core/src/ops/ddb/mod.rs`, `crates/ss-core/src/ops/ddb/fixture.rs`
- Modify: `crates/ss-core/src/ops/mod.rs` (`pub mod ddb;`)

**Interfaces:**
- Consumes: Task 1–2 types and `FakeDdb`, `SuggestFn`, `Choice`, `Deps`, `jsonutil`.
- Produces (`ops::ddb`, all `pub(crate)`):
  ```rust
  pub(crate) fn port(deps: &Deps) -> anyhow::Result<Arc<dyn DdbPort>>;   // error text: "DynamoDB is not configured"
  pub(crate) fn suggest_tables() -> SuggestFn;                             // every table name; none when DDB is not configured
  pub(crate) fn suggest_indexes() -> SuggestFn;                            // GSIs of the typed `table`; label "{name} — hash {hash}[, sort {sort}][ ({projection})]"
  pub(crate) fn parse_key(spec: &str, info: &TableInfo) -> anyhow::Result<Key>;
  pub(crate) fn coerce(val: serde_json::Value, attr_type: Option<AttrType>) -> serde_json::Value;
  pub(crate) fn version_num(label: &str) -> Option<u32>;
  pub(crate) fn item_version(item: &Item, attr: &str) -> String;           // string as is; number n → "V{n}"; otherwise ""
  pub(crate) fn sort_by_version(items: &mut [Item], attr: &str);           // stable; parseable labels ascending by number, then the rest alphabetically
  pub(crate) fn compact_preview(item: &Item, skip: &BTreeSet<String>, max: usize) -> String;
  pub(crate) mod fixture                                                   // #[cfg(test)]
  ```
  `fixture` (test-only): `deps(ddb: FakeDdb) -> Deps` (env `dev`, diff ignore `**.updatedAt`), `fake() -> FakeDdb` holding table `tenants` (hash `TenantId` S, sort `Version` S, attr types for `TenantId`/`Version`/`Tier` all `AttrType::S`, GSI `TierIndex` hash `Tier` projection `ALL`) with items `acme V0 platinum rps 200`, `acme V1 gold 100`, `acme V2 gold 50`, `beta V0 bronze 10`; every item is `{TenantId, Version, Tier, limits:{rps}, updatedAt:"2026-07-<Version>"}`. After seeding, `set_index_entry(tenants, TierIndex, acme V0 with rps 100)` — a stale projection.

- [ ] **Step 1: Write the failing tests** (`ops/ddb/mod.rs`):

```rust
#[test] fn version_num_accepts_only_v_and_digits() {
    for (label, want) in [("V0", Some(0)), ("v12", Some(12)), (" V3 ", Some(3)), ("V", None), ("V+1", None), ("v-3", None),
                          ("V1x", None), ("weird", None), ("V99999999999", None), ("", None)] {
        assert_eq!(version_num(label), want, "{label:?}");
    }
}
#[test] fn sort_by_version_puts_latest_first_and_strangers_last() {
    // versions [V10, V2, weird, V0, ""] → V0, V2, V10, "", weird? — alphabetical among unparseable: "" < "weird" → V0,V2,V10,"",weird
}
#[test] fn item_version_reads_strings_and_numbers() { /* {"Version":"V3"} → "V3"; {"Version":0} → "V0"; {"Version":2.5} → "V2.5"; missing → "" */ }
#[test] fn parse_key_accepts_json_and_pairs_and_coerces_to_the_declared_type() {
    // info attr_types {Id: AttrType::N, Sort: AttrType::S}: "Id=42,Sort=7" → Id 42.0 (number), Sort "7" (string)
    // r#"{"Id":"42","Sort":7}"# → same typed result
    // "TenantId=acme,Version=V0" and r#"{"TenantId":"acme","Version":"V0"}"# parse to equal keys on the tenants table
}
#[test] fn parse_key_splits_on_the_first_equals_only() { /* "Name=a=b" → {"Name":"a=b"} */ }
#[test] fn parse_key_rejects_blank_malformed_and_non_object_specs() {
    // "" and "   " → "empty key"; "nonsense" → error containing r#"key part "nonsense" is not name=value"#;
    // "{bad" → error starting "key is not valid JSON"; "[1]" → error
}
#[test] fn coerce_leaves_unknown_attributes_alone() { /* attr_type None keeps "7" a string */ }
#[test] fn compact_preview_sorts_skips_and_truncates_on_a_character_boundary() {
    // skip {TenantId}; {"b":1,"a":"x","TenantId":"t"} → `a="x" b=1`
    // max 6 on `a="é" b=1`-style multi-byte content never panics and ends with "…"; the result has at most max characters
}
#[tokio::test] async fn suggestions_list_tables_and_label_indexes() {
    // suggest_tables → ["tenants"]; suggest_indexes with table=tenants → one choice value "TierIndex", label "TierIndex — hash Tier (ALL)"
    // with no DDB in Deps, or table unset → empty
}
#[test] fn port_names_the_missing_configuration() { /* Deps::default() → Err containing "DynamoDB is not configured" */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core ops::ddb` → compile errors.

- [ ] **Step 3: Implement the helpers.** `coerce`: `AttrType::N` — a number stays; a string that parses as `f64` (after trimming) becomes a number via `Number::from_f64`, otherwise unchanged; `AttrType::S` — a number becomes its `jsonutil::fmt_number` text; anything else unchanged. `parse_key`: a spec starting with `{` is parsed as a JSON object (numbers normalized); otherwise split on `,`, each pair on its first `=`, both sides trimmed; every value is run through `coerce` with `info.attr_types.get(name).copied()`. `version_num`: trim, require a leading `V`/`v`, then one or more ASCII digits, parse as `u32` (overflow → `None`). `compact_preview`: `name=<compact JSON>` pairs joined by spaces in name order, then truncated to `max` characters with a final `…` (character-based, not byte-based).

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core ops::ddb` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): DynamoDB key, version, and suggestion helpers`

---

### Task 7: `ddb.item-compare`

**Files:**
- Create: `crates/ss-core/src/ops/ddb/item_compare.rs`
- Modify: `crates/ss-core/src/ops/ddb/mod.rs` (module + `pub use`), `crates/ss-core/src/ops/mod.rs` (register; extend `builtin_registers_every_operation`)

**Interfaces:**
- Consumes: Task 6 helpers and fixture; `diff::{json, Opts}`; `Runner::step`; `find_cause`.
- Produces: `pub struct ItemCompare;` (`#[derive(Clone, Copy, Debug, Default)]`).

Meta: id `ddb.item-compare`, name `Compare items`, summary `Semantic diff between two DynamoDB items`, category `DynamoDB`, class `Read`.
Params, in order:

| name | label | type | required | default | help | suggest |
|---|---|---|---|---|---|---|
| table | Table | String | yes | | | tables |
| key-a | Key A | String | yes | | `item key: {"TenantId":"acme","Version":"V0"} or TenantId=acme,Version=V0` | |
| key-b | Key B | String | yes | | `second item's key, same formats` | |
| table-b | Table B | String | no | | `compare across tables; defaults to Table` | tables |
| ignore | Ignore paths | Json | no | | `extra ignore globs, e.g. "**.updatedAt"` | |

- [ ] **Step 1: Write the failing tests** (`Runner::new(fixture::deps(fixture::fake())).execute(&ItemCompare, input)`):

```rust
#[tokio::test] async fn compares_two_keys_in_both_key_syntaxes() {
    // key-a "TenantId=acme,Version=V0", key-b r#"{"TenantId":"beta","Version":"V0"}"#
    // views[0] == View::summary([("A","tenants TenantId=acme|Version=V0"),("B","tenants TenantId=beta|Version=V0"),("Changes", <stats>)])
    // views[1] is View::Diff with left/right equal to those labels; no change path contains "updatedAt" (config ignore applied)
}
#[tokio::test] async fn identical_items_report_no_changes() { /* same key twice → delta.stats.total() == 0; Changes == "no changes" */ }
#[tokio::test] async fn compares_across_tables() {
    // add table "tenants-qa" (same schema) holding acme V0 with rps 999; table-b = tenants-qa → one change at limits.rps; B label starts "tenants-qa "
}
#[tokio::test] async fn ignore_param_adds_to_config_globs() { /* ignore "**.Tier" removes the Tier change */ }
#[tokio::test] async fn a_missing_item_fails_as_not_found() {
    // key-b TenantId=nope,Version=V0 → Err; find_cause::<SvcError>(&f.error).is_some_and(SvcError::is_not_found); format!("{:#}", f.error) starts with "fetch both items: "
}
#[tokio::test] async fn an_unknown_table_fails_as_not_found_in_the_describe_step() { /* format!("{:#}") starts "describe tables: " */ }
#[tokio::test] async fn a_malformed_key_fails_before_any_fetch() { /* key-a "nonsense" → error contains "is not name=value" */ }
#[tokio::test] async fn without_ddb_the_run_fails_clearly() { /* Deps::default() → chain contains "DynamoDB is not configured" */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core item_compare` → compile error.

- [ ] **Step 3: Implement.** Take `port(&r.deps)?`; clone what the steps need before calling `r.step`. Step `describe tables`: describe `table`; describe `table-b` only when it differs (else reuse). Parse both keys against their own table's info (a parse error returns before any fetch). Step `fetch both items`: `tokio::try_join!` the two `get_item` calls. Diff with `Opts { ignore: deps.diff.ignore ++ input.globs("ignore"), array_match: deps.diff.array_match.clone(), ..Default::default() }` over `Value::Object` of each item. Labels `"{table} {key_string(key)}"`. Views: summary `A`, `B`, `Changes` (`stats.to_string()`), then `View::Diff { left, right, delta }`. Register in `builtin()`; the builtin test expects, in `(category, id)` order, `["demo.hello", "demo.json-diff", "ddb.item-compare", "s3.object-inspect", "s3.prefix-audit", "s3.version-diff"]`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): ddb.item-compare diffs two items`

---

### Task 8: `ddb.item-history`

**Files:**
- Create: `crates/ss-core/src/ops/ddb/item_history.rs`
- Modify: `crates/ss-core/src/ops/ddb/mod.rs`, `crates/ss-core/src/ops/mod.rs`

**Interfaces:**
- Consumes: Task 6 helpers and fixture; `FakeDdb::paged`.
- Produces: `pub struct ItemHistory;` (`#[derive(Clone, Copy, Debug, Default)]`); `pub(crate) const DEFAULT_VERSION_ATTR: &str = "Version"`.

Meta: id `ddb.item-history`, name `Item history`, summary `List an item's versions (V0 = latest) and diff two of them`, category `DynamoDB`, class `Read`.
Params, in order:

| name | label | type | required | default | help | suggest |
|---|---|---|---|---|---|---|
| table | Table | String | yes | | | tables |
| pk | Partition key value | String | yes | | `value of the table's partition key, e.g. acme` | |
| a | Version A | String | no | `V1` | `older side of the diff` | item versions |
| b | Version B | String | no | `V0` | `newer side of the diff; V0 = latest` | item versions |
| version-attr | Version attribute | String | no | `Version` | `attribute holding the version label` | |
| limit | Max versions | Int | no | `50` | | |
| ignore | Ignore paths | Json | no | | `extra ignore globs, e.g. "**.updatedAt"` | |

The item-version suggestion lists the labels present under `pk`; `V0`/`v0` gets the label `"{label} — latest"`; empty without `table` and `pk`.

- [ ] **Step 1: Write the failing tests** (fixture; `pagedDeps(n, page)` = a table `tenants` holding `n` versions `V0..V{n-1}` of `acme`, `rps = 100 + i`, served `page` per response):

```rust
#[tokio::test] async fn defaults_to_v1_versus_v0() {
    // views[1] is View::Table { title: "Versions (V0 = latest)", cols: ["VERSION","","ITEM"], rows } with 3 rows; rows[0][..2] == ["V0","latest"]; rows[2][0] == "V2"
    // views[0] summary: Item "tenants TenantId=acme", Versions "3", Diff "V1 → V0: ~2"
    // the Diff view has exactly 2 changes at "Tier" and "limits.rps"; Version and updatedAt are not in it
}
#[tokio::test] async fn an_explicit_pair_diffs_just_those_versions() { /* a=V2 b=V1 → one change at limits.rps */ }
#[tokio::test] async fn an_unknown_version_lists_the_available_ones() { /* a=V9 → error contains `version "V9" not found` and "V0, V1, V2" */ }
#[tokio::test] async fn a_single_version_lists_without_diffing() {
    // pk=beta → Ok; summary has ("Note","only one version exists; nothing to diff"); no Diff view
}
#[tokio::test] async fn no_items_is_an_error_naming_the_key() { /* pk=zzz → error `no items found for TenantId="zzz" in tenants` */ }
#[tokio::test] async fn page_size_does_not_change_the_answer() {
    // 6 versions; a=V5 b=V0 limit 50; for page in [1,2,3,5,6,7,100] the views equal the unpaged baseline
}
#[tokio::test] async fn limit_caps_the_total() {
    // 50 versions served 3 at a time, limit 4 → the table has exactly 4 rows and the first is V0 (Ruling 9)
}
#[tokio::test] async fn a_custom_version_attribute_is_used_and_ignored() { /* items keyed with attribute "Rev"; version-attr=Rev → history and diff work; "Rev" absent from the diff */ }
#[tokio::test] async fn unparseable_labels_sort_last_without_failing() { /* a "weird" label row appears after V-rows; no panic */ }
```
  Suggest test: `suggest_item_versions` for `acme` → values `["V0","V1","V2"]`, first label `V0 — latest`.

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core item_history` → compile error.

- [ ] **Step 3: Implement.** Step `query versions`: describe the table, clamp `limit` to `1..=10000` (a missing or non-positive value is 50), coerce `pk` to the hash key's declared type, then drain `query` (`"#pk = :pk"`, names `{#pk: hash_key}`, `limit` as the page size) following `next` until it is `None` or `limit` items are held; truncate to `limit`; `sort_by_version`. No items → the error above. Rows: `[label, "latest" when version_num == Some(0) else "", compact_preview(item, skip{hash_key, version_attr, sort_key}, 80)]`. Diff only when both requested labels exist (≥2 items); the diff ignores `[version-attr] ++ deps.diff.ignore ++ globs`. Summary `Item` = `"{table} {hash_key}={pk}"`, `Versions`, then `Diff` = `"{a} → {b}: {stats}"` or the single-version `Note`. A missing requested label errors with `version "{label}" not found (have: V0, V1, …)` naming the first missing of `a` then `b`. Register in `builtin()` (the builtin test adds `ddb.item-history`).

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): ddb.item-history lists versions and diffs two of them`

---

### Task 9: `ddb.gsi-consistency`

**Files:**
- Create: `crates/ss-core/src/ops/ddb/gsi_consistency.rs`
- Modify: `crates/ss-core/src/ops/ddb/mod.rs`, `crates/ss-core/src/ops/mod.rs`

**Interfaces:**
- Consumes: Task 6 helpers and fixture (including its stale `acme V0` index entry); `FakeDdb::{set_index_entry, remove_index_entry, paged}`.
- Produces: `pub struct GsiConsistency;` (`#[derive(Clone, Copy, Debug, Default)]`); `pub(crate) const MAX_CANDIDATES: usize = 10_000`.

Meta: id `ddb.gsi-consistency`, name `GSI consistency check`, summary `Diff an item against its projection in a GSI`, category `DynamoDB`, class `Read`.
Params, in order:

| name | label | type | required | default | help | suggest |
|---|---|---|---|---|---|---|
| table | Table | String | yes | | | tables |
| index | GSI | String | yes | | | indexes |
| key | Item key | String | yes | | `base-table key: {"TenantId":"acme","Version":"V0"} or TenantId=acme,Version=V0` | |

- [ ] **Step 1: Write the failing tests:**

```rust
#[tokio::test] async fn a_stale_projection_is_reported_as_drift() {
    // table tenants, index TierIndex, key TenantId=acme,Version=V0
    // summary fields: Item "tenants TenantId=acme|Version=V0"; GSI "TierIndex (projection: ALL)"; Compared attributes "5 (those present in the projection)";
    //   Result starts "DRIFT — projection disagrees with the base item ("
    // views[1] is View::Diff { left: "base table", right: "GSI TierIndex", delta } with exactly one change at "limits.rps"
}
#[tokio::test] async fn a_matching_projection_is_consistent_and_has_no_diff_view() {
    // key beta V0 → Result "consistent — projection matches the base item"; views.len() == 1
}
#[tokio::test] async fn a_missing_index_entry_is_reported_as_missing() {
    // remove_index_entry(TierIndex, beta V0) → Result starts "MISSING — no GSI entry matches the base key (" and contains "candidates under Tier=bronze)"; views.len() == 1
}
#[tokio::test] async fn an_item_without_the_index_key_is_not_projected() {
    // put acme V3 with no Tier → Result "item has no \"Tier\" attribute — not projected into this GSI"; only Item and GSI and Result fields
}
#[tokio::test] async fn keys_only_projections_compare_only_what_they_carry() {
    // a table whose index is KeysOnly (entries hold only key attributes) → Result "consistent …" although the base item has more attributes; GSI field shows "(projection: KEYS_ONLY)"
}
#[tokio::test] async fn an_unknown_index_is_reported() { /* index "Nope" → error `table tenants has no GSI "Nope"` */ }
#[tokio::test] async fn a_missing_base_item_fails_as_not_found() { /* key acme V9 → find_cause SvcError is_not_found; "{:#}" starts "fetch base item: " */ }
#[tokio::test] async fn the_entry_is_found_on_a_later_page() {
    // 20 items share Tier "gold"; the matching entry is last; fake paged(3) → found, Result "consistent …"
}
#[tokio::test] async fn scan_stops_after_the_candidate_cap() {
    // MAX_CANDIDATES + 1 entries share one Tier, the target's entry is removed, fake paged(1000) →
    // Result starts "INCONCLUSIVE — no matching GSI entry among the first 10000 candidates under Tier="
}
#[tokio::test] async fn without_ddb_the_run_fails_clearly() { /* Deps::default() */ }
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ss-core gsi_consistency` → compile error.

- [ ] **Step 3: Implement.** Steps `describe table`, `fetch base item`, `query GSI projection`. Result strings are exact copy: no hash attribute → `item has no "{hash_key}" attribute — not projected into this GSI`; not found within the scan → `MISSING — no GSI entry matches the base key ({n} candidates under {hash_key}={value})`; capped → `INCONCLUSIVE — no matching GSI entry among the first 10000 candidates under {hash_key}={value}; the entry may exist further on`; found → compare only attributes present in the projection (`base` restricted to the projected entry's attribute names) using `deps.diff.ignore` and `array_match`, with `Compared attributes` = `"{n} (those present in the projection)"` and `Result` = `consistent — projection matches the base item` or `DRIFT — projection disagrees with the base item ({stats})` (plus the `View::Diff` only on drift). `GSI` field is `"{index} (projection: {projection or "unknown"})"`. The scan queries `query_index(table, index, spec)` with `"#h = :h"` / `names {#h: hash_key}` / `values {:h: base[hash_key]}`, follows `next`, counts candidates, matches an entry whose base-key attributes all equal the key's (compare by `key_string` of the base-key subset), and stops at the match, at `next == None`, or when the count reaches `MAX_CANDIDATES`. Register in `builtin()`; the builtin test final order is `["demo.hello", "demo.json-diff", "ddb.gsi-consistency", "ddb.item-compare", "ddb.item-history", "s3.object-inspect", "s3.prefix-audit", "s3.version-diff"]`.

- [ ] **Step 4: Run the tests.** `cargo test -p ss-core` → PASS. `make check` → PASS.

- [ ] **Step 5: Commit.** `feat(ops): ddb.gsi-consistency checks an item against its index`

---

### Task 10: Read-only guard tests

**Files:**
- Modify: `crates/ss-core/src/svc/ddb/mod.rs` (test module), `crates/ss-support/src/aws/ddb.rs` (test module)
- Create: `crates/ss-support/tests/ddb_guards.rs`

**Interfaces:**
- Consumes: the finished port, adapter, and operations; `ss_core::ops::builtin`, `ss_core::guard::Class`.
- Produces: tests only. No product code; no write path of any kind.

- [ ] **Step 1: Write the guard tests.**

```rust
// ss-core svc/ddb/mod.rs
#[test] fn the_port_trait_has_no_write_method() {
    // a `struct ReadOnly;` implementing DdbPort with exactly the five read methods compiles (the trait has no other required method)
    // and the production half of this file (everything before `#[cfg(test)]`) contains none of
    // put_item, update_item, delete_item, batch_write, transact_write — the needles are assembled with concat!/format! so this test does not match itself
}
// ss-support aws/ddb.rs
#[test] fn the_adapter_never_calls_a_write_api() { /* same needle scan over the production half of aws/ddb.rs */ }
// ss-support tests/ddb_guards.rs  (isolated services, fake DDB, empty stdin, default config: no environment allows writes)
#[test] fn every_ddb_operation_is_a_read() {
    // builtin().all() filtered to ids starting "ddb." → exactly ["ddb.gsi-consistency","ddb.item-compare","ddb.item-history"], each meta().class == Class::Read
}
#[tokio::test] async fn ddb_operations_run_where_writes_are_refused() {
    // for each op with valid arguments against a seeded FakeDdb: ss_with(...) → Exit::Ok, stderr empty, stdout non-empty;
    // the audit log under the isolated root has one line per run, each containing "\"outcome\":\"ok\""
    // (an op that asked to confirm a change would read the empty stdin and exit Declined instead)
}
```

- [ ] **Step 2: Run them.** `cargo test -p ss-core svc::ddb::tests::the_port_trait` and `cargo test -p ss-support --test ddb_guards` → PASS (they pin existing behavior; to prove each can fail, temporarily add `fn put_item(&self)` to the trait source or a `Class::Mutate` to one op, see the matching test fail, and revert).

- [ ] **Step 3: Run the suite.** `make check` → PASS.

- [ ] **Step 4: Commit.** `test(ddb): guards that keep the DynamoDB slice read-only`

---

### Task 11: End-to-end tests, README, CHANGELOG

**Files:**
- Create: `crates/ss-support/tests/ddb.rs`
- Modify: `README.md`, `CHANGELOG.md`, `docs/README.md` (only if it lists what exists)

**Interfaces:**
- Consumes: `Services::{isolated, with_ddb}`, `FakeDdb`, `contract::{table_info, seed_items}`, the `ss_with` helper shape from `tests/s3.rs`.

- [ ] **Step 1: Write the failing e2e tests** (`tests/ddb.rs`; a `fake()` seeds table `tenants` from `contract::table_info("tenants")` and `contract::seed_items()`):

```rust
#[tokio::test] async fn ops_list_shows_the_ddb_operations() { /* out contains ddb.item-compare, ddb.item-history, ddb.gsi-consistency */ }
#[tokio::test] async fn demo_and_ops_list_run_without_ddb() {
    // Services::isolated (DDB unconfigured): `ops list` and `run demo.hello --name x` exit 0 with empty stderr
}
#[tokio::test] async fn item_history_through_the_cli_as_json() {
    // with_ddb(fake): run ddb.item-history --table tenants --pk acme -o json → Exit::Ok; the first view's Item field is "tenants TenantId=acme"
    // (read render::json for the exact JSON shape before writing the assertion)
}
#[tokio::test] async fn item_compare_through_the_cli_as_text() { /* --key-a TenantId=acme,Version=V0 --key-b TenantId=acme,Version=V1 → Exit::Ok; stdout contains "limits.rps" */ }
#[tokio::test] async fn gsi_consistency_reports_a_stale_index() {
    // fake with set_index_entry(TierIndex, acme V0 with rps 1) → stdout contains "DRIFT"
}
#[tokio::test] async fn a_missing_item_exits_1_with_one_line() {
    // --key-b TenantId=nope,Version=V0 → Exit::Failed; stderr is one line starting "ss-support run ddb.item-compare: " containing "not found"
}
#[tokio::test] async fn an_unconfigured_ddb_exits_1() { /* isolated: run ddb.item-history --table t --pk p → Exit::Failed; stderr contains "DynamoDB is not configured" */ }
#[tokio::test] async fn a_missing_required_parameter_exits_2() { /* run ddb.item-compare --table tenants → Exit::Usage */ }
```

- [ ] **Step 2: Run to verify they fail or pass as expected.** `cargo test -p ss-support --test ddb` — everything the earlier tasks built should already satisfy these; fix only genuine gaps (a failing test here is a wiring bug, not a reason to weaken the test).

- [ ] **Step 3: CHANGELOG.** Under `## [Unreleased]` add `### Added`:
  - Read-only DynamoDB operations `ddb.item-compare`, `ddb.item-history`, and `ddb.gsi-consistency`, with table, index, and version suggestions.
  - DynamoDB access through the selected environment's profile and region, with the same account guardrail and expired-session message as S3.
  - `SS_SUPPORT_DDB_ENDPOINT` targets a DynamoDB-compatible endpoint such as dynalite (no identity check).
  - `make contract-ddb` runs the DynamoDB contract suite against dynalite.

- [ ] **Step 4: README.** Add a `## DynamoDB` section beside `## S3`: the three operations (one example each, including both key syntaxes), the `V0` = latest version convention and the `Version` attribute (`--version-attr` to change it), that every operation is read-only, that item fetches and base-table queries use strongly consistent reads, and that GSI queries are eventually consistent, how the environment, `--profile`, and `--region` pick the identity (same `account_id` guardrail as S3), `SS_SUPPORT_DDB_ENDPOINT`, and a dynalite quick start (`npx dynalite --port 8000`, seed with the AWS CLI `aws --endpoint-url http://127.0.0.1:8000 dynamodb create-table …`, then `SS_SUPPORT_REGION=us-east-1 SS_SUPPORT_DDB_ENDPOINT=http://127.0.0.1:8000 ss-support run ddb.item-history --table tenants --pk acme`). Update the README's operations list and environment-variable list and any flag table so they match behavior. No Go, porting, or plan references.

- [ ] **Step 5: Verify.** `make check` → PASS; `make msrv` → PASS; `cargo build --release` → succeeds.

- [ ] **Step 6: Smoke the release binary against dynalite** (skip with a stated reason if Task 5's dynalite is not running). Create and seed a table with the AWS CLI (credentials `test`/`test`, region `us-east-1`), then:
  ```bash
  target/release/ss-support run ddb.item-history --table <t> --pk acme                      # exit 0, 3 rows, diff V1 → V0
  target/release/ss-support run ddb.item-compare --table <t> --key-a TenantId=acme,Version=V0 --key-b TenantId=acme,Version=V1   # exit 0
  target/release/ss-support run ddb.gsi-consistency --table <t> --index TierIndex --key TenantId=acme,Version=V0   # exit 0, "consistent"
  target/release/ss-support run ddb.item-compare --table <t> --key-a TenantId=acme,Version=V0 --key-b TenantId=nope,Version=V0; echo $?   # one "not found" line, 1
  ```
  (each with `SS_SUPPORT_DDB_ENDPOINT=http://127.0.0.1:8000 AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test SS_SUPPORT_REGION=us-east-1`). Then stop dynalite. Record the outputs in the task report.

- [ ] **Step 7: Commit.** `feat(ddb): end-to-end tests and documentation for the DynamoDB slice`

---

## Task index

| # | Title | Main files | Depends on |
|---|---|---|---|
| 1 | DDB port and types | `ss-core/src/svc/ddb/mod.rs` | — |
| 2 | `FakeDdb` + contract suite | `svc/ddb/{fake,contract}.rs` | 1 |
| 3 | `Lazy`, `Deps::ddb`, `Services` factory | `svc/ddb/lazy.rs`, `ops/operation.rs`, `ss-support/src/{services,run}.rs` | 1, 2 |
| 4 | `AwsDdb` adapter | `ss-support/src/aws/{ddb,awsx,mod}.rs`, manifests | 3 |
| 5 | dynalite contract run + CI | `aws/ddb.rs`, `Makefile`, CI | 2, 4 |
| 6 | Op helpers + suggestion chain | `ops/ddb/{mod,fixture}.rs` | 2 |
| 7 | `ddb.item-compare` | `ops/ddb/item_compare.rs` | 6 |
| 8 | `ddb.item-history` | `ops/ddb/item_history.rs` | 6 |
| 9 | `ddb.gsi-consistency` | `ops/ddb/gsi_consistency.rs` | 6 |
| 10 | Read-only guard tests | `svc/ddb/mod.rs`, `aws/ddb.rs`, `tests/ddb_guards.rs` | 3, 4, 7–9 |
| 11 | e2e, README, CHANGELOG | `tests/ddb.rs`, `README.md`, `CHANGELOG.md` | 3–10 |
