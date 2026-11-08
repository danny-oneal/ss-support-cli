# Phase 2 — ops core, safety model, headless CLI — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the Rust `ss` binary a working headless CLI (`ss ops list|describe`, `ss run <op>`, `ss presets list|show|delete`) built on a ported operations layer (Operation / Registry / Runner / View), the full safety model (env allowlist, confirm-at-first-mutation, `--yes`, escalation, dry-run, audit), config, presets, cache store, fuzzy matcher, renderers, and the two demo operations.

**Architecture:** Everything except argument parsing and terminal I/O lives in `ss-core` (no AWS deps). The `ss` crate holds a clap derive tree augmented at runtime with one subcommand per registered operation, a single pipeline (`run.rs`) that turns every failure into one stderr line and an `Exit` code, and an `Io` bundle so end-to-end tests run in-process with captured output and scripted stdin. The Runner returns a typed `RunFailure` (kind + error + views) instead of `anyhow::Result`, so the CLI can map failures to exit codes without string matching.

**Tech Stack:** Rust 2024 · tokio + tokio-util (`CancellationToken`) · async-trait · anyhow / thiserror · serde / serde_json / serde_yaml_ng · chrono · sha2 · tempfile · clap 4 (derive + builder).

**Spec:** `docs/superpowers/plans/2026-11-07-rust-port-master-plan.md` — §Phase 2 (pinned interfaces, tests to port, exit criteria), §2 Global constraints, §2a CLI style guide, §3 Review focus 1, 2, 5. Behavior reference: Go sources `internal/{ops,guard,audit,cache,config,preset,fuzzy,render,report}`, `internal/ops/demoops`, `cmd/{root,run,ops,presets}.go` (frozen at tag `go-freeze`).

## Global Constraints

- Work under `rust/`; run cargo from `rust/`. Edition 2024, MSRV 1.88; `make rust-check` (fmt + clippy `-D warnings` + tests) green at every commit.
- No `unwrap()` outside tests; `expect()` only for invariants. `missing_docs` applies to every `pub` item in `ss-core`.
- **Comments, doc comments, test names, and manifests under `rust/` never mention Go/Golang, the plan, phases, reviews, or redliner.** They describe behavior only. Before each commit: `grep -rniE '\bgo\b|golang|\bplan\b|\bphases?\b|§|\breview|redliner' rust/crates` must print nothing.
- One `thiserror` enum per `ss-core` module, lowercase messages, the wrapped error in `#[source]` and **not** repeated in the message (the CLI prints chains with `{e:#}`).
- Env vars and paths identical to the original tool: `SS_CONFIG`, `SS_PRESETS`, `SS_ENV`, `SS_PROFILE`, `SS_REGION`, `XDG_CONFIG_HOME`/`~/.config/ss/{config,presets}.yaml`, `XDG_STATE_HOME`/`~/.local/state/ss/audit.log`, `XDG_CACHE_HOME`/`~/.cache/ss`. An env var set to `""` counts as unset.
- Library code never reads process env directly: lookups go through a `Vars` snapshot (Task 1) so tests are hermetic.
- Operation IDs, names, summaries, categories, param names, defaults, and help strings copied verbatim from the original operations.
- Audit JSONL field names and order: `time,user,env,account,op,params,outcome,duration_ms,error` (`account`, `params`, `error` omitted when empty).
- `-o json` shape: `{"views":[{"type":"summary","fields":{…}}, …]}`, printed with `jsonutil::pretty` (never `serde_json::to_string_pretty`, which prints `8080.0`) plus a trailing newline.
- Exit codes: `0` ok (incl. dry-run) · `1` step error, guard denied, declined · `2` usage/config error (bad flag value, unknown op, unknown `-o`, missing required param, unknown preset, unparsable config/presets) · `130` interrupted. clap usage text only for parse errors.
- Results to stdout; diagnostics (progress, prompts, `exported to …`, `saved preset …`, errors) to stderr. Every failure is exactly one stderr line: `<prefix>: {e:#}` where prefix is `ss run <op.id>`, `ss ops describe`, `ss presets show`, etc.
- Conventional Commits with scope (`feat(ops): …`, `feat(cli): …`); every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

**Resolved spec conflicts** (decided here; recorded in master plan §4a by Task 16):
- Review Focus 5 says Ctrl-C exits `1`; §2a's exit table says `130`. **130 wins** (more specific, binding style guide). The audit record still says outcome `error`.
- Pinned `Runner::execute -> anyhow::Result<OpResult>` becomes `Result<OpResult, RunFailure>` (declined runs carry views *and* an error; the CLI needs the failure kind for exit codes).
- Pinned `RunnerError::NoConfirmer` becomes `GuardError::NoConfirmer` so "no confirmer" is a decline (fail closed, outcome `declined`), matching the original.
- `config::resolve` takes the env value instead of an env key; `File::env` takes `&str` (empty = no override); `Input::duration` is dropped (no operation reads it; `ParamType::Duration` stays).
- `Confirmer` is a fifth `async_trait` trait (it is dyn-dispatched).
- Bare `ss` prints help and exits 0 until the TUI exists. `ss tui` and `ss completion` are not added in this phase.

## Review Focus

1. **`--yes` on a run that calls `escalate()`** still consults the confirmer with class `destructive`, and with no confirmer fails closed (outcome `declined`, nothing mutated). → Task 10 `yes_never_covers_an_escalated_run`.
2. **Audit write failure** (log path under a regular file) leaves the run's result, views, and failure kind unchanged. → Task 9 `audit_write_failure_does_not_change_the_outcome`.
3. **Ctrl-C mid-step** ends the step promptly with `<step>: cancelled`, records audit outcome `error`, and the CLI exits 130. → Task 9 `cancellation_mid_step_is_prompt_and_audited`, Task 16 `ctrl_c_mid_run_exits_130`.
4. **Lenient input** — `int("abc") == 0`, `int(" 7 ") == 7`, `bool("TRUE")`, a glob value starting with `[` that is not a JSON array falls back to CSV. Operations rely on it (`repeat <= 0 → 1`). → Task 7 `typed_accessors_are_lenient`.
5. **A typo in `config.yaml`** fails fast naming the key (`deny_unknown_fields`), exit 2, before anything runs; an empty or comments-only file is a valid empty config. → Task 1 `unknown_key_is_rejected_with_its_name`, `empty_and_comment_only_files_are_empty_config`; Task 15 `config_typo_exits_2_naming_the_key`.

---

## File Structure

```
rust/crates/ss-core/src/
  lib.rs                 # + pub mod audit, cache, config, fuzzy, guard, ops, preset, render, report, vars
  vars.rs                # Vars snapshot, non-empty lookup, XDG base dirs
  config/mod.rs          # File, EnvSpec, DiffDefaults, Environment, load, resolve, default_path
  config/trace.rs        # TraceConfig, TraceDomain, TraceError
  guard.rs               # Class, Request, Guard, AllowAll, EnvAllowlist, GuardError
  audit.rs               # Entry, Outcome, Logger, Nop, FileLogger, default_path
  cache.rs               # Store, Nop, Dir, default_dir
  preset.rs              # Store, PresetError, default_path
  fuzzy.rs               # score, matches, rank
  ops/mod.rs             # re-exports, builtin()
  ops/input.rs           # Input
  ops/view.rs            # View, Node, OpResult
  ops/operation.rs       # Meta, ParamType, Param, Choice, SuggestFn, BoxFuture, Deps, Operation
  ops/registry.rs        # Registry, RegistryError
  ops/confirm.rs         # Confirmer, ConfirmSpec
  ops/runner.rs          # Runner, Progress, StepStatus, RunnerError, RunFailure, FailureKind, find_cause, with_defaults
  ops/demo/{mod,hello,json_diff}.rs
  render.rs              # text, json
  report.rs              # markdown
rust/crates/ss-core/tests/runner.rs      # Runner scenarios (Tasks 9–10)
rust/crates/ss/src/
  lib.rs                 # Exit + re-exports
  main.rs                # shim
  cli.rs                 # derive tree, command(), parse_from(), pure helpers
  cli/dynamic.rs         # op_command, collect_input
  io.rs                  # Io, Sink, Source, Captured
  services.rs            # Services
  confirm.rs             # StdinConfirmer
  run.rs                 # pipeline, Failure → Exit, interrupt handling
  cmd/{mod,ops,presets,run_op}.rs
rust/crates/ss/tests/e2e.rs
```

---

### Task 1: Vars snapshot and config file

**Files:**
- Create: `rust/crates/ss-core/src/vars.rs`, `rust/crates/ss-core/src/config/mod.rs`
- Modify: `rust/crates/ss-core/src/lib.rs`, `rust/crates/ss-core/Cargo.toml` (add `serde_yaml_ng.workspace = true`; dev-dep `tempfile.workspace = true`)

**Interfaces:**
- Produces:
  - `vars::Vars = BTreeMap<String, String>`; `vars::get(v: &Vars, key: &str) -> Option<&str>` (None when absent or empty); `vars::process() -> Vars` (from `std::env::vars_os()`, skipping non-UTF-8 entries); `vars::base_dir(v: &Vars, xdg_key: &str, home_rel: &str) -> PathBuf` (`$xdg_key`, else `$HOME/<home_rel>`, else `./<home_rel>`).
  - `config::{File, EnvSpec, DiffDefaults, Environment, ConfigError, load, resolve, default_path}` and `File::env(&self, name: &str, profile: &str, region: &str) -> Environment`.
  - `Environment { name, profile, region, account_id, kube_context: String, allowed: bool }` derives `Debug, Clone, Default, PartialEq`.
  - `DiffDefaults { ignore: Vec<String>, array_match: BTreeMap<String,String> }`.
  - `File { default_env: String, environments: BTreeMap<String, EnvSpec>, diff: DiffDefaults, trace: TraceConfig }` — `trace` is added in Task 2; until then omit the field.
  - YAML keys: `default_env, environments, diff{ignore, array_match}, trace`; `EnvSpec{profile, region, account_id, kube_context}`. Every struct `#[serde(deny_unknown_fields, default)]`.

- [ ] **Step 1: Write the failing tests** (in `config/mod.rs` `#[cfg(test)]`)

```rust
#[test] fn resolve_precedence() {
    assert_eq!(resolve(Some("flag"), Some("env"), "file", "def"), "flag");
    assert_eq!(resolve(None, Some("env"), "file", "def"), "env");
    assert_eq!(resolve(Some(""), Some(""), "file", "def"), "file"); // empty = unset
    assert_eq!(resolve(None, None, "", "def"), "def");
}
#[test] fn default_path_honors_ss_config_then_xdg_then_home() {
    // {SS_CONFIG:/x/c.yaml} → /x/c.yaml; {XDG_CONFIG_HOME:/xdg} → /xdg/ss/config.yaml;
    // {HOME:/h} → /h/.config/ss/config.yaml; {SS_CONFIG:"", HOME:/h} → /h/.config/ss/config.yaml
}
#[test] fn missing_file_is_empty_config()                 // load(tmp/nope.yaml) == Ok(File::default())
#[test] fn empty_and_comment_only_files_are_empty_config() // "" and "# just a comment\n" → File::default()
#[test] fn load_and_env_resolution() {
    // body (verbatim):
    // default_env: dev
    // environments:
    //   dev: {profile: acme-dev, region: us-west-2, account_id: "111122223333", kube_context: dev-cluster}
    // diff:
    //   ignore: ["**.updatedAt"]
    //   array_match: {"$.rules[*]": id}
    // assert default_env == "dev", diff.ignore == ["**.updatedAt"], array_match["$.rules[*]"] == "id"
    // env("dev","","") == Environment{name:"dev",profile:"acme-dev",region:"us-west-2",account_id:"111122223333",kube_context:"dev-cluster",allowed:true}
    // env("dev","other-profile","eu-west-1") → profile/region overridden, still allowed
    // env("prod","","").allowed == false; env("","","").allowed == false
}
#[test] fn unknown_key_is_rejected_with_its_name() {
    // body "envs: {}\n" → Err(ConfigError::Parse{..});
    // format!("{:#}", anyhow::Error::from(err)) starts with "parse config " and contains "unknown field `envs`"
}
```
`vars.rs` tests: `get` treats `""` as absent; `base_dir` with `{XDG_STATE_HOME:/s}` → `/s`, with `{HOME:/h}` and `home_rel=".local/state"` → `/h/.local/state`, with `{}` → `./.local/state`.

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core config:: vars::` → FAIL (modules missing).

- [ ] **Step 3: Implement**
  - `pub fn resolve(flag: Option<&str>, env_val: Option<&str>, file_val: &str, default: &str) -> String` — first non-empty wins.
  - `pub fn default_path(v: &Vars) -> PathBuf` — `SS_CONFIG`, else `base_dir(v, "XDG_CONFIG_HOME", ".config")/ss/config.yaml`.
  - `pub fn load(path: &Path) -> Result<File, ConfigError>` — `NotFound` → default; deserialize as `Option<File>` so empty/comment-only docs become default.
  - `ConfigError::{Read { path: PathBuf, #[source] source: io::Error }, Parse { path: PathBuf, #[source] source: serde_yaml_ng::Error }}` with messages `read config {path}` / `parse config {path}` (`path.display()`).
  - `File::env`: `allowed = name non-empty && environments contains name`; spec fields copied when present; non-empty `profile`/`region` override.

- [ ] **Step 4: Run to verify pass** — `cargo test -p ss-core config:: vars::` → PASS.

- [ ] **Step 5: Commit** — `feat(config): env-var snapshot, config file, precedence resolution`

---

### Task 2: Trace domains

**Files:**
- Create: `rust/crates/ss-core/src/config/trace.rs`
- Modify: `rust/crates/ss-core/src/config/mod.rs` (add `pub mod trace;`, `pub use trace::{TraceConfig, TraceDomain, TraceError};`, field `File.trace: TraceConfig`)

**Interfaces:**
- Produces: `TraceConfig { domains: BTreeMap<String, TraceDomain> }`; `TraceDomain { table, bucket, hash_attr, sort_attr, sort_format, version_attr, s3_key_attr, s3_version_attr, namespace: String, tables, buckets: BTreeMap<String,String>, labels: Vec<String> }` (YAML keys snake_case = field names), both `#[serde(deny_unknown_fields, default)]`, `Clone, Debug, Default, PartialEq`.
  `TraceDomain::with_defaults(self) -> Self`, `resolve_table(&self, env: &str) -> String`, `resolve_bucket(&self, env: &str) -> String`, `build_sort(&self, vars: &BTreeMap<String,String>) -> Result<String, TraceError>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn defaults_fill_the_common_shape() {
    // TraceDomain::default().with_defaults(): hash_attr "hash", version_attr "version",
    // s3_key_attr "key", s3_version_attr "versionId", sort_format "" (no sort_attr)
    // with sort_attr "version" → sort_format "{version}"; explicit values are kept
}
#[test] fn table_and_bucket_resolve_override_then_template() {
    // table "ss-menu-{env}" → resolve_table("dev") == "ss-menu-dev"
    // tables {"prod": "menu-prod-v2"} → resolve_table("prod") == "menu-prod-v2"; same for bucket/buckets
}
#[test] fn build_sort_renders_and_names_missing_vars() {
    // sort_format "" → Ok("")
    // "{locale}#{channel}" with {locale:en-US, channel:web} → Ok("en-US#web")
    // "{locale}#{channel}" with {locale:en-US} → Err; to_string() ==
    //   r#"sort key "{locale}#{channel}" needs vars: channel (pass --vars channel=…)"#
    // "{a}{b}" with {} → message ends "needs vars: a, b (pass --vars a=… b=…)"
}
#[test] fn trace_section_parses_from_config() // YAML `trace: {domains: {menu: {table: "ss-menu-{env}", hash_attr: storeNumber, sort_attr: sk, sort_format: "{locale}#{channel}", labels: [storeNumber]}}}` loads
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core config::trace` → FAIL.

- [ ] **Step 3: Implement.** Placeholders are `{name}` where `name` contains no `{`/`}`; scan by hand (no regex dep); unresolved placeholders stay in the output and are collected in order (duplicates repeat). `TraceError::MissingVars { format: String, missing: Vec<String> }` with message `sort key {format:?} needs vars: {joined ", "} (pass --vars {joined "=… "}=…)`.

- [ ] **Step 4: Run to verify pass** — `cargo test -p ss-core config::` → PASS.

- [ ] **Step 5: Commit** — `feat(config): trace domain locators`

---

### Task 3: Guard and audit log

**Files:**
- Create: `rust/crates/ss-core/src/guard.rs`, `rust/crates/ss-core/src/audit.rs`
- Modify: `rust/Cargo.toml` (`chrono = { version = "0.4", default-features = false, features = ["std", "clock", "serde"] }` in `[workspace.dependencies]`), `rust/crates/ss-core/Cargo.toml` (chrono)

**Interfaces:**
- Produces:
  - `guard::Class { Read, Mutate, Destructive }` — `Copy, Eq, Debug`; `Display`/`as_str`: `read|mutate|destructive`.
  - `guard::Request { op_id: String, class: Class, resource: String }`.
  - `trait Guard: Send + Sync { fn authorize(&self, req: &Request) -> Result<(), GuardError>; }`; `AllowAll`; `EnvAllowlist { name: String, allowed: bool }`.
  - `GuardError::{EnvNotAllowed { op: String, env: String }, NotConfirmed { op: String }, NoConfirmer { op: String }}`, messages: `environment {env:?} is not in the configured allowlist`, `operation not confirmed`, `no confirmer available: operation not confirmed`. `GuardError::is_decline(&self) -> bool` (true for the latter two).
  - `audit::Outcome { Ok, Error, Denied, DryRun, Declined }` serialized `ok|error|denied|dry-run|declined`.
  - `audit::Entry { time: DateTime<Utc>, user: String, env: String, account: String /*skip if empty*/, #[serde(rename="op")] op_id: String, params: BTreeMap<String,String> /*skip if empty*/, outcome: Outcome, duration_ms: i64, error: Option<String> /*skip if None*/ }` — all fields `pub`; `Serialize, Deserialize, Clone, Debug`.
  - `trait Logger: Send + Sync { fn record(&self, e: &Entry) -> io::Result<()>; }`; `Nop`; `FileLogger::new(path: impl Into<PathBuf>)`; `audit::default_path(v: &Vars) -> PathBuf` (`base_dir(v,"XDG_STATE_HOME",".local/state")/ss/audit.log`).

- [ ] **Step 1: Write the failing tests**

```rust
// guard.rs
#[test] fn read_always_passes()            // EnvAllowlist{name:"prod",allowed:false}.authorize(Read) == Ok
#[test] fn env_allowlist_is_a_hard_stop()  // Mutate and Destructive → Err(EnvNotAllowed{op:"x",env:"prod"});
                                           // to_string() == r#"environment "prod" is not in the configured allowlist"#
#[test] fn allowlisted_env_passes_every_class()
#[test] fn class_names_are_stable()        // "read","mutate","destructive"
// audit.rs
#[test] fn file_logger_appends_jsonl_with_stable_field_order() {
    // record two entries into tmp/a/b/audit.log (parents created);
    // file has 2 lines; line 1 starts with r#"{"time":""# and its keys in order are
    // time,user,env,account,op,params,outcome,duration_ms,error;
    // an entry with account "", params {}, error None omits those three keys;
    // outcome DryRun serializes as "dry-run"
}
#[cfg(unix)] #[test] fn file_logger_creates_private_files() // dir mode 0o700, file mode 0o600
#[test] fn file_logger_reports_unwritable_paths()           // parent path is a regular file → Err
#[test] fn default_path_honors_xdg_state_home()             // {XDG_STATE_HOME:/s} → /s/ss/audit.log; {HOME:/h} → /h/.local/state/ss/audit.log
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core guard:: audit::` → FAIL.

- [ ] **Step 3: Implement.** `FileLogger` holds `Mutex<()>` around create-dirs → open(append, create) → write `serde_json::to_string(e)` + `\n`. On unix use `DirBuilderExt::mode(0o700)` and `OpenOptionsExt::mode(0o600)`.

- [ ] **Step 4: Run to verify pass** — same command → PASS.

- [ ] **Step 5: Commit** — `feat(guard,audit): env allowlist and JSONL audit trail`

---

### Task 4: Cache store

**Files:**
- Create: `rust/crates/ss-core/src/cache.rs`
- Modify: `rust/Cargo.toml` (`sha2 = "0.10"`), `rust/crates/ss-core/Cargo.toml` (`sha2`, and `tempfile` moved to `[dependencies]`)

**Interfaces:**
- Produces: `trait Store: Send + Sync { fn get(&self, key: &str) -> Option<Vec<u8>>; fn put(&self, key: &str, val: &[u8]) -> io::Result<()>; }`; `Nop`; `Dir::new(root: impl Into<PathBuf>)`; `cache::default_dir(v: &Vars) -> PathBuf` (`base_dir(v,"XDG_CACHE_HOME",".cache")/ss`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn nop_never_hits()
#[test] fn dir_round_trips_and_overwrites()   // put("s3/b/k/v1", b"one"); get == Some(b"one"); put again b"two"; get == Some(b"two")
#[test] fn dir_misses_unknown_keys()
#[test] fn dir_fans_out_by_sha256() {
    // key "s3/b/k/v1": file exists at root/<hex[..2]>/<hex[2..]> where hex = lowercase sha256 of the key bytes;
    // no ".tmp-" files remain in that directory after put
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core cache::` → FAIL.

- [ ] **Step 3: Implement.** `put`: create parent dirs (0o700 on unix), `tempfile::Builder::new().prefix(".tmp-").tempfile_in(parent)`, write, `persist(path)`. `get`: any read error → `None`.

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(cache): content-addressed disk store`

---

### Task 5: Presets store

**Files:**
- Create: `rust/crates/ss-core/src/preset.rs`

**Interfaces:**
- Consumes: `vars::{Vars, get, base_dir}`.
- Produces: `preset::Store::new(path: impl Into<PathBuf>)`, `path(&self) -> &Path`, and
  `get(&self, op: &str, name: &str) -> Result<Option<BTreeMap<String,String>>, PresetError>`,
  `list(&self, op: &str) -> Result<Vec<String>, PresetError>` (sorted),
  `all(&self) -> Result<BTreeMap<String, Vec<String>>, PresetError>` (ops with ≥1 preset),
  `save(&self, op: &str, name: &str, params: &BTreeMap<String,String>) -> Result<(), PresetError>`,
  `delete(&self, op: &str, name: &str) -> Result<(), PresetError>`;
  `preset::default_path(v: &Vars) -> PathBuf` (`SS_PRESETS`, else `base_dir(v,"XDG_CONFIG_HOME",".config")/ss/presets.yaml`).
  `PresetError::{Read{path,#[source] io}, Parse{path,#[source] yaml}, Write{path,#[source] io}, MissingName, Empty, NotFound{op,name}}` — messages `read presets {path}`, `parse presets {path}`, `write presets {path}`, `operation and preset name are required`, `refusing to save an empty preset`, `{op} has no preset {name:?}`. `PresetError::is_usage(&self) -> bool` (true for Parse, MissingName, Empty, NotFound).
- On-disk shape: `presets: {<op>: {<name>: {<param>: <value>}}}`, top-level struct `#[serde(deny_unknown_fields, default)]`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn round_trip_drops_empty_values()   // save("s3.version-diff","acme",{bucket:cfg-nonprod,key:tenants/acme.json,empty:""}); get → Some without "empty"
#[test] fn list_all_delete() {
    // save s3.version-diff/beta, s3.version-diff/acme, ddb.item-history/acme
    // list("s3.version-diff") == ["acme","beta"]; all().len() == 2
    // delete acme → get None; delete "nope" → Err(NotFound) with to_string() == r#"s3.version-diff has no preset "nope""#
    // delete beta → all() has no "s3.version-diff" key and the file has no "s3.version-diff:" line
}
#[test] fn missing_file_is_empty()            // list("anything") == Ok(vec![])
#[test] fn rejects_empty_presets()            // save("op","name",{a:""}) → Empty; save("","name",{a:"1"}) → MissingName
#[test] fn file_is_human_editable()           // after save("demo.hello","world",{name:world}) file contains "presets:", "demo.hello:", "world:", "name: world"
#[test] fn corrupt_file_is_a_usage_error()    // file "presets: [1,2\n" → list() Err(Parse), is_usage() == true
#[test] fn default_path_honors_ss_presets()
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core preset::` → FAIL.

- [ ] **Step 3: Implement.** Read per call (no caching); empty/comment-only file = empty; writes atomic via tempfile `.presets-` prefix + persist, parent dirs 0o700.

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(preset): named parameter sets per operation`

---

### Task 6: Fuzzy matcher

**Files:**
- Create: `rust/crates/ss-core/src/fuzzy.rs`

**Interfaces:**
- Produces: `fuzzy::score(pattern: &str, s: &str) -> Option<i32>`, `fuzzy::matches(pattern: &str, s: &str) -> bool`, `fuzzy::rank(pattern: &str, items: &[&str]) -> Vec<(usize, i32)>` (best first, stable on ties).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn matches_subsequences_case_insensitively() {
    // true: ("", "anything"), ("vdiff","s3.version-diff"), ("svd","s3.version-diff"), ("ih","ddb.item-history"),
    //       ("VDIFF","s3.version-diff"), ("restore","Restore object version")
    // false: ("diffv","s3.version-diff"), ("zzz","s3.version-diff")
}
#[test] fn exact_score_is_pinned()        // score("vdiff","s3.version-diff") == Some(22); score("","x") == Some(0)
#[test] fn score_ordering() {
    // score("vd","s3.version-diff") > score("vd","avoided")
    // score("hist","ddb.item-history") > score("hist","hxixsxtxxxxxxx")
    // score("hello","demo.hello") > score("hello","hxexlxlxo-something-very-long-here")
}
#[test] fn rank_orders_and_filters() {
    // items ["ddb.item-history","s3.version-diff","demo.hello","s3.object-inspect"]
    // rank("s3") first item is one of the two s3.* entries; rank("zzz") empty;
    // rank("") returns all four in original order with score 0
}
#[test] fn case_folding_never_shifts_indices() // score("i","İx") is Some(_) and does not panic
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core fuzzy::` → FAIL.

- [ ] **Step 3: Implement** the greedy scorer exactly:

```text
p = lowercase(pattern) per char; t = chars(s); tl = t lowercased per char (first char of to_lowercase, so len(tl)==len(t))
score=0, pi=0, last=-2
for i in 0..len(tl) while pi < len(p):
  if tl[i] != p[pi]: continue
  score += 1; if i == last+1: score += 3; if word_start(t,i): score += 5; if i == 0: score += 3
  last = i; pi += 1
if pi < len(p): None
score -= (len(tl) - len(p)) / 4  (integer division); Some(max(score, 0))
word_start(t,i): i==0 || t[i-1] in ". - _ / space #" || (t[i-1].is_lowercase() && t[i].is_uppercase())
```
Empty pattern → `Some(0)`.

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(fuzzy): subsequence scorer for pickers`

---

### Task 7: Input, View, OpResult

**Files:**
- Create: `rust/crates/ss-core/src/ops/mod.rs`, `ops/input.rs`, `ops/view.rs`
- Modify: `rust/crates/ss-core/src/lib.rs`

**Interfaces:**
- Consumes: `diff::Delta`.
- Produces:
  - `Input(BTreeMap<String,String>)` — `Clone, Debug, Default, PartialEq`; `From<BTreeMap<..>>`, `FromIterator<(K,V)>` for `K,V: Into<String>`; `insert(&mut self, k: impl Into<String>, v: impl Into<String>)`, `contains(&self, k) -> bool`, `as_map(&self) -> &BTreeMap<..>`, `into_map(self)`, and the lenient accessors `str(&self,k)->&str` (missing → `""`), `int(&self,k)->i64`, `bool(&self,k)->bool`, `globs(&self,k)->Vec<String>`, `json(&self,k)->Option<Value>`.
  - `View::{Summary { fields: Vec<(String,String)> }, Diff { left: String, right: String, delta: Delta }, Table { title: String, cols: Vec<String>, rows: Vec<Vec<String>> }, Doc { title: String, body: String, lang: String }, Tree { title: String, root: Node }}` — `Clone, Debug, PartialEq`; `View::summary<K: Into<String>, V: Into<String>>(fields: impl IntoIterator<Item=(K,V)>) -> View`.
  - `Node { label: String, children: Vec<Node> }`; `OpResult { views: Vec<View> }` (`Default`).

- [ ] **Step 1: Write the failing tests** (`ops/input.rs`)

```rust
#[test] fn typed_accessors_are_lenient() {
    // {n:" 7 ", bad:"abc", t:"TRUE", f:"0", one:"1"}
    // int("n")==7, int("bad")==0, int("missing")==0
    // bool("t") true, bool("one") true, bool("f") false, bool("bad") false   (accepts 1,t,T,TRUE,true,True / 0,f,F,FALSE,false,False)
    // str("missing") == ""
}
#[test] fn globs_accept_json_arrays_and_csv() {
    // r#"["**.a","**.b"]"# → ["**.a","**.b"]; "**.a, **.b,," → ["**.a","**.b"];
    // "[not json" → ["[not json"]; "" → []
}
#[test] fn json_is_none_when_empty_or_invalid() // "" → None; "{" → None; r#"{"a":1}"# → Some(json!({"a":1}))
```
`ops/view.rs`: `summary_builds_ordered_fields` — `View::summary([("A","1"),("B","2")])` equals `View::Summary{fields: vec![("A".into(),"1".into()),("B".into(),"2".into())]}`.

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core ops::` → FAIL.

- [ ] **Step 3: Implement.** All accessors trim the value first. `int` parses `i64` (no float/garbage acceptance). `json` parses through `jsonutil::parse` so numbers are normalized.

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(ops): lenient Input and the View union`

---

### Task 8: Operation, Deps, Registry

**Files:**
- Create: `rust/crates/ss-core/src/ops/operation.rs`, `ops/registry.rs`, `ops/confirm.rs`
- Modify: `ops/mod.rs`; `rust/Cargo.toml` (`tokio-util = "0.7"`); `ss-core/Cargo.toml` (`anyhow`, `async-trait`, `tokio`, `tokio-util`)

**Interfaces:**
- Consumes: `guard::Class`, `cache::{Store, Nop}`, `config::{Environment, DiffDefaults, TraceConfig}`, `Input`, `View`, `OpResult`.
- Produces:
  - `Meta { id, name, summary, category: &'static str, class: Class }` — `Clone, Copy, Debug`.
  - `ParamType { String, Int, Bool, Enum, Duration, Json }` — `Display` `string|int|bool|enum|duration|json`; `value_name(self) -> &'static str` `STRING|INT|BOOL|ENUM|DURATION|JSON`.
  - `Choice { value: String, label: Option<String> }`.
  - `BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>`; `SuggestFn = Arc<dyn Fn(Deps, Input) -> BoxFuture<anyhow::Result<Vec<Choice>>> + Send + Sync>`.
  - `Param { name: &'static str, label: Option<&'static str>, ty: ParamType, required: bool, default: Option<&'static str>, help: Option<&'static str>, suggest: Option<SuggestFn> }` — `Clone`; `Param::new(name) -> Param` (ty String, everything else empty) with builder methods `label/ty/required/default/help/suggest`; `display_label(&self) -> &str`.
  - `Deps { cache: Arc<dyn Store>, env: Environment, diff: DiffDefaults, trace: TraceConfig }` — `Clone`; `Default` uses `cache::Nop`.
  - `#[async_trait] trait Operation: Send + Sync { fn meta(&self) -> Meta; fn params(&self) -> Vec<Param>; async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult>; }` (declare `pub struct Runner;` placeholder in `ops/runner.rs` with a doc comment; Task 9 replaces it).
  - `Registry` (`Clone`, `Default`): `new()`, `register(&mut self, op: Arc<dyn Operation>) -> Result<(), RegistryError>`, `get(&self, id: &str) -> Option<Arc<dyn Operation>>`, `all(&self) -> Vec<Arc<dyn Operation>>` (by category, then id), `categories(&self) -> Vec<&'static str>` (distinct, in `all()` order). `RegistryError::{EmptyId, Duplicate(String)}` — `operation id is empty`, `duplicate operation id {0:?}`.
  - `ConfirmSpec { op_id: String, class: Class, resource: String, step: String, preview: Vec<View> }` (`Clone, Debug`); `#[async_trait] trait Confirmer: Send + Sync { async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool>; }`.

- [ ] **Step 1: Write the failing tests** (`ops/registry.rs`, with a tiny `Stub { id, category }` op returning `OpResult::default()`)

```rust
#[test] fn sorts_by_category_then_id_and_looks_up() {
    // register s3.version-diff/S3, ddb.item-compare/DynamoDB, s3.object-inspect/S3
    // all() ids == ["ddb.item-compare","s3.object-inspect","s3.version-diff"]
    // get("s3.version-diff").is_some(); get("nope").is_none(); categories() == ["DynamoDB","S3"]
}
#[test] fn rejects_duplicate_and_empty_ids() // second "dup.op" → Err(Duplicate("dup.op")); "" → Err(EmptyId)
```
`ops/operation.rs`: `param_type_names_are_stable` (Display + value_name for all six) and `display_label_falls_back_to_name`.

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core ops::` → FAIL.

- [ ] **Step 3: Implement** per Interfaces; `Registry` is a `BTreeMap<&'static str, Arc<dyn Operation>>`.

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(ops): Operation trait, Deps, explicit Registry`

---

### Task 9: Runner — execute and step

**Files:**
- Modify: `rust/crates/ss-core/src/ops/runner.rs` (replace placeholder), `ops/mod.rs` (re-exports)
- Test: `rust/crates/ss-core/tests/runner.rs`

**Interfaces:**
- Consumes: Tasks 3, 7, 8.
- Produces:
  - `StepStatus { Running, Done, Failed, Skipped }`; `Progress { step: String, status: StepStatus, elapsed: Duration, err: Option<String> }` (`err` is the step's own error, `{:#}`, without the step-name prefix).
  - `RunnerError::{DryRun, Cancelled, MissingParam { param: String }}` — `dry-run: stopped before mutating step`, `cancelled`, `required param {param:?} missing`.
  - `FailureKind { MissingParam, Denied, Declined, Cancelled, Failed }`; `RunFailure { kind: FailureKind, error: anyhow::Error, views: Vec<View> }` (`Debug`).
  - `pub fn find_cause<E: std::error::Error + Send + Sync + 'static>(err: &anyhow::Error) -> Option<&E>` (searches `err.chain()`).
  - `pub fn with_defaults(op: &dyn Operation, input: Input) -> Input` (fills declared defaults for keys not present).
  - `Runner`: `pub deps: Deps`; `new(deps) -> Self` (guard `AllowAll`, audit `Nop`, fresh token); consuming builders `guard(Arc<dyn Guard>)`, `audit(Arc<dyn Logger>)`, `progress(impl Fn(Progress) + Send + Sync + 'static)`, `dry_run(bool)`, `yes(bool)`, `user(impl Into<String>)`, `confirmer(Arc<dyn Confirmer>)`, `cancel(CancellationToken)`; `cancel_token(&self) -> CancellationToken`;
    `async fn execute(&mut self, op: &dyn Operation, input: Input) -> Result<OpResult, RunFailure>`;
    `async fn step<F, Fut, T>(&mut self, name: &str, f: F) -> anyhow::Result<T> where F: FnOnce(CancellationToken) -> Fut, Fut: Future<Output = anyhow::Result<T>>`.

**Execute semantics (in order):** apply defaults → first required param whose value is empty → `MissingParam` (not audited) → guard with `Request{op_id, class, resource: input.str("resource")}`; on `Err` audit `denied` and return `Denied` → reset per-run state (meta, preview, resource = `input.str("resource")`, confirmed, escalated) → `op.run` → classify: chain has `RunnerError::DryRun` → audit `dry-run`, `Ok` with views `[Summary{Operation: id, Outcome: "dry-run: stopped before the first mutating step"}]`; chain has a `GuardError` with `is_decline()` → audit `declined`, `Declined` with views `[Summary{Operation: id, Outcome: "declined: stopped before the first mutating step"}]`; chain has `RunnerError::Cancelled` → audit `error`, `Cancelled`; other `Err` → audit `error`, `Failed`; `Ok` → audit `ok`. Audit records skip `Class::Read`; `params` = the input after defaults; `error` = `{e:#}`; a `record` error is ignored.

**Step semantics:** token already cancelled → `Err(Cancelled)` with context `name`, no progress emitted. Otherwise emit `Running`, run `f(token.clone())` inside `tokio::select! { biased; _ = token.cancelled() => Err(Cancelled), r = fut => r }`, emit `Done`/`Failed` with elapsed; errors get `.context(name.to_string())` so `{:#}` reads `<name>: <inner>`.

- [ ] **Step 1: Write the failing tests** in `tests/runner.rs`. Fixture: `Script { meta: Meta, params: Vec<Param>, acts: Vec<Act>, mutated: Arc<AtomicUsize> }` implementing `Operation`, running acts in order and returning the first error; `enum Act { Step(&'static str), FailStep(&'static str, &'static str), Sleep(&'static str, u64 /*ms*/), AssertInt(&'static str, i64) }` (`AssertInt` fails with `expected <k>=<n>` unless `input.int(k) == n`). Task 10 adds the mutating acts. Helpers `RecordingAudit { entries: Mutex<Vec<Entry>> }`, `FailingAudit` (always `Err`).

```rust
#[tokio::test] async fn step_wraps_errors_with_step_name()     // FailStep("fetch both versions","AccessDenied") → kind Failed; format!("{:#}", f.error) == "fetch both versions: AccessDenied"
#[tokio::test] async fn step_emits_progress_in_order()         // Step("one"),Step("two") → ["one:running","one:done","two:running","two:done"]
#[tokio::test] async fn failed_step_reports_unprefixed_error() // FailStep("x","boom") → last Progress {status Failed, err Some("boom")}
#[tokio::test] async fn required_param_and_defaults() {
    // params: bucket required; repeat default "3"; acts [AssertInt("repeat", 3)]
    // execute(Input::default()) → kind MissingParam, error to_string() == r#"required param "bucket" missing"#
    // execute({bucket:b}) → Ok
}
#[tokio::test] async fn guard_denial_beats_yes_and_is_audited() {
    // Mutate op; guard EnvAllowlist{name:"prod",allowed:false}; yes(true); RecordingAudit
    // → kind Denied, op body never runs (mutated==0), one entry outcome Denied, env "", error contains "not in the configured allowlist"
}
#[tokio::test] async fn non_read_runs_are_audited() {
    // Mutate op, yes(true), deps.env {name:"dev",account_id:"123"}, user "danny", FileLogger(tmp/audit.log), input {x:"1"}
    // file has one JSON line: op "test.mutate", outcome "ok", user "danny", env "dev", account "123", params {"x":"1"}
}
#[tokio::test] async fn read_runs_are_not_audited()            // Read op with FileLogger → log file does not exist
#[tokio::test] async fn pre_cancelled_token_stops_steps()      // cancel before execute; Step("never runs") → kind Cancelled, "{:#}" == "never runs: cancelled", no progress events
#[tokio::test] async fn cancellation_mid_step_is_prompt_and_audited() {
    // Mutate-class op, yes(true), Sleep("slow", 5000); spawn: sleep 20ms then token.cancel()
    // → kind Cancelled within 1s; "{:#}" == "slow: cancelled"; RecordingAudit one entry outcome Error, error Some("slow: cancelled")
}
#[tokio::test] async fn audit_write_failure_does_not_change_the_outcome() // Mutate op, yes(true), FailingAudit → Ok(result) identical to the run with Nop
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core --test runner` → FAIL.

- [ ] **Step 3: Implement** per the semantics above (`mutate` arrives in Task 10).

- [ ] **Step 4: Run to verify pass** — `cargo test -p ss-core --test runner` → PASS.

- [ ] **Step 5: Commit** — `feat(ops): Runner execute, steps, guard, audit, cancellation`

---

### Task 10: Runner — mutate and confirmation

**Files:**
- Modify: `rust/crates/ss-core/src/ops/runner.rs`
- Test: `rust/crates/ss-core/tests/runner.rs`

**Interfaces:**
- Produces: `Runner::mutate<F, Fut, T>(&mut self, name: &str, f: F) -> anyhow::Result<T>` (same bounds as `step`); `preview(&mut self, views: impl IntoIterator<Item = View>)`; `set_resource(&mut self, s: impl Into<String>)`; `escalate(&mut self)`.

**Semantics:** dry-run → emit `Skipped`, `Err(DryRun)`. First mutate of a run confirms: class = `Destructive` if escalated else `meta.class`; `Mutate && yes` → proceed; no confirmer → `Err(GuardError::NoConfirmer{op})`; else `confirm(ConfirmSpec{op_id, class, resource: resource or op_id when empty, step: name, preview: registered views})`; `Err` passes through unchanged (outcome `error`); `Ok(false)` → `Err(GuardError::NotConfirmed{op})`. On any confirm failure emit `Skipped`. Later mutates in the same run skip confirmation. Then behaves as `step`.

- [ ] **Step 1: Write the failing tests.** Extend `Act` with `Mutate(&'static str)` (increments `mutated`), `Preview` (registers `View::summary([("About to","write things")])`), `Resource(&'static str)`, `Escalate`. Fixture op `writer(class)` = `[Preview, Resource("cfg-nonprod/tenants/acme.json"), Mutate("write one"), Mutate("write two")]`. `RecordingConfirmer { specs: Mutex<Vec<ConfirmSpec>>, answer: Result<bool, &'static str> }`.

```rust
#[tokio::test] async fn confirm_asks_once_per_run_with_preview() {
    // Mutate, answer Ok(true) → mutated==2, one spec: op_id "test.write", class Mutate, step "write one",
    // resource "cfg-nonprod/tenants/acme.json", preview.len()==1
}
#[tokio::test] async fn resource_defaults_to_op_id()             // writer without Resource → spec.resource == "test.write"
#[tokio::test] async fn decline_stops_before_mutation_and_audits_declined() {
    // answer Ok(false), RecordingAudit → kind Declined, mutated 0, views[0] Summary Outcome "declined: stopped before the first mutating step",
    // one entry outcome Declined; find_cause::<GuardError>(&f.error) is NotConfirmed
}
#[tokio::test] async fn confirmer_error_is_a_failure_not_a_decline() // answer Err("tty gone") → kind Failed, "{:#}" contains "tty gone", audit Error, mutated 0
#[tokio::test] async fn yes_covers_mutate_only() {
    // Mutate + yes, no confirmer → Ok, mutated 2
    // Destructive + yes + confirmer Ok(true) → confirmer consulted exactly once
}
#[tokio::test] async fn yes_never_covers_an_escalated_run() {
    // Mutate op with Escalate before the mutates, yes(true), confirmer Ok(true) → one spec with class Destructive
    // same op, yes(true), no confirmer → kind Declined, mutated 0, error to_string() == "no confirmer available: operation not confirmed"
}
#[tokio::test] async fn missing_confirmer_fails_closed()         // Mutate, no yes, no confirmer → kind Declined, mutated 0
#[tokio::test] async fn dry_run_skips_confirmation_entirely() {
    // dry_run(true) + confirmer → Ok; mutated 0; no specs; views[0] Summary Outcome "dry-run: stopped before the first mutating step";
    // progress includes "write one:skipped"; RecordingAudit outcome DryRun with error None
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core --test runner` → FAIL (no `mutate`).

- [ ] **Step 3: Implement** per semantics.

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(ops): confirm at first mutation, --yes, escalation, dry-run`

---

### Task 11: Text and JSON renderers

**Files:**
- Create: `rust/crates/ss-core/src/render.rs`

**Interfaces:**
- Consumes: `View`, `Node`, `diff::{Change, Kind, Delta}`, `jsonutil::compact`.
- Produces: `render::text(views: &[View]) -> String`; `render::json(views: &[View]) -> serde_json::Value`.

**Text format:** views joined by one blank line. Summary: `{key:<w} : {value}` with `w` = longest key in chars. Table: optional title line; header, a rule of `-` per column width, rows; two-space gutter; missing cells blank; each line `trim_end`. Diff: `diff {left} → {right}   ({stats})` then per change `  + p = v`, `  - p = v`, `  ~ p: o -> n`, `  ! p: o -> n (type changed)` (`p` = dotted path, values via `compact`). Doc: optional title line, body with trailing newlines trimmed, newline. Tree: optional title, then `label` lines indented two spaces per depth.
**JSON shape:** `{"views":[…]}` with `{"type":"summary","fields":{k:v}}` (later duplicate keys win), `{"type":"table","title","cols","rows"}`, `{"type":"diff","left","right","delta":<Delta serialized>}`, `{"type":"doc","title","lang","body"}`, `{"type":"tree","title","root":{"label","children":[…]}}`.

- [ ] **Step 1: Write the failing tests.** Fixture `all_views()`: Summary `[("Object","cfg/a.json"),("Version","v1")]`; Diff `v1 → current` of `{"tier":"gold","port":"8080"}` vs `{"tier":"platinum","port":8080}` (both via `jsonutil::parse`); Table `Versions` cols `["ID","Modified"]` rows `[["3HL4","2026-07-01"],["a"]]`; Doc `Raw` body `"{\"a\":1}\n\n"` lang `json`; Tree `Layout` root `root` → `leaf`.

```rust
#[test] fn text_renders_every_view_kind() {
    assert_eq!(text(&all_views()), "\
Object  : cfg/a.json
Version : v1

diff v1 → current   (~1 !1)
  ! port: \"8080\" -> 8080 (type changed)
  ~ tier: \"gold\" -> \"platinum\"

Versions
ID    Modified
----  ----------
3HL4  2026-07-01
a

Raw
{\"a\":1}

Layout
root
  leaf
");
}
#[test] fn untitled_table_has_no_title_line()  // Table{title:"",cols:["A"],rows:[]} → "A\n-\n"
#[test] fn json_has_the_typed_view_shape() {
    // v = json(&all_views()); v["views"][0] == json!({"type":"summary","fields":{"Object":"cfg/a.json","Version":"v1"}})
    // v["views"][2] == json!({"type":"table","title":"Versions","cols":["ID","Modified"],"rows":[["3HL4","2026-07-01"],["a"]]})
    // v["views"][3] == json!({"type":"doc","title":"Raw","lang":"json","body":"{\"a\":1}\n\n"})
    // v["views"][4] == json!({"type":"tree","title":"Layout","root":{"label":"root","children":[{"label":"leaf","children":[]}]}})
    // v["views"][1]["delta"]["stats"] == json!({"added":0,"removed":0,"changed":1,"type_changed":1})
}
#[test] fn json_numbers_print_without_fraction() // jsonutil::pretty(&json(&all_views())) contains "\"new\": 8080," and not "8080.0"
#[test] fn empty_table_rows_are_an_empty_array() // pretty(&json(&[Table{title:"",cols:vec![],rows:vec![]}])) contains "\"rows\": []"
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core render::` → FAIL.

- [ ] **Step 3: Implement.** Build the JSON with `serde_json::json!`/`Map`; the diff delta via `serde_json::to_value(&delta)` (cannot fail for these types; map an error to `Value::Null` rather than panicking).

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(render): text and JSON renderers for views`

---

### Task 12: Markdown report

**Files:**
- Create: `rust/crates/ss-core/src/report.rs`

**Interfaces:**
- Produces: `report::markdown(meta: &Meta, input: &Input, views: &[View], now: DateTime<Utc>) -> String`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn markdown_renders_every_view_kind() {
    // meta {id "s3.version-diff", name "Compare object versions"}, input {bucket:"cfg-nonprod", empty:""},
    // views = Summary[("Object","cfg/a.json")], the Diff/Table/Doc/Tree from render's fixture minus the short row,
    // now = 2026-11-08T12:00:00Z
    assert_eq!(md, "\
# Compare object versions

- **Operation:** `s3.version-diff`
- **Generated:** 2026-11-08T12:00:00Z
- **Params:** `bucket=cfg-nonprod`

## Summary

- **Object:** cfg/a.json

## Diff: v1 → current

**~1 !1**

```diff
- port = \"8080\"  (type)
+ port = 8080  (type)
- tier = \"gold\"
+ tier = \"platinum\"
```

## Versions

| ID | Modified |
|---|---|
| 3HL4 | 2026-07-01 |

## Raw

```json
{\"a\":1}
```

## Layout

- root
  - leaf

");
}
#[test] fn params_line_is_sorted_and_omitted_when_empty() // input {b:"2",a:"1"} → "- **Params:** `a=1` `b=2`"; input {} or {x:""} → no Params line
#[test] fn table_cells_escape_pipes()                     // cell "a|b" → "| a\\|b |"
#[test] fn empty_diff_has_no_fence()                      // delta with no changes → "**no changes**\n\n" and no "```diff"
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core report::` → FAIL.

- [ ] **Step 3: Implement.** Params: collect `` `k=v` `` for non-empty values, sort the formatted strings, join with a space. Changed → `- p = old` then `+ p = new`; TypeChanged same with `  (type)` suffix. `now` formatted with `to_rfc3339_opts(SecondsFormat::Secs, true)`.

- [ ] **Step 4: Run to verify pass** — PASS.

- [ ] **Step 5: Commit** — `feat(report): markdown export`

---

### Task 13: Demo operations and the builtin registry

**Files:**
- Create: `rust/crates/ss-core/src/ops/demo/{mod,hello,json_diff}.rs`
- Modify: `rust/crates/ss-core/src/ops/mod.rs` (`pub mod demo; pub fn builtin() -> Registry`)

**Interfaces:**
- Consumes: Runner (Tasks 9–10), `diff::{json, text, Opts}`, `jsonutil::parse`.
- Produces: `demo::{Hello, JsonDiff}` (unit structs); `demo::json_files_in(dir: &Path) -> io::Result<Vec<Choice>>`; `ops::builtin() -> Registry` registering both (`expect("builtin operation ids are unique")`).

**demo.hello** — Meta: `demo.hello` / `Hello (demo)` / `Fake operation exercising the full pipeline end to end` / `Demo` / Read. Params: `name` (label `Name`, required, help `Who to greet`, suggest → `world — the classic`, `support — the audience`, `danny — the owner` with values `world|support|danny`); `repeat` (label `Repeat`, Int, default `3`, help `Rows to generate`); `shout` (label `Shout`, Bool, default `false`, help `Uppercase the greeting`). Run: `repeat <= 0 → 1`; greeting `Hello, {name}!`, uppercased when `shout`; step `compose greeting` sleeps 150 ms; step `tabulate` sleeps 150 ms then builds rows `[i, greeting]` for `1..=repeat`; env label = `deps.env.name` or `(none)`. Views: Summary `Greeting`, `Rows`, `Environment`; Table `Greetings` cols `#`, `Message`.

**demo.json-diff** — Meta: `demo.json-diff` / `Diff two JSON files` / `Semantic diff of two local JSON documents` / `Demo` / Read. Params: `left` (label `Left file`, required, help `Path to JSON file A`, suggest = `json_files_in(".")`); `right` (label `Right file`, required, help `Path to JSON file B`, same suggest); `ignore` (label `Ignore paths`, Json, help `Glob list, e.g. "**.updatedAt,**.etag" or ["**.updatedAt"]`); `tolerance` (label `Float tolerance`, default `0`, help `Epsilon for numeric comparison`). Run: tolerance parse failure → error `invalid tolerance "<v>"` with the float parse error as source (before any step); step `read both files` reads both with `tokio::try_join!`, each read error with context `open <path>`; step `parse and diff`: if either side fails `jsonutil::parse`, line-diff mode; else `diff::json` with `ignore = deps.diff.ignore ++ input.globs("ignore")`, `array_match = deps.diff.array_match`, tolerance. Views (JSON mode): Summary `Left`, `Right`, `Changes` = stats display; Diff with left/right = file basenames. Views (line mode): Summary `Left`, `Right`, `Mode` = `line diff (at least one side is not valid JSON)`; Doc `Line diff` body = `diff::text`, lang `diff`.

- [ ] **Step 1: Write the failing tests** (in each module)

```rust
// hello.rs
#[tokio::test] async fn hello_greets_and_tabulates()   // {name:world} → Summary [("Greeting","Hello, world!"),("Rows","3"),("Environment","(none)")]; Table rows ["1","Hello, world!"]..["3",…]
#[tokio::test] async fn hello_lenient_repeat_and_shout() // {name:x, repeat:"abc", shout:"true"} → Rows "1", greeting "HELLO, X!"
#[tokio::test] async fn hello_suggests_three_names()   // suggest(Deps::default(), Input::default()) values == ["world","support","danny"]
// json_diff.rs (files in a tempdir)
#[tokio::test] async fn diffs_json_files_with_config_and_param_ignores() {
    // a {"a":1,"t":"x","u":1}, b {"a":2,"t":"y","u":2}; deps.diff.ignore ["**.t"]; input ignore "**.u"
    // → views[0] Changes "~1"; views[1] Diff left "a.json" right "b.json" with one change at "a"
}
#[tokio::test] async fn falls_back_to_line_diff_for_invalid_json() // a "not json\n" → Summary Mode line…; Doc lang "diff"
#[tokio::test] async fn missing_file_names_the_step_and_path()     // left missing → kind Failed; "{:#}" starts with "read both files: open <left>: "
#[tokio::test] async fn bad_tolerance_fails_before_reading()       // tolerance "x" → "{:#}" starts with r#"invalid tolerance "x": "#
#[test] fn json_files_in_lists_only_json()                          // dir with a.json, b.txt → [Choice{value:"a.json",label:None}]
// mod.rs
#[test] fn builtin_registers_the_demo_ops()                         // builtin().all() ids == ["demo.hello","demo.json-diff"]
```
Tests drive ops through `Runner::new(Deps::default()).execute(&op, input)`.

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss-core ops::demo` → FAIL.

- [ ] **Step 3: Implement** per the spec above.

- [ ] **Step 4: Run to verify pass** — `cargo test -p ss-core` → PASS (all suites).

- [ ] **Step 5: Commit** — `feat(ops): demo.hello and demo.json-diff, builtin registry`

---

### Task 14: CLI tree and per-operation subcommands

**Files:**
- Create: `rust/crates/ss/src/cli.rs`, `rust/crates/ss/src/cli/dynamic.rs`
- Modify: `rust/crates/ss/src/lib.rs` (move `Cli` out; `pub use cli::{Cli, parse_from, command}`), `rust/crates/ss/tests/e2e.rs` (keep `exit_codes_are_distinct_and_stable` and `version_is_the_workspace_version`; delete the obsolete `no_arguments_runs_and_exits_ok`, re-added in Task 15)

**Interfaces:**
- Consumes: `ss_core::ops::{Registry, Operation, Param, ParamType, Input}`, `ss_core::config::{File, Environment, resolve}`, `ss_core::vars::{Vars, get}`.
- Produces:
  - `Cli { command: Option<Command>, global: GlobalFlags }` (`Parser`, `Debug, Clone`), about `Support operations for non-production AWS and Kubernetes`.
  - `GlobalFlags` (all `global = true`): `--profile <NAME>` (`Override the environment's AWS profile (SS_PROFILE)`), `--region <REGION>` (`Override the environment's AWS region (SS_REGION)`), `--env <NAME>` (`Target environment; writes require it in the config allowlist (SS_ENV) [default: config default_env]`), `--context <KUBE_CONTEXT>` (`Override the environment's Kubernetes context`), `-o, --output <FORMAT>` (`Output format: text or json [default: text]`), `--export <FILE>` (`Write a markdown report to FILE`), `--dry-run` (`Stop before the first mutating step`), `--yes` (`Auto-confirm Mutate operations; never Destructive`), `-v, --verbose` (`Print step progress to stderr`), `--no-cache` (`Bypass the local object-version cache`). `output` is `Option<String>` (validated by `output_format`).
  - `Command::{Ops(OpsCmd), Run(RunCmd), Presets(PresetsCmd)}`; `OpsCmd::{List, Describe { id }}`; `PresetsCmd::{List { op: Option<String> }, Show { op, name }, Delete { op, name }}` (value names `OPERATION`, `NAME`).
  - `RunCmd { preset: Option<String> /* --preset <NAME>, global: "Load a saved parameter preset; explicit flags override it" */, save_preset: Option<String> /* --save-preset <NAME>, global: "After a successful run, save the given params under NAME" */, #[arg(skip)] call: Option<OpCall> }`; `OpCall { id: String, input: Input }`.
  - `pub enum Output { Text, Json }`; `GlobalFlags::output_format(&self) -> Result<Output, String>`; `GlobalFlags::resolve_env(&self, cfg: &File, vars: &Vars) -> Environment`; `Cli::prefix(&self) -> String`.
  - `pub fn command(registry: &Registry) -> clap::Command`; `pub fn parse_from<I, T>(registry: &Registry, args: I) -> Result<Cli, clap::Error> where I: IntoIterator<Item = T>, T: Into<OsString> + Clone`.
  - `dynamic::op_command(op: &dyn Operation) -> clap::Command`; `dynamic::collect_input(op: &dyn Operation, m: &ArgMatches) -> Input`.

**Rules:** `command()` = `Cli::command()` with `run` augmented by one subcommand per `registry.all()` and `subcommand_required(true)`. Op subcommand: name = id, `about` = summary, `long_about` = `{name} ({category}, class: {class})\n\n{summary}`. One `--<param>` per param, help = `help`, plus ` [default: <d>]` when a default exists and ` [required]` when required. Bool → `num_args(0..=1)`, `require_equals(true)`, `default_missing_value("true")`, `BoolishValueParser`; others → string with `value_name(ty.value_name())`. No clap defaults or required flags (the Runner applies both after preset merge). `collect_input` includes only params whose `value_source` is `CommandLine` (bools as `"true"`/`"false"`). `output_format`: `None|"text"|"table"` → Text, `"json"` → Json, else `Err(format!("unknown output format {s:?} (want text or json)"))`. `resolve_env`: `name = resolve(flag env, SS_ENV, cfg.default_env, "")`, `profile = resolve(flag, SS_PROFILE, "", "")`, `region` likewise, `cfg.env(..)`, then non-empty `--context` replaces `kube_context`. `prefix`: `ss`, `ss ops list`, `ss ops describe`, `ss run <id>` (`ss run` without a call), `ss presets list|show|delete`.

- [ ] **Step 1: Write the failing tests** (`cli.rs` `#[cfg(test)]`, using `ops::builtin()`)

```rust
#[test] fn command_tree_is_valid()                  // command(&builtin()).debug_assert()
#[test] fn run_collects_only_explicit_flags()       // ["ss","run","demo.hello","--name","x"] → call.id "demo.hello", input == {name:"x"}
#[test] fn bool_params_accept_bare_and_explicit()   // "--shout" → "true"; "--shout=false" → "false"
#[test] fn globals_parse_after_the_op_id()          // ["ss","run","demo.hello","--name","x","-o","json","--env","dev","--preset","p","--yes"] → output Some("json"), env Some("dev"), preset Some("p"), yes
#[test] fn unknown_op_and_missing_op_are_usage_errors() // ["ss","run","nope"] and ["ss","run"] → Err with exit_code() == 2
#[test] fn param_help_shows_default_and_required()  // op_command(&Hello): arg "name" help "Who to greet [required]", "repeat" help "Rows to generate [default: 3]", value_name "INT"
#[test] fn output_format_validation()               // None/"text"/"table" → Text; "json" → Json; "xml" → Err(r#"unknown output format "xml" (want text or json)"#)
#[test] fn resolve_env_precedence() {
    // cfg default_env "dev" with environments dev{profile:p-dev,kube_context:k-dev}, stage{}
    // no flags, {} → name "dev", allowed, profile "p-dev"
    // vars {SS_ENV:"stage"} → "stage"; flag --env prod + SS_ENV stage → "prod", allowed false
    // --profile x → profile "x"; --context k2 → kube_context "k2"; vars {SS_PROFILE:"y"} → profile "y"
}
#[test] fn prefix_names_the_subcommand()            // run demo.hello → "ss run demo.hello"; ops describe x → "ss ops describe"; bare → "ss"
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss cli::` → FAIL.

- [ ] **Step 3: Implement.** `parse_from`: `command(registry).try_get_matches_from(args)?` → `Cli::from_arg_matches(&m)?` → when `run` matched, fill `call` from its subcommand name and `collect_input`.

- [ ] **Step 4: Run to verify pass** — `cargo test -p ss` → PASS.

- [ ] **Step 5: Commit** — `feat(cli): clap tree with one subcommand per operation`

---

### Task 15: Pipeline, services, `ss ops`, `ss presets`

**Files:**
- Create: `rust/crates/ss/src/{io.rs, services.rs, run.rs}`, `rust/crates/ss/src/cmd/{mod,ops,presets}.rs`
- Modify: `rust/crates/ss/src/{lib.rs, main.rs}`, `rust/crates/ss/Cargo.toml` (`serde_json`, `tokio-util`, `chrono`; dev `tempfile`), `rust/crates/ss/tests/e2e.rs`

**Interfaces:**
- Consumes: Task 14; `ss_core::{config, cache, guard, audit, preset, render, ops}`.
- Produces:
  - `io::Sink` (`Clone`, `Write`; wraps `Arc<Mutex<Box<dyn Write + Send>>>`), `io::Source` (`Clone`; `read_line(&self) -> io::Result<Option<String>>`, `None` at EOF), `Io { stdout: Sink, stderr: Sink, stdin: Source }`, `Io::std() -> Io`, `Io::capture(stdin: &str) -> (Io, Captured)`, `Captured::{stdout(&self) -> String, stderr(&self) -> String}`.
  - `Services { registry: Registry, vars: Vars }`; `Services::from_process()` (`ops::builtin()`, `vars::process()`); `Services::isolated(root: &Path)` (`builtin()`, vars `{HOME: root, USER: "tester"}`).
  - `run::run(cli, services) -> Exit` (real `Io`, two-stage Ctrl-C), `run::run_with(cli, services, io) -> Exit` (never interrupted), `run::run_with_interrupt<F>(cli, services, io, interrupt: F) -> Exit where F: Future<Output = ()> + Send + 'static`; all re-exported from `lib.rs`.
  - Internal `enum Failure { Usage(anyhow::Error), Failed(anyhow::Error), Interrupted(anyhow::Error) }` → `Exit::{Usage, Failed, Interrupted}`; the pipeline prints `{prefix}: {e:#}` to stderr once.
  - `struct App { cfg: config::File, deps: Deps, guard: Arc<dyn Guard>, audit: Arc<dyn Logger>, presets: preset::Store, user: String, cancel: CancellationToken }` built once per invocation.

**Pipeline:** load config from `config::default_path(vars)` (any error → Usage) → `env = cli.global.resolve_env(..)` → cache `Dir(cache::default_dir(vars))`, or `Nop` with `--no-cache` → `Deps{cache, env, diff: cfg.diff, trace: cfg.trace}` → guard `EnvAllowlist{name, allowed}` → audit `FileLogger(audit::default_path(vars))` → presets `Store(preset::default_path(vars))` → user = `USER`, else `LOGNAME`, else `""` → dispatch. No subcommand → print `command(registry).render_help()` to stdout, Ok. `run` dispatch is a stub returning `Failure::Usage(anyhow!("not available"))` until Task 16.
**`ss ops list`:** `render::text` of Table cols `ID, CATEGORY, CLASS, SUMMARY`, one row per op. **`ss ops describe <id>`:** unknown → Usage `unknown operation "<id>" (try `ss ops list`)`; else Summary (`ID`, `Name`, `Category`, `Class`, `Summary`) + Table `Params` cols `FLAG, TYPE, REQUIRED, DEFAULT, HELP` rows `--<name>`, type, `yes`/``, default or ``, help or ``.
**`ss presets list [op]`:** rows `OPERATION, PRESET` (ops sorted, names sorted); none → stdout `no presets saved yet — use `ss run <op> ... --save-preset <name>``. **`show <op> <name>`:** missing → Usage `<op> has no preset "<name>"`; else Summary of `--<k>` → `v` sorted by key. **`delete`:** stdout `deleted preset "<name>" for <op>`. Preset errors: `is_usage()` → Usage, else Failed.
**main.rs:** `Services::from_process()` → `parse_from(&services.registry, std::env::args_os())` (`Err(e) => e.exit()`) → `std::process::exit(ss::run(cli, services).await.code())`.

- [ ] **Step 1: Write the failing tests** in `tests/e2e.rs`. Helper `async fn ss(root: &Path, args: &[&str]) -> (Exit, String, String)` = `Services::isolated(root)`, `parse_from` (panic on parse error), `Io::capture("")`, `run_with`.

```rust
#[tokio::test] async fn bare_invocation_prints_help()  // ["ss"] → Ok, stdout contains "Usage: ss", stderr empty
#[tokio::test] async fn ops_list_shows_every_operation() // Ok; stdout first line starts with "ID"; contains "demo.hello", "demo.json-diff", "read"
#[tokio::test] async fn ops_describe_lists_params()    // demo.hello → stdout contains "ID       : demo.hello", "--name", "Who to greet", "--repeat", "int", "3", "yes"
#[tokio::test] async fn ops_describe_unknown_is_usage() // nope → Usage; stderr == "ss ops describe: unknown operation \"nope\" (try `ss ops list`)\n"; stdout empty
#[tokio::test] async fn config_typo_exits_2_naming_the_key() // root/.config/ss/config.yaml = "envs: {}\n"; ops list → Usage; stderr starts "ss ops list: parse config " and contains "unknown field `envs`"
#[tokio::test] async fn presets_list_show_delete() {
    // write root/.config/ss/presets.yaml: "presets:\n  demo.hello:\n    w:\n      name: world\n"
    // list → stdout contains "demo.hello" and "w"; show demo.hello w → "--name : world"
    // show demo.hello nope → Usage, stderr == "ss presets show: demo.hello has no preset \"nope\"\n"
    // delete demo.hello w → stdout "deleted preset \"w\" for demo.hello\n"; list → "no presets saved yet — …"
}
```
`services.rs` unit test: `isolated_paths_stay_under_root` — config, presets, audit, cache default paths all start with `root`.

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss` → FAIL.

- [ ] **Step 3: Implement.** Two-stage Ctrl-C in `run()`: first `tokio::signal::ctrl_c()` resolves the interrupt future; a spawned task then waits for a second one and calls `std::process::exit(130)`. `run_with_interrupt` spawns a task that awaits `interrupt` then cancels `App.cancel`.

- [ ] **Step 4: Run to verify pass** — `cargo test -p ss` → PASS.

- [ ] **Step 5: Commit** — `feat(cli): pipeline, exit codes, ops and presets commands`

---

### Task 16: `ss run`, confirmation prompt, interrupt, docs, parity check

**Files:**
- Create: `rust/crates/ss/src/cmd/run_op.rs`, `rust/crates/ss/src/confirm.rs`
- Modify: `rust/crates/ss/src/run.rs` (dispatch `run`), `rust/crates/ss/tests/e2e.rs`, `rust/CHANGELOG.md`, `rust/README.md`, `docs/superpowers/plans/2026-11-07-rust-port-master-plan.md` (§4a)

**Interfaces:**
- Consumes: Tasks 9–15.
- Produces: `confirm::StdinConfirmer { stdin: Source, stderr: Sink }` implementing `Confirmer`.

**`ss run <op>` order:** `output_format()` (Err → Usage, before anything else) → input = preset params (if `--preset`) overlaid by `call.input`; unknown preset → Usage `no preset "<n>" for <op> (have: a, b); save one with --save-preset` or `no preset "<n>" for <op> (none saved); save one with --save-preset` → Runner with deps, guard, audit, dry-run, yes, user, cancel token, `StdinConfirmer`, and under `-v` a progress printer → `execute`. `RunFailure` kinds: MissingParam → Usage; Cancelled → Interrupted; others → Failed. On success: Text → `render::text`; Json → `jsonutil::pretty(&render::json(..))` + `\n`. `--export F` → write `report::markdown(meta, &with_defaults(op, input), views, Utc::now())`; error → Failed with context `export {F}`; stderr `exported to {F}`. `--save-preset N` → `presets.save(id, N, input.as_map())` (the explicit input, never defaults); stderr `saved preset "{N}" for {id}`.
**Progress lines (stderr):** `→ {step} ...`, `✓ {step} ({d:?})` with `d` rounded to whole milliseconds, `✗ {step}: {err}`, `⊘ {step} (dry-run)`.
**StdinConfirmer:** non-empty preview → stderr `\nAbout to run {op} ({class}) — step "{step}" will change:\n\n{render::text(preview)}\n`. Destructive → prompt `This is DESTRUCTIVE. Type "{resource}" to confirm: `, true iff the trimmed line equals `resource`. Otherwise prompt `Proceed with {op}? [y/N] `, true iff the trimmed, lowercased line is `y` or `yes`. EOF → `Ok(false)`.

- [ ] **Step 1: Write the failing tests**

`confirm.rs` unit tests (`Io::capture` sinks): `yes_answers_proceed` (`"y\n"`, `"YES\n"` → true), `anything_else_declines` (`"n\n"`, `""` → false), `destructive_requires_the_exact_resource` (`"cfg/acme.json\n"` → true; `"cfg\n"` → false; prompt text on stderr), `preview_is_rendered_before_the_prompt` (stderr starts `"\nAbout to run test.write (mutate) — step \"write one\" will change:\n\n"`).

`tests/e2e.rs` (fixtures: `a.json` `{"a":1,"b":[1,2]}`, `b.json` `{"a":2,"b":[1,2,3]}` in root):
```rust
#[tokio::test] async fn run_json_diff_text()            // Ok; stdout contains "diff a.json → b.json" and "~ a: 1 -> 2"
#[tokio::test] async fn run_json_diff_json()            // -o json → stdout parses; views[0].type "summary"; views[1].delta.stats.changed == 1; stdout contains "\"new\": 2" and not "2.0"
#[tokio::test] async fn unknown_output_format_is_usage_before_running() {
    // -o xml → Usage; stderr == "ss run demo.json-diff: unknown output format \"xml\" (want text or json)\n"; stdout empty; no --export file created
}
#[tokio::test] async fn missing_required_param_is_usage() // --left only → Usage; stderr == "ss run demo.json-diff: required param \"right\" missing\n"
#[tokio::test] async fn step_failure_exits_1()           // --left missing.json → Failed; stderr starts "ss run demo.json-diff: read both files: open "
#[tokio::test] async fn dry_run_on_a_read_op_runs_normally() // --dry-run → Ok with the diff output
#[tokio::test] async fn export_writes_markdown()         // --export r.md → file starts "# Diff two JSON files"; contains "`tolerance=0`"; stderr contains "exported to "
#[tokio::test] async fn save_and_replay_preset() {
    // run … --save-preset p → stderr contains "saved preset \"p\" for demo.json-diff"; presets.yaml has no "tolerance"
    // run demo.json-diff --preset p → Ok, same stdout; --preset nope → Usage, stderr == "ss run demo.json-diff: no preset \"nope\" for demo.json-diff (have: p); save one with --save-preset\n"
}
#[tokio::test] async fn env_flows_into_the_operation()   // config dev env; run demo.hello --name x --env dev -o json → "Environment" field "dev"; services.vars SS_ENV=dev without flag → same
#[tokio::test] async fn verbose_prints_progress()        // demo.hello -v → stderr contains "→ compose greeting ..." and "✓ compose greeting ("
#[tokio::test] async fn ctrl_c_mid_run_exits_130()       // run_with_interrupt(demo.hello, sleep(50ms)) → Interrupted; stderr == "ss run demo.hello: compose greeting: cancelled\n"
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ss` → FAIL.

- [ ] **Step 3: Implement** per the spec above.

- [ ] **Step 4: Run to verify pass** — `make rust-check` from the repo root → PASS.

- [ ] **Step 5: Parity check against the frozen reference** (from repo root; no file changes)

```bash
go build -o rust/target/ss-go . && (cd rust && cargo build -q -p ss)
T=internal/diff/testdata
for args in "run demo.hello --name world -o json" \
            "run demo.hello --name world --repeat 2 --shout -o json" \
            "run demo.json-diff --left $T/keyed/a.json --right $T/keyed/b.json -o json" \
            "run demo.json-diff --left $T/basic/a.json --right $T/basic/b.json -o json" \
            "run demo.json-diff --left $T/typechange/a.json --right $T/typechange/b.json -o json"; do
  diff <(rust/target/ss-go $args | jq -S .) <(rust/target/debug/ss $args | jq -S .) && echo "same: $args"
done
```
Expected: five `same:` lines. A difference that is a §4a divergence (e.g. `null` vs `[]`) is recorded, not fixed; any other difference is a bug to fix before committing.

- [ ] **Step 6: Docs.**
  - `rust/CHANGELOG.md` `[Unreleased] ### Added`: operations layer with guard, confirmation, dry-run, audit log; config, presets, cache store; `ss ops list|describe`, `ss run <op>`, `ss presets list|show|delete`; demo operations.
  - `rust/README.md`: `## CLI` with a `flag | overrides | notes` table for every global flag plus `--preset`/`--save-preset`, and one line on exit codes `0/1/2/130`.
  - Master plan §4a, append: empty slices print `[]` where the original printed `null` (`changes`, `rows`, `cols`); audit `time` uses 0/3/6/9 fractional digits; audit `user` comes from `$USER`/`$LOGNAME`; `--save-preset` stores only explicit params (defaults are not frozen in); EOF at a confirmation prompt declines; summary text uses `key : value`; Ctrl-C exits 130 (Review Focus 5 superseded); unknown config/presets keys are rejected.

- [ ] **Step 7: Commit** — `feat(cli): ss run with presets, export, confirmation, interrupt`

**Phase exit criteria:** `make rust-check` green; Step 5 prints five `same:` lines (or only documented §4a differences); every Review Focus test above exists and passes; `grep -rniE '\bgo\b|golang|\bplan\b|\bphases?\b|§|\breview|redliner' rust/crates` prints nothing.

---

### Task 17: Split the Rust workspace into a standalone repo

Added 2026-11-08 at the user's request: the Rust tree becomes the root of a new repository with no Go in it, keeping the Rust commit history. The Go repo stays outside as a read-only reference for porting the remaining operations. Run only after Task 16 (its parity check needs the Go binary beside the Rust tree). Nothing is pushed: the user adds the remote and pushes.

**Files (all inside `rust/`, committed on this branch before the split):**
- Create: `rust/crates/ss-core/tests/fixtures/diff/<case>/{a.json,b.json,expected.txt}` (copied from `internal/diff/testdata`, all 7 cases)
- Modify: `rust/crates/ss-core/tests/diff_golden.rs` (fixtures dir = `CARGO_MANIFEST_DIR/tests/fixtures/diff`)
- Create: `rust/Makefile`, `rust/.gitignore`, `rust/.github/workflows/ci.yml`, `rust/docs/` (copies of `docs/superpowers/plans/*`, `docs/DESIGN.md`, `docs/TESTING_PLAN.md`, `docs/config.example.yaml`)
- Modify: `rust/README.md`, `rust/CHANGELOG.md`

**Decisions:**
- History is carried with `git subtree split --prefix=rust` (built into git; creates a new branch and leaves existing refs untouched). Commits before the `rust/` directory existed are not carried.
- New repo directory: `/home/engineer/Code/starbucks/ss-support-cli-rs` (rename freely).
- Docs under `rust/docs/` may mention the original implementation; code, tests and manifests still may not.

- [ ] **Step 1: Vendor the golden fixtures.** Copy the 7 case directories; point `fixtures_dir()` at `tests/fixtures/diff`; `cargo test -p ss-core --test diff_golden` → 1 passed. `grep -rn 'internal/' rust/crates` → nothing. Commit `test(diff): vendor golden fixtures into the crate`.

- [ ] **Step 2: Root-level files for the standalone repo.**
  - `rust/Makefile`: targets `build` (`cargo build --workspace --release`), `test` (`cargo test --workspace`), `lint` (`cargo fmt --all -- --check` + `cargo clippy --workspace --all-targets -- -D warnings`), `check` (lint + test), `msrv` (`cargo +1.88 check --workspace --all-targets --locked`), `clean`; `## name: help` comments as in the current Makefile.
  - `rust/.gitignore`: `/target/`, `.superpowers/`.
  - `rust/.github/workflows/ci.yml`: the current `rust` and `rust-msrv` jobs with `working-directory`/`workspaces: rust` removed.
  - `rust/README.md`: one-sentence origin note (allowed there), build/install (`cargo install --path crates/ss`), `make check`, the CLI section from Task 16.
  - Copy the docs listed above into `rust/docs/`.
  - Run `cd rust && make check` → green. Commit `chore: standalone repository layout`.

- [ ] **Step 3: Split with history.**
```bash
git subtree split --prefix=rust -b rust-root
git clone --branch rust-root --single-branch . ../ss-support-cli-rs
cd ../ss-support-cli-rs && git branch -m rust-root main && git remote remove origin
```

- [ ] **Step 4: Verify the new repo stands alone.** In `../ss-support-cli-rs`: `make check` → green; `cargo build --release && ./target/release/ss --version` prints the workspace version; `git log --oneline | wc -l` ≈ number of commits that touched `rust/`; `grep -rnE '\.\./\.\./|internal/' crates` → nothing; `ls` shows `Cargo.toml` at the root and no Go files.

- [ ] **Step 5: Hand-off.** Report the new repo path, commit count, and the exact commands for the user: `git remote add origin <url> && git push -u origin main`. Do not push. Leave the `rust-root` branch in the old repo (it is the split's source of truth until the push is confirmed).
