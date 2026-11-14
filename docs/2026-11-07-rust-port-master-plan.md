# ss → Rust Port — Master Plan

> **For agentic workers:** This is the *master* (roadmap) plan. Each phase below is
> executed from its own task-level plan written with `superpowers:writing-plans`
> (TDD, one test cycle per task, frequent commits) and run via
> `superpowers:subagent-driven-development` or `superpowers:executing-plans`.
> Phases 0 and 1 are specified at task granularity here so work can start today.

## Status and re-sequencing (2026-11-12)

This plan is superseded for everything that has not landed by
`docs/2026-11-12-finish-roadmap.md`. The sections below are kept as the record of the
original design; where they disagree with the roadmap, the roadmap wins.

| Phase | Status at 0.1.0-alpha.4 | Remaining work now lives in |
|---|---|---|
| 0, 1, 2 | Landed | — |
| 3, 4 | Landed read-only (S3, DynamoDB, kubectl; no write methods on any port) | Roadmap M3 (port writes) |
| 5 | 8 of 10 real operations landed | Roadmap M3 (`s3.restore-version`, `ddb.item-edit`) |
| 6 | Not started; **moved ahead of the rest of Phase 5** | Roadmap M1 (`docs/2026-11-12-tui-core.md`) and M2 (pods, logs) |
| 7 | Reduced to minimal distribution | Roadmap M4 |

What changed, and who decided it:
- **The TUI comes early** (user, 2026-11-12): it is built next, before the write
  operations. The new order is TUI core → pods and logs screens → write operations →
  distribution.
- **Go parity is dropped** (user, 2026-11-12): the repository is standalone and holds no
  Go source or binary, so the differential harness, the `go-freeze`/`go-final` tags, and
  the Go retirement steps of Phases 5 and 7 are removed. §4a remains as a record of known
  behavior differences, not as input to a normalizer.
- **Kubernetes is read-only** (project rule): the rollout-restart and scale operations of
  Phase 5 and the `rollout_restart`/`scale` methods of the Phase 3 `KubectlPort` sketch are
  out for good. The project ends at ten real operations, not twelve.
- **Distribution is minimal** (user, 2026-11-12): static completions, version stamping, a
  configuration reference, and `cargo install` instructions. The release workflow,
  cross-compiled binaries, and install script of Phase 7 are removed.
- **Phase 6 key map**: the README never gained a "TUI keys" section, so the key list in
  the Phase 6 section below is the source, and the TUI plan adds the README table. The
  picker's `p` key arrives with the pods screen in M2.
- Already done outside their phase: the workspace sits at the repository root, and the
  diff golden fixtures are vendored under `crates/ss-core/tests/fixtures/diff/`.

**Goal:** Reimplement the `ss` support-operations CLI in Rust with feature-for-feature
parity against the Go implementation (P0–P6: 16 operations, headless CLI, TUI, guard,
audit, presets, cache, diff engine), shipping a single static binary.

**Architecture:** Same shape as Go (`docs/DESIGN.md`): front-ends (clap CLI, ratatui TUI)
→ `ops` application layer (Operation / Registry / Runner / View) → domain services (diff,
jsonutil, render, report) → ports (S3, DynamoDB, kubectl) with fakes and AWS adapters.
Async from day one (tokio). Views are a closed `enum`; renderers are `match` visitors.

**Tech stack:** Rust 2024 edition · tokio · clap 4 · ratatui + crossterm · serde /
serde_json · aws-sdk-rust (s3, dynamodb, sts, config) · serde_dynamo · thiserror /
anyhow · insta (snapshots) · tempfile · assert_cmd.

**Reference implementation (behavior):** Go source in this repo (`cmd/`, `internal/`),
`docs/DESIGN.md`, `docs/TESTING_PLAN.md`. The Go binary stays buildable until Phase 7
cutover and is the **parity oracle** for differential tests.

**Reference implementation (CLI style):** `/home/engineer/Code/redliner` — the author's
earlier Go→Rust port. Its command layout, flag naming, help text, output formatting, error
reporting, exit codes, workspace shape, and release process are the house style; §2a
distills them. Where redliner and Go-`ss` disagree, §2a says which wins.

**Prior decisions (brainstorm 2026-10-16, kept unless noted):** async/tokio from day one;
`enum View`; `thiserror` in library modules, `anyhow` + context at CLI/Runner boundary;
unkeyed arrays diffed by LCS over element equality; functional (not byte-for-byte)
parity with fresh Rust tests mirroring Go scenarios.
**Revised here:** operation registration is an explicit builder function, not the
`inventory` crate (see Phase 2 rationale).

---

## 0. Current state (measured 2026-11-07)

| Item | Value |
|---|---|
| Go source | 78 non-test files, 27 test files, 106 `Test*` funcs, ~14k LOC incl. tests |
| Operations | 16: demo×2, s3×4, ddb×4, flow×2, k8s×2 (+ `ops`, `presets`, `tui` commands) |
| TUI | 6 screens, 4 components, theme, screen stack |
| Rust scaffold | `internal/rust/` — 320 LOC, `diff.rs` partial (4 tests pass), placeholder `main.rs` |
| Scaffold git status | **untracked and ignored** (`.gitignore:18 rust/` matches `internal/rust/`) |
| Toolchains | cargo/rustc 1.99.1, Go 1.27.3 |

The memory note says the crate lives in `rust/`; it actually lives in `internal/rust/`.
Phase 0 moves it to `rust/` at repo root and un-ignores it.

---

## 1. Pros / Cons / Risks — the honest version

### Pros
- **Single static binary, no runtime.** musl build, no GC, ~30–50% smaller RSS; startup
  measured in ms. Fits the "one binary, no runtime deps" goal in DESIGN §1.
- **The type system fits this domain.** `View`, `Segment`, `Kind`, `Class`, `StepStatus`
  are all closed unions that Go fakes with sealed interfaces/iota; in Rust they are
  `enum`s with exhaustive `match`, so a new view variant is a compile error in every
  renderer, not a silent fallthrough.
- **Error chains are first-class.** `thiserror` + `#[source]` replaces `%w` wrapping with
  typed variants; `NotFound` becomes a variant, not a sentinel compared with `errors.Is`.
- **Learning payoff.** Ports & adapters, async traits, channel-driven TUI, snapshot
  testing — all the idioms the project will need later are exercised here.

### Cons
- **Volume.** ~10k LOC of production Go to re-express. Rust will land at roughly 1.1–1.3×
  the line count (explicit error types, lifetimes in the TUI, serde derives).
- **Compile times.** aws-sdk-rust alone is minutes on a cold build. Mitigated by a
  2-crate workspace so `diff`/`ops` tests never compile the SDK.
- **TUI ecosystem gap.** Bubble Tea is Elm-architecture; ratatui is immediate-mode with
  no built-in message loop, text input, or spinner. We write a small Elm-style shell
  (Phase 6) rather than adopt a framework that may not survive.
- **Dynamic shell completion.** Cobra's `RegisterFlagCompletionFunc` with live AWS
  suggestions has only a partial clap equivalent (`clap_complete` dynamic mode). Static
  completion ships first; live suggestions are a stretch goal.
- **YAML ecosystem churn.** `serde_yaml` is archived. Resolved by following redliner:
  `serde_yaml_ng` 0.10, pinned in Phase 0.
- **Two codebases during the port.** Bugfixes made in Go during the port must be mirrored.
  Mitigated by: freeze Go features at the start; differential tests catch drift.

### Risks (ranked) and mitigations
| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | **JSON number semantics drift.** Go parses every number as `f64`; `1` == `1.0`, canonical form prints `1`. serde_json keeps `1` (u64) and `1.0` (f64) as *different* values and prints `1.0`. Breaks equality, LCS hashing, and keyed-array identity. | High | High | Phase 1 task: `jsonutil::canonical` normalizes all numbers to f64 and formats them Go-style (`encoding/json` rules: shortest repr, exponent form outside 1e-6..1e21). Scalar compare goes through `as_f64`. Differential fixtures include `1` vs `1.0`, `1e3`, `-0`. |
| R2 | **Safety-model regressions** (env allowlist, Mutate/Destructive confirm, `--yes` never covering Destructive, escalation, fail-closed when no confirmer, audit never masking the run outcome). | Medium | Critical | Port `runner_test.go` scenarios 1:1 in Phase 2 *before* any real adapter exists. Review-Focus tests (§3) pin each rule. No Phase 4/5 merge without them green. |
| R3 | **TUI parity is subjective and large** (~1,700 LOC Go, zero Go tests). | High | Medium | Phase 6 is last and independent; CLI is fully usable before it. Snapshot every screen state with ratatui `TestBackend` + insta. Keep the key map identical (README "TUI keys"). |
| R4 | **AWS adapter edge cases** (delete markers, `InvalidArgument` version-id → NotFound only when a version was sent, 32 MiB body cap, path-style for MinIO, SSO-expired message). | Medium | High | Port the `porttest` contract suite as a generic Rust fn and run it against both Fake and AWS adapters (MinIO/dynalite in CI), exactly as Go does. |
| R5 | **Async trait object safety.** Native `async fn` in traits is not dyn-compatible; the registry and ports need `dyn`. | Certain | Low | Use `async_trait` on the four dyn-dispatched traits (`Operation`, `S3Port`, `DdbPort`, `KubectlPort`) only; native `async fn` everywhere else. One place to change if/when `dyn async` stabilizes. |
| R6 | **Lenient input parsing parity.** Go `Input.Int("x")` returns 0 on garbage; tests and ops (`repeat <= 0 → 1`) rely on it. | Medium | Low | Mirror leniency exactly in Phase 2; add Review-Focus test. Revisit (strict parsing) only *after* cutover as a deliberate behavior change. |
| R7 | **Scope creep / two-codebase drift.** | Medium | Medium | Feature freeze on Go at Phase 0 (tag `go-freeze`). Any Go fix lands with a matching Rust task in the phase backlog. |
| R8 | **Compile-time cost slows TDD.** | Medium | Low | Workspace split (`ss-core` without AWS deps), `profile.dev.package."*".opt-level=3`, `cargo nextest` optional. |

---

## 2. Global constraints

- Edition **2024**; `rust-version = "1.88"` (let-chains). CI runs clippy at MSRV.
- `#![deny(clippy::correctness)]`, warn `clippy::{suspicious,style,complexity,perf}`,
  `unexpected_cfgs`, `missing_docs` on `pub` items in `ss-core`. Configured once in the
  workspace `[workspace.lints]`.
- No `unwrap()` outside tests; `expect()` only for invariants.
- Release profile: `opt-level=3, lto="fat", codegen-units=1, panic="abort", strip=true`.
- Config/preset/audit/cache paths and env vars are **identical** to Go:
  `SS_CONFIG`, `SS_PRESETS`, `SS_ENV`, `SS_PROFILE`, `SS_REGION`, `SS_S3_ENDPOINT`,
  `SS_DDB_ENDPOINT`, `XDG_CONFIG_HOME`/`~/.config/ss/{config,presets}.yaml`,
  `XDG_STATE_HOME`/`~/.local/state/ss/audit.log`, `XDG_CACHE_HOME`/`~/.cache/ss`.
- CLI surface identical: `ss`, `ss tui`, `ss ops list|describe <id>`,
  `ss run <op.id> [--param …] [--preset N] [--save-preset N]`,
  `ss presets list [op]|show <op> <n>|delete <op> <n>`, plus new `ss completion <shell>`; persistent flags
  `--profile --region --env --context -o/--output --export --dry-run --yes -v/--verbose --no-cache`.
- Exit codes per §2a (0/1/2/130); clap usage text printed only for parse errors, never for runtime errors.
- Operation IDs, categories, param names, defaults, and help strings copied verbatim.
- Audit JSONL field names identical (`time,user,env,account,op,params,outcome,duration_ms,error`).
- `-o json` shape identical (`{"views":[{"type":"summary",…}]}`; `Change` has
  `path,jsonpath,kind,old,new`).

---

## 2a. CLI style guide (distilled from redliner; binding for Phases 2 and 7)

**Command layout**
- `main.rs` is a shim: `let args = Cli::parse(); std::process::exit(ss::run(args).await.code())`.
  Everything else lives in the lib (`ss/src/lib.rs` re-exports `run`, `run_with`, `Cli`, `Exit`).
- `cli.rs` holds a clap **derive** tree: `#[derive(Parser)] #[command(name = "ss", version, about)] struct Cli { #[command(subcommand)] command: Option<Command>, #[command(flatten)] global: GlobalFlags }`
  with `enum Command { Tui, Ops(OpsCmd), Run(RunCmd), Presets(PresetsCmd), Completion(..) }`.
  No subcommand = `tui` (Go behavior). `ss run <op>` subcommands are the one place the tree
  is built dynamically (`Command::subcommand(...)` from `Registry::all()`), wrapped so the
  derive tree still owns everything else.
- Pure, unit-testable helpers on the args struct (redliner's `apply_overrides` /
  `config_source` pattern): `GlobalFlags::resolve_env(&self, cfg) -> Environment`,
  `RunCmd::collect_input(&self, op) -> Input`, `RunCmd::output_format() -> Result<Output, String>`.
  Each returns **one clear error string**, never a cascade, and is tested with `Cli::parse_from([...])`.

**Flag naming and help text**
- Long flags are kebab-case; short flags only for the conventional few (`-o`, `-v`, `-n` for
  namespace where Go had it). `value_name` on every typed value: `--output <FORMAT>`,
  `--export <FILE>`, `--preset <NAME>`, `--profile <NAME>`, `--region <REGION>`,
  `--context <KUBE_CONTEXT>`.
- The field doc comment *is* the help: one sentence, states the default in
  `[default: …]` form, names what it overrides (`Override the environment's AWS profile
  (SS_PROFILE)`), and spells out interplay (`--yes auto-confirms Mutate operations; never
  Destructive`). Op param help strings are copied verbatim from Go.
- `ss ops describe` and `ss run <op> --help` must agree: both are generated from `Param`.
- README gets a `## CLI` section with redliner's three-column table: `flag | overrides | notes`.

**Output formatting**
- Results on **stdout**, diagnostics (progress, warnings, "exported to …", "saved preset …")
  on **stderr**, so `ss run … -o json | jq` is clean.
- Summary views render as an aligned colon column: `Object   : cfg/tenants/acme.json`
  (`{key:<w} : {value}`), replacing Go's `Object:   value`. Tables use `format!("{:<w}")`
  columns with a two-space gutter and a dashed rule, as Go does. Both are pinned by insta.
- JSON output is `serde_json::to_string_pretty` + trailing newline; every map that reaches
  output is a `BTreeMap` so ordering is deterministic (params in audit/report included).
- Durations print as Rust `Debug` rounded to micros (`fmt_dur`); timestamps RFC 3339 UTC.
- Warnings are `warning: …` on stderr; progress lines under `-v` keep Go's glyphs
  (`→ ✓ ✗ ⊘`).

**Error reporting**
- Library modules: one `thiserror` enum per module, lowercase messages, `#[source]` on wrapped
  errors. `anyhow` only in `run.rs` / operation bodies, with `.context("<step name>")`.
- The binary prints exactly one line per failure: `ss: {e:#}` for the root command,
  `ss run s3.version-diff: {e:#}` for a subcommand (binary + subcommand prefix, anyhow
  alternate form so the chain reads `fetch both versions: not found: cfg/x@v1`).
- Usage/validation problems are reported *before* anything runs, as a single sentence that
  tells the fix: `no preset "acme" for s3.version-diff (have: dev, prod); save one with
  --save-preset`.

**Exit codes** (redliner's scheme; a deliberate change from Go's uniform `1`)
| code | meaning |
|---|---|
| 0 | operation completed (including a successful `--dry-run`) |
| 1 | operation failed: step error, guard denied, operator declined |
| 2 | usage or config error: bad flag value, unknown operation, unknown `-o` format, unparsable config/presets file |
| 130 | interrupted by Ctrl-C (first Ctrl-C cancels the step and still writes the audit record; a second exits immediately) |
Implemented as `enum Exit { Ok = 0, Failed = 1, Usage = 2, Interrupted = 130 }` with
`run_with_interrupt(args, interrupt: impl Future)` so e2e tests inject the signal.

**Config files**
- `serde_yaml_ng` (what redliner ships). Every config struct is
  `#[serde(deny_unknown_fields, default)]` so a typo in `config.yaml` fails fast with the
  key name — a deliberate tightening over Go's yaml.v3, documented in `docs/configuration.md`
  (new, redliner-style schema reference: one table per section).
- Presets file keeps its exact on-disk shape (`presets: {op: {name: {param: value}}}`) so
  existing files round-trip.

**Testing style**
- CLI parsing: `#[cfg(test)]` beside `cli.rs` using `Cli::parse_from`/`try_parse_from`.
- End-to-end: `tests/e2e.rs` drives `ss::run_with(Cli::parse_from([...]), Services::fakes())`
  **in-process** (no `assert_cmd` subprocess) and asserts on `Exit` plus captured
  stdout/stderr buffers (`run_with` takes `impl Write` sinks). Only the final Phase-7 smoke
  test spawns the real binary.

**Project hygiene**
- Conventional Commits with crate/area scope (`feat(diff): …`, `fix(cli): …`).
- `CHANGELOG.md` (Keep a Changelog) under `rust/` until cutover, then at root.
- Version lives once in `[workspace.package]`; clap `version` reads it. Release flow is
  redliner's: `scripts/release.sh` bumps + dates the changelog, `release.yml` tags on merge,
  builds musl/macOS targets, attaches `.sha256`, and `scripts/install.sh` verifies it.
- Divergence from redliner, on purpose: edition 2024 (not 2021) and `cargo fmt --check` in
  CI. Redliner's no-fmt rule protects a hand-formatted tree from an unreviewable diff; this
  tree is greenfield, so the rule does not apply.

---

## 3. Review focus (cross-phase failure modes the spec implies but no Go test pins)

Each line gets a test in the owning phase.
1. **`--yes` on an operation that calls `Escalate()`** must still prompt for the typed
   resource name and must fail closed with no confirmer. (Phase 2, Runner)
2. **Audit logger write failure** (read-only dir) must not change the run's result or
   exit code. (Phase 2, Runner/audit)
3. **Number formatting in diff output**: `{"a":1}` vs `{"a":1.0}` → no changes;
   `{"a":1e21}` renders Go-style `1e+21`. (Phase 1, jsonutil/diff)
4. **Corrupt cache entry** on disk must fall through to a network fetch and overwrite.
   (Phase 3, Cached)
5. **Ctrl-C mid-operation** in CLI cancels the step with a `cancelled` error, writes an
   audit record with outcome `error`, and exits 1. (Phase 2 runner + Phase 7 CLI e2e)

---

## 4. Phase map

| Phase | Deliverable (always a working binary) | Depends on | Size |
|---|---|---|---|
| 0 | Workspace, CI, lints, crate moved to `rust/`, Go feature freeze | — | S |
| 1 | Diff engine complete + shared golden fixtures + Go-parity canonical JSON | 0 | M |
| 2 | ops core, Runner safety model, config, guard, audit, cache, presets, fuzzy, render/report, demo ops, headless `ss run/ops/presets` | 1 | L |
| 3 | Port traits, fakes, Lazy/Cached decorators, kubectl exec adapter, contract suite | 2 | M |
| 4 | AWS S3 + DynamoDB adapters, STS guardrail, env-gated integration + CI contract job | 3 | M |
| 5 | The 12 real operations + differential tests vs Go binary | 3, 4 | L |
| 6 | TUI (screens, components, async run bridge) + snapshot tests | 2 (5 for S3 browse/pods) | XL |
| 7 | Cutover: completions, release build, docs, Go retirement | all | M |

Phases 4 and 6 can run in parallel with 5 once 3 lands.

---

## Phase 0 — Foundation (task-level)

**Files**
- Move: `internal/rust/**` → `rust/` (git `mv` after un-ignoring)
- Create: `rust/Cargo.toml` (workspace), `rust/crates/ss-core/Cargo.toml`,
  `rust/crates/ss/Cargo.toml`, `rust/rustfmt.toml`, `rust/clippy.toml`,
  `rust/.cargo/config.toml` (none needed yet; placeholder omitted), `rust/README.md`
- Modify: `.gitignore` (drop `rust/`, add `rust/target/`), `.github/workflows/ci.yml`
  (add `rust` job), `Makefile` (`rust-build rust-test rust-check`)

**Workspace layout**
```
rust/
├── Cargo.toml                 # [workspace] members, [workspace.dependencies], [workspace.lints]
└── crates/
    ├── ss-core/               # lib: diff, jsonutil, config, guard, audit, cache, preset,
    │                          #      fuzzy, ops (+ all operations), svc traits + fakes,
    │                          #      kubectl exec adapter, render, report
    └── ss/                    # bin: clap CLI, tui, aws adapters (s3, ddb, sts/awsx)
```
Rationale: `ss-core` never links aws-sdk, so the TDD loop for the diff engine, Runner,
and operations stays in seconds. Operations depend on port *traits*, not adapters.

### Task 0.1: Un-ignore and relocate the crate
- [ ] Edit `.gitignore`: remove line `rust/`; add `rust/target/`.
- [ ] `git mv`-equivalent: `mkdir -p rust/crates && mv internal/rust rust/crates/ss-core`
      (it is untracked, so a plain move), delete `rust/crates/ss-core/target`.
- [ ] Verify: `git status --short | grep rust/` lists the new files as untracked (not ignored).
- [ ] Commit: `chore(rust): relocate scaffold to rust/crates/ss-core and track it`

### Task 0.2: Workspace, lints, profiles
- [ ] Write `rust/Cargo.toml` with `[workspace] resolver = "3"`, members
      `crates/ss-core`, `crates/ss`; `[workspace.package] edition="2024" rust-version="1.88"`;
      `[workspace.dependencies]` pinning: `tokio = { version = "1", features = ["rt-multi-thread","macros","sync","process","io-util","signal","time","fs"] }`,
      `serde`, `serde_json` (**no** `preserve_order`), `thiserror = "2"`, `anyhow = "1"`,
      `async-trait = "0.1"`, `serde_yaml_ng = "0.10"`, `clap = { version = "4", features = ["derive"] }`,
      `tracing`, `insta`, `tempfile`.
- [ ] `[workspace.lints.clippy]`: `correctness = "deny"`, `suspicious/style/complexity/perf = "warn"`;
      `[workspace.lints.rust] unexpected_cfgs = "warn"`.
- [ ] Release profile per Global Constraints; `[profile.dev.package."*"] opt-level = 3`.
- [ ] `ss-core/Cargo.toml`: `[lib] name = "ss_core"`, `lints.workspace = true`.
      Remove the `[[bin]]`; delete placeholder `main.rs`.
- [ ] `ss/Cargo.toml`: `[[bin]] name = "ss"`, `version.workspace = true`, depends on
      `ss-core`, `clap`, `tokio`. `src/main.rs` is the redliner shim
      (`Cli::parse()` → `ss::run(args).await` → `std::process::exit(code)`); `src/lib.rs`
      exposes `pub async fn run(Cli) -> Exit`, `pub async fn run_with(Cli, Services, &mut dyn Write, &mut dyn Write) -> Exit`,
      and `pub enum Exit` per §2a. `ss --version` prints the workspace version.
- [ ] Verify: `cd rust && cargo build && cargo test -p ss-core` → 4 diff tests pass.
- [ ] Commit: `chore(rust): workspace, lints, release profile`

### Task 0.3: CI and Makefile
- [ ] Add `rust` and `rust-msrv` jobs to `.github/workflows/ci.yml` mirroring redliner's
      `ci.yml` (`dtolnay/rust-toolchain@stable` with clippy, `Swatinem/rust-cache`,
      `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`;
      separate `cargo +1.88 check --workspace --all-targets --locked`) plus `cargo fmt --check`.
- [ ] Add `rust/CHANGELOG.md` (Keep a Changelog, `## [Unreleased]`) and
      `rust/rust-toolchain.toml` (`channel = "stable"`).
- [ ] Makefile targets `rust-build`, `rust-test`, `rust-check` (fmt + clippy + test),
      all `cd rust && …`. `make check` stays Go-only until Phase 7.
- [ ] Verify locally: `make rust-check` passes.
- [ ] Commit: `ci(rust): fmt, clippy, test, msrv`

### Task 0.4: Freeze the Go reference
- [ ] `git tag go-freeze` on current `main` HEAD (after committing the stray `.gitignore`
      change and deleting the three `*.NNNN` editor-backup files listed in `git status`).
- [ ] Add `docs/superpowers/plans/README.md` one-liner: "Go is frozen at `go-freeze`;
      Go fixes after this tag need a matching Rust task."

**Exit criteria:** `make rust-check` green in CI; `rust/` tracked; Go unchanged and green.

---

## Phase 1 — Diff engine (task-level)

**Files (all in `rust/crates/ss-core/src/`)**
- Create: `jsonutil.rs`, `diff/mod.rs`, `diff/path.rs`, `diff/glob.rs`, `diff/opts.rs`,
  `diff/stats.rs`, `diff/json.rs`, `diff/text.rs`, `diff/lcs.rs`
- Replace: existing `diff.rs` (split into the files above; keep its 4 tests)
- Test: unit tests per module; `tests/diff_golden.rs` integration test reading
  `../../../internal/diff/testdata/<case>/{a,b}.json,expected.txt` (the **same fixtures Go
  uses**), rendering via the Phase-2 text renderer's change-line format (temporarily a
  local `render_expected()` in the test until Phase 2 lands).

**Interfaces (produced; later phases consume verbatim)**
```rust
// diff/path.rs
pub enum Segment { Key(String), Index(usize), MatchKey { field: String, value: Value } }
pub struct Path(Vec<Segment>);
impl Path { pub fn child(&self, s: Segment) -> Path; pub fn dotted(&self) -> String; pub fn jsonpath(&self) -> String; }
impl Display for Path /* dotted */

// diff/glob.rs
pub struct PathGlob { raw: String, segs: Vec<GlobSeg> }
impl PathGlob { pub fn compile(pattern: &str) -> Result<Self, GlobError>; pub fn matches(&self, p: &Path) -> bool; pub fn raw(&self) -> &str; }
pub fn compile_globs(patterns: &[String]) -> (Vec<PathGlob>, Vec<GlobError>)  // invalid skipped, reported

// diff/opts.rs
#[derive(Default, Clone)] pub struct Opts { pub ignore: Vec<String>, pub array_match: BTreeMap<String,String>, pub tolerance: f64, pub treat_null_as_absent: bool }

// diff/json.rs
pub enum Kind { Added, Removed, Changed, TypeChanged }   // Display: added|removed|changed|type-changed
pub struct Change { pub path: Path, pub kind: Kind, pub old: Option<Value>, pub new: Option<Value> }
impl Serialize for Change  // {"path","jsonpath","kind","old"?,"new"?}
pub struct Delta { pub changes: Vec<Change>, pub stats: Stats }
pub fn json(a: &Value, b: &Value, opts: &Opts) -> Delta   // sorted by dotted path

// diff/stats.rs
pub struct Stats { pub added: usize, pub removed: usize, pub changed: usize, pub type_changed: usize }
impl Stats { pub fn total(&self)->usize; pub fn is_empty(&self)->bool }  // Display: "+2 −1 ~3 !1" | "no changes"

// diff/text.rs
pub fn text(a: &str, b: &str) -> String   // "-"/"+"/" " prefixed lines, no hunk headers

// jsonutil.rs
pub fn parse(bytes: &[u8]) -> Result<Value, JsonError>    // strips BOM + whitespace; empty → error; numbers normalized to f64
pub fn canonical(v: &Value) -> String                      // sorted keys, Go-style number formatting
pub fn pretty(v: &Value) -> String; pub fn compact(v: &Value) -> String
pub fn redacted(v: &Value, fields: &[&str]) -> Value       // case-insensitive, "«redacted»"
pub fn json_type(v: &Value) -> &'static str                // null|bool|number|string|object|array
```

### Task 1.1: Go-compatible number handling (R1)
- [ ] Tests in `jsonutil.rs`: `parse(b"1").unwrap() == parse(b"1.0").unwrap()`;
      `canonical(&json!(1.0)) == "1"`; `canonical(&json!(1e21)) == "1e+21"`;
      `canonical(&json!(0.000001)) == "0.000001"`; `canonical(&json!(1e-7)) == "1e-7"` (encoding/json strips strconv's zero-pad on negative exponents);
      `canonical(&json!({"b":1,"a":[2,{"d":3,"c":4}]})) == r#"{"a":[2,{"c":4,"d":3}],"b":1}"#`.
- [ ] Run → fail (module missing).
- [ ] Implement `parse` with a post-pass converting every `Number` to `Number::from_f64`;
      implement `fmt_number(f64) -> String` mirroring Go `encoding/json`: `%g`-style
      shortest repr (use `ryu`), switch to exponent when `abs < 1e-6 || abs >= 1e21`,
      positive exponents printed with sign and two digits (`1e+21`), negative ones unpadded (`1e-7`).
- [ ] Run → pass. Commit `feat(jsonutil): Go-compatible parse/canonical`.

### Task 1.2: Path, Segment, rendering
- [ ] Tests: dotted `a.b[0].c`, `rules[id=r1].limit`; jsonpath `$.a.b[0].c`,
      `$.rules[?(@.id=="r1")]`; root path renders `""` / `"$"`.
- [ ] Implement `diff/path.rs` (MatchKey value rendered via `compact`, string values unquoted
      in dotted form to match Go `%v`).
- [ ] Commit.

### Task 1.3: PathGlob
- [ ] Tests ported from `glob_test.go`: exact keys, `*`, `**` (zero or more), `foo[*]`,
      `foo[2]`, leading `$.` ignored, `[*]` matches `MatchKey` segments, errors for
      `""`, `a..b`, `a[`, `a[x]`.
- [ ] Implement recursive matcher as in Go (`match_segs`).
- [ ] Commit.

### Task 1.4: Object + scalar diff with options
- [ ] Tests: added/removed/changed; `TypeChanged` for `"5"`→`5`; null↔value is `Changed`
      not `TypeChanged`; `tolerance` within/outside epsilon; `treat_null_as_absent`
      makes `{"a":null}` == `{}`; ignore globs suppress both leaf and subtree.
- [ ] Implement `walk`/`object`/`scalar` in `diff/json.rs`; scalar equality via `as_f64`
      for numbers.
- [ ] Commit.

### Task 1.5: Arrays — LCS and keyed
- [ ] Tests: pure insert at index 0 → one `Added`; delete → one `Removed`; edited element
      between matches → nested `Changed` at `[i]`; keyed `$.rules[*]: id` → `rules[id=r2].limit`
      changed, unkeyed leftovers positional, duplicate ids keep first; array vs non-array
      → `TypeChanged`.
- [ ] Implement `diff/lcs.rs::pairs(a: &[String], b: &[String]) -> Vec<(usize,usize)>`
      (classic DP, tie-break `dp[i+1][j] >= dp[i][j+1]` exactly as Go) and
      `lcs_array`/`keyed_array` using `canonical` as the element hash.
- [ ] Commit.

### Task 1.6: Stats, sort, Serialize, text diff
- [ ] Tests: `Stats` Display cases; changes sorted by dotted path (stable); `Change`
      JSON omits `old`/`new` when `None` **or JSON null** (Go `omitempty` on `any`); `text("a\nb\nc","a\nc\nd") == "...”` matching Go.
- [ ] Implement; commit.

### Task 1.7: Shared golden fixtures
- [ ] `tests/diff_golden.rs`: for each dir in `internal/diff/testdata`, parse `a.json`,
      `b.json`, options from the Go test table (basic/ignore/keyed/lcsinsert/nullabsent/
      tolerance/typechange — copy the `Opts` per case from `json_test.go`), render
      `- p = v` / `+ p = v` / `~ p: o -> n` / `! p: o -> n (type changed)` / `stats: …`,
      compare to `expected.txt` with the same normalization Go's `golden.normalize` uses.
- [ ] All 7 pass. Commit `test(diff): share Go golden fixtures`.

**Exit criteria:** `cargo test -p ss-core` green; the 7 Go fixtures pass unchanged;
`clippy -D warnings` clean.

---

## Phase 2 — ops core, safety model, headless CLI (phase-level + interfaces)

**Modules (`ss-core`)**: `config/{mod,trace}.rs`, `guard.rs`, `audit.rs`, `cache/{mod,dir}.rs`,
`preset.rs`, `fuzzy.rs`, `ops/{mod,operation,view,registry,runner,confirm,input}.rs`,
`ops/demo/{hello,jsondiff}.rs`, `render.rs`, `report.rs`.
**Modules (`ss` bin)**: `cli/{mod,root,run,ops_cmd,presets_cmd}.rs`, `cli/confirm.rs`.

**Pinned interfaces**
```rust
// guard.rs
pub enum Class { Read, Mutate, Destructive }           // Display: read|mutate|destructive
#[derive(thiserror::Error)] pub enum GuardError { #[error("environment is not in the configured allowlist")] EnvNotAllowed{op:String, env:String}, #[error("operation not confirmed")] NotConfirmed{op:String} }
pub trait Guard: Send + Sync { fn authorize(&self, req: &Request) -> Result<(), GuardError>; }
pub struct AllowAll; pub struct EnvAllowlist { pub name: String, pub allowed: bool }

// audit.rs
#[derive(Serialize)] pub struct Entry { time: DateTime<Utc>, user, env, #[serde(skip_serializing_if="String::is_empty")] account, #[serde(rename="op")] op_id, params: BTreeMap<String,String>, outcome, duration_ms: i64, error: Option<String> }
pub trait Logger: Send + Sync { fn record(&self, e: &Entry) -> std::io::Result<()>; }   // Nop, FileLogger(path, Mutex)

// ops/operation.rs
pub struct Meta { pub id: &'static str, pub name: &'static str, pub summary: &'static str, pub category: &'static str, pub class: Class }
pub enum ParamType { String, Int, Bool, Enum, Duration, Json }
pub struct Choice { pub value: String, pub label: Option<String> }
pub type SuggestFn = Arc<dyn Fn(Deps, Input) -> BoxFuture<'static, anyhow::Result<Vec<Choice>>> + Send + Sync>;
pub struct Param { pub name: &'static str, pub label: Option<&'static str>, pub ty: ParamType, pub required: bool, pub default: Option<&'static str>, pub help: Option<&'static str>, pub suggest: Option<SuggestFn> }
#[async_trait] pub trait Operation: Send + Sync { fn meta(&self) -> Meta; fn params(&self) -> Vec<Param>; async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult>; }

// ops/input.rs — Go leniency preserved (R6)
pub struct Input(BTreeMap<String,String>);
impl Input { pub fn str(&self,k)->&str; pub fn int(&self,k)->i64 /*0 on parse fail*/; pub fn bool(&self,k)->bool; pub fn duration(&self,k)->Duration; pub fn globs(&self,k)->Vec<String> /*JSON array or CSV*/; pub fn json(&self,k)->Option<Value>; }

// ops/view.rs
pub enum View { Summary{fields: Vec<(String,String)>}, Diff{left:String,right:String,delta:Delta}, Table{title:String,cols:Vec<String>,rows:Vec<Vec<String>>}, Doc{title:String,body:String,lang:String}, Tree{title:String,root:Node} }
pub struct OpResult { pub views: Vec<View> }

// ops/registry.rs — explicit, no inventory
pub struct Registry { by_id: BTreeMap<&'static str, Arc<dyn Operation>> }
impl Registry { pub fn new()->Self; pub fn register(&mut self, op: Arc<dyn Operation>) -> Result<(), RegistryError> /*empty or dup id*/; pub fn get(&self,id)->Option<Arc<dyn Operation>>; pub fn all(&self)->Vec<Arc<dyn Operation>> /*category, id*/; pub fn categories(&self)->Vec<&str>; }
pub fn builtin() -> Registry   // registers demo + (later) s3, ddb, flow, k8s

// ops/runner.rs
pub enum StepStatus { Running, Done, Failed, Skipped }
pub struct Progress { pub step: String, pub status: StepStatus, pub elapsed: Duration, pub err: Option<String> }
#[async_trait] pub trait Confirmer: Send + Sync { async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool>; }
pub struct ConfirmSpec { pub op_id: String, pub class: Class, pub resource: String, pub step: String, pub preview: Vec<View> }
pub struct Runner { pub deps: Deps, pub dry_run: bool, /* guard, auditor, progress: Option<Box<dyn Fn(Progress)+Send+Sync>>, confirmer: Option<Arc<dyn Confirmer>>, yes, user, per-run state */ }
impl Runner {
  pub fn new(deps: Deps) -> Self;  // builder: .guard(..) .audit(..) .progress(..) .dry_run(b) .yes(b) .user(s) .confirmer(..)
  pub async fn execute(&mut self, op: &dyn Operation, input: Input) -> anyhow::Result<OpResult>;  // defaults, required check, guard, run, audit, dry-run/declined view injection
  pub async fn step<F,Fut,T>(&mut self, name: &str, f: F) -> anyhow::Result<T> where F: FnOnce(CancellationToken)->Fut, Fut: Future<Output=anyhow::Result<T>>;
  pub async fn mutate<F,Fut,T>(&mut self, name: &str, f: F) -> anyhow::Result<T>;  // dry-run → Err(DryRun); first call confirms
  pub fn preview(&mut self, views: impl IntoIterator<Item=View>); pub fn set_resource(&mut self, s: impl Into<String>); pub fn escalate(&mut self);
}
#[derive(thiserror::Error)] pub enum RunnerError { #[error("dry-run: stopped before mutating step")] DryRun, #[error("{0}: no confirmer available")] NoConfirmer(String) }

// config/mod.rs — serde + yaml
pub struct File { default_env: String, environments: BTreeMap<String,EnvSpec>, diff: DiffDefaults, trace: TraceConfig }
pub fn default_path() -> PathBuf; pub fn load(path:&Path)->Result<File,ConfigError> /*missing → default*/; pub fn resolve(flag:Option<&str>, env_key:&str, file_val:&str, default:&str)->String;
impl File { pub fn env(&self, name:&str, profile:Option<&str>, region:Option<&str>) -> Environment }
// config/trace.rs: TraceDomain { with_defaults, resolve_table, resolve_bucket, build_sort(vars) -> Result<String, TraceError> }

// cache: trait Store { get(&str)->Option<Vec<u8>>; put(&str,&[u8])->io::Result<()> }  Nop, Dir(sha256 2-level fanout, tmp+rename)
// preset: Store { get/list/all/save/delete } — atomic write, empty values dropped, last-preset removal deletes op section
// fuzzy: pub fn score(pattern,&str)->Option<i32>; pub fn rank(pattern,&[&str])->Vec<(usize,i32)>  (port Go scoring exactly)
// render: pub fn text(&OpResult)->String; pub fn json(&OpResult)->serde_json::Value
// report: pub fn markdown(meta:&Meta, input:&Input, res:&OpResult, now: DateTime<Utc>) -> String
```

**Registration decision (revises brainstorm):** `ops::builtin()` explicitly constructs
the registry. Pros: no link-time magic, each test builds its own `Registry`, dead-code
elimination still works, ordering deterministic. Con: adding an op means editing one
list — which Go also required (blank imports in `cmd/root.go`). `inventory` buys nothing
here.

**Tests to port (scenario parity):** `runner_test.go` (16 scenarios incl. required-param
error, guard deny → audit `denied`, dry-run view injection, declined returns views + error,
confirmer hard error, `--yes` covers Mutate only, escalate ignores `--yes`, no confirmer
fails closed, Read ops not audited, step error wrapping `"<step>: <err>"`), `confirm_test.go`,
`registry_test.go`, `config_test.go`, `preset_test.go`, `fuzzy_test.go`, `jsonutil_test.go`,
`report/markdown_test.go`, plus Review-Focus 1, 2, 5. CLI: `cli.rs` unit tests with
`Cli::parse_from`; `tests/e2e.rs` in-process via `run_with(..)` for `ss ops list`,
`ss ops describe demo.hello`, `ss run demo.json-diff --left --right`, `-o json`,
`--export`, `--preset/--save-preset`, `presets list/show/delete`, unknown format → exit 2
with the one-line `ss run demo.json-diff: unknown output format "xml" (want text or json)`,
missing required param → exit 2, step failure → exit 1, dry-run → exit 0.

**CLI build (per §2a):** derive tree in `cli.rs` (`Cli`, `GlobalFlags`, `Command`,
`RunCmd`, `OpsCmd`, `PresetsCmd`); `cli/dynamic.rs` augments `run` with one clap
`Command` per registered op and one `Arg` per param (`Bool` → `ArgAction::SetTrue`, else
`String` with `value_name` from `ParamType`; defaults applied by Runner not clap; required
validated by Runner after preset merge — same reasoning as Go). `run.rs` is the pipeline
(resolve config → build `Services` → dispatch subcommand → map errors to `Exit`), the
single place `anyhow` is turned into an exit code and the `ss <sub>: {e:#}` line.
`cli/confirm.rs`: stdin confirmer (y/yes; typed resource name for Destructive) with
injectable `Read`/`Write`. Signals: `run_with_interrupt` takes the interrupt future;
production passes redliner's two-stage `ctrl_c()` (first cancels via `CancellationToken`,
second exits 130 immediately).

**Exit criteria:** `ss run demo.hello --name world -o json | diff <(./ss run … -o json)`
identical after key-sorting; all ported scenarios green; safety tests green.

---

## Phase 3 — Ports, fakes, decorators, kubectl adapter

**Modules (`ss-core`)**: `svc/s3/{mod,port,fake,lazy,cached,contract}.rs`,
`svc/ddb/{mod,port,fake,lazy}.rs`, `svc/kubectl/{mod,port,exec,fake,pods}.rs`.

```rust
#[derive(thiserror::Error)] pub enum SvcError { #[error("not found: {0}")] NotFound(String), #[error("{0}")] Other(#[from] anyhow::Error) }
#[async_trait] pub trait S3Port: Send+Sync { async fn list_buckets(&self)->Result<Vec<Bucket>,SvcError>; async fn list(&self,bucket,prefix,token:Option<String>)->Result<Page,SvcError>; async fn list_versions(&self,bucket,key)->Result<Vec<Version>,SvcError>; async fn get_version(&self,bucket,key,version_id)->Result<Object,SvcError>; async fn head(&self,bucket,key)->Result<ObjectMeta,SvcError>; async fn put(&self,bucket,key,body:Bytes)->Result<String,SvcError>; }
impl Object { pub fn version_label(&self)->String }   // "current" | 8-char id | "id (YYYY-MM-DD HH:MM)"
#[async_trait] pub trait DdbPort { list_tables, describe_table, get_item, query, query_index, put_item }   // Item = serde_json::Map; Key = Map
#[async_trait] pub trait KubectlPort { get_json, list_json, logs(ns,pod,follow)->Result<mpsc::Receiver<String>,SvcError>, rollout_restart, scale }
pub struct Lazy<P>(tokio::sync::OnceCell<Result<Arc<P>,…>>, builder)   // one per port via macro or generic
pub struct Cached<P: S3Port> { inner: P, store: Arc<dyn Store> }       // only explicit version ids cached; entry = {meta,version_id,body}
pub async fn s3_contract<P: S3Port>(fx: Fixture<P>)                      // port of porttest; run vs Fake in unit tests, vs AWS adapter env-gated
pub fn parse_pods(raw:&[u8])->Result<Vec<Pod>,SvcError>; pub fn configmap_data(raw:&[u8])->Result<BTreeMap<String,String>,SvcError>
```
kubectl exec: `tokio::process::Command` with `--context`/`-n` args; stderr `notfound`/`not found`
→ `NotFound`; logs via `BufReader::lines()` into a bounded `mpsc(64)`; kill on cancel.
Tests stub the binary with a temp shell script, as Go does.

**Exit criteria:** contract suite green against `FakeS3`; Review-Focus 4 (corrupt cache
entry) green; kubectl exec tests (stub binary) green.

---

## Phase 4 — AWS adapters (`ss` bin crate, `aws/` module)

`aws/awsx.rs`: `load(profile, region) -> SdkConfig` via `aws_config::defaults(BehaviorVersion::latest()).profile_name(..).region(..)`;
`check_identity` via `aws_sdk_sts::get_caller_identity`; `friendly()` maps SSO-expired
shapes to `SSO session expired — run \`aws sso login --profile {p}\``; account-mismatch error
text copied from Go. `aws/s3.rs`: `AwsS3 { client, max_body: 32 MiB, page_size }`,
`with_endpoint` sets `endpoint_url` + `force_path_style(true)`; `ListObjectVersions` drain
with key filter and delete markers; stable sort by `last_modified` desc; `normalize()` maps
`NoSuchKey/NoSuchBucket/NoSuchVersion/NotFound` and HTTP 404 → `NotFound`;
`normalize_version_lookup()` adds `InvalidArgument` + "version id" **only when a version
id was sent**. `aws/ddb.rs`: `serde_dynamo` for `Item ↔ AttributeValue`;
`ConsistentRead(true)` on GetItem; `ResourceNotFoundException` → `NotFound`.

Tests: env-gated `SS_S3_TEST_ENDPOINT` / `SS_DDB_TEST_ENDPOINT` integration tests running
the Phase-3 contract suite; CI `contract` job extended to run the Rust suite against the
same MinIO container. Port `hack/seed-*` to Rust later only if needed (the Go seeders
remain usable until cutover).

**Exit criteria:** contract suite green vs MinIO in CI; `ss run s3.version-diff` (Phase 5)
against MinIO matches Go output.

---

## Phase 5 — The twelve real operations

`ops/s3/{suggest,version_diff,inspect,prefix_audit,restore}.rs`,
`ops/ddb/{common,item_compare,item_history,gsi_consistency,item_edit}.rs`,
`ops/flow/{trace,drift}.rs`, `ops/k8s/deploy.rs`.

Helpers pinned: `parse_key(spec, &TableInfo) -> Result<Key>` (JSON object or `k=v,k=v`,
coerce to `N`/`S`), `version_num("V12") -> Option<u32>`, `sort_by_version`,
`compact_preview(item, skip, max)`, `human_size`, `age`, `short_id` (12 chars + `…`),
`split_s3_uri`, `split_configmap_spec`, `previous_version_id`, `configmap_delta`.
Parallel fetches use `tokio::try_join!` where Go used `errgroup`.

Each op gets fake-backed tests mirroring its Go test file (`s3ops_test.go`,
`restore_test.go`, `pagination_test.go`, `ddbops_test.go`, `itemedit_test.go`,
`flowops_test.go`, `deploy_test.go`), including: restore no-op when target is current,
item-edit refuses key attributes, scale-to-zero escalates, prefix-audit truncation at
`--max`, GSI `MISSING` verdict, trace with/without previous version and with ConfigMap drift.

**Differential harness** (`rust/xtask/parity.sh`): seed MinIO + dynalite with the Go
seeders, run both binaries over a matrix of `ss run … -o json` invocations, normalize
(`jq -S`, drop `Generated` timestamps), diff. Added to `make rust-parity` (manual, not CI).

**Exit criteria:** `ss ops list` identical to Go; parity script reports zero diffs across
the matrix; all op tests green.

---

## Phase 6 — TUI

Crate `ss` module `tui/`: `app.rs` (screen stack, status bars, `fit_height`, ctrl+c),
`screen.rs` (`trait Screen { fn init(&mut self)->Vec<Cmd>; fn update(&mut self, Msg)->Vec<Cmd>; fn view(&self, Frame, Rect); fn title(&self)->String; fn hints(&self)->String }`),
`msg.rs` (`enum Msg { Key(KeyEvent), Resize, Push(Box<dyn Screen>), Pop, Status(String), Progress(..), RunDone(..), Confirm(..), Suggest(..), S3Loaded(..), PodsLoaded(..), LogLine(..) }`),
`theme.rs`, `widgets/text_input.rs` (minimal, since bubbles has no ratatui twin),
`components/{diffviewer,progresslist,errorpanel,confirm}.rs`,
`screens/{opspicker,opsform,opsresult,s3browse,k8spods,logstail}.rs`.

Event loop: crossterm `EventStream` + an `mpsc<Msg>` for async results, `tokio::select!`
in the main loop; no operation runs inside `update`. The run bridge mirrors Go: Runner
executes in `tokio::spawn`, progress via `mpsc`, confirmation via `oneshot` parked in the
`Confirmer`. Key map copied from README "TUI keys" verbatim (picker `b/p///q`, form
`tab/↑↓/enter/ctrl+p/ctrl+s/esc`, result `tab/e/ctrl+s/esc`, diff `n/p/s/c///g/pgup/pgdown`,
browse `enter/space/d/r/esc`).

Tests: drive `update` with synthetic `KeyEvent`s; render into `ratatui::backend::TestBackend`
at 80×24; `insta` snapshots (plain text, so no ANSI stripping needed). Scenarios from
`confirm_test.go`, the Go TESTING_PLAN T1 list (confirm modal y/n, typed name match/mismatch),
picker filter, form required-field error, preset overlay, result tab cycling, diffviewer
modes, s3browse mark A/B → pushes result screen for `s3.version-diff`.

**Exit criteria:** every screen snapshotted in each state; manual walkthrough of the README
key table against MinIO/dynalite; no `unwrap` in TUI code.

---

## Phase 7 — Cutover and distribution

- Shell completions: `clap_complete` static for bash/zsh/fish (`ss completion <shell>`);
  dynamic value completion for params with `suggest` if `clap_complete` dynamic mode is
  stable at the time — otherwise documented gap.
- Version stamping: `build.rs` emitting `git describe --tags --always --dirty` into
  `SS_VERSION` (fallback `CARGO_PKG_VERSION`).
- Release: copy redliner's `release.yml`, `scripts/release.sh`, `scripts/changelog-section.sh`,
  `scripts/install.sh` (+ their `test-*.sh`), retargeted to `ss`. Targets
  `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`,
  `aarch64-apple-darwin`; `.sha256` per asset; 0.x as pre-releases. Replaces Go P7
  (goreleaser/Homebrew). `make release-{patch,minor,major}` targets as in redliner.
- `docs/configuration.md`: schema reference for `config.yaml` and `presets.yaml`
  (one table per section, redliner layout); README `## CLI` flag table and exit-code line.
- Move workspace to repo root (`Cargo.toml` at `/`), `make check` → Rust, Go sources
  deleted in one commit after tag `go-final`; `docs/DESIGN.md` rev 4 documents the Rust
  package layout; README quickstart updated (`cargo install --path .` / release assets).
- Review-Focus 5 e2e (ctrl-c) under `assert_cmd` with a slow fake op.
- **Relocate the shared diff fixtures.** `rust/crates/ss-core/tests/diff_golden.rs` reads
  `internal/diff/testdata` from the Go tree; move them to `rust/crates/ss-core/tests/fixtures/diff/`
  in the same commit that deletes the Go sources, or the test silently loses its oracle.

**Exit criteria:** release binary passes the parity matrix and the manual TUI walkthrough;
Go removed; CI green on Rust only.

---

## 4a. Known Go↔Rust divergences (feed the Phase 5 differential normalizer)

Deliberate or accepted; each is a line the parity script must canonicalize, not a bug to fix.
- Go `json.Marshal` HTML-escapes `<`, `>`, `&` as `\u003c` etc.; Rust does not.
- Dotted `MatchKey` values: Rust renders compact JSON (`rules[id=1234567]`, `[id=null]`);
  Go's `%v` renders `1.234567e+06` for numbers ≥ 1e6 and `<nil>` for null.
- Go `json.Unmarshal` accepts invalid UTF-8 inside strings (replaced with U+FFFD); `serde_json`
  rejects the document, so such bodies fall to the text diff in Rust.
- Two `array_match` rules matching the same array: Rust picks deterministically (sorted
  pattern order); Go's choice depends on map iteration.
- Empty slices print `[]` where Go printed `null` (`changes`, `rows`, `cols` in `-o json`).
- `-o json` object keys come out sorted in Rust; Go used struct field order (`jq -S` canonicalizes).
- Audit `time` uses 0/3/6/9 fractional-second digits; Go's RFC3339Nano trims trailing zeros
  digit by digit.
- Audit `user` comes from `$USER`/`$LOGNAME`; Go asked the OS (`user.Current()`).
- `--save-preset` stores only the explicit params (preset + flags); defaults are not frozen in,
  so later default changes still apply on replay.
- EOF at a confirmation prompt declines (audit outcome `declined`); Go returned the read error
  (outcome `error`). Both exit 1.
- Summary text uses `key : value` (colon-aligned).
- Ctrl-C exits 130 (Review Focus 5's exit 1 superseded by §2a); the audit outcome is still
  `error`. Ctrl-C while the confirmation prompt is waiting cancels the run at once.
- Unknown keys in `config.yaml` / `presets.yaml` are rejected (exit 2) instead of ignored.
- `demo.json-diff` with two byte-identical invalid-JSON files shows the line diff; Go fell
  through to an empty JSON diff.
- A closed stdout (`ss ops list | head -0`) exits 0 silently.
- kubectl is read-only: there is no class escalation anywhere (Review Focus 1 dropped).
- `-o` is validated before the operation runs (unknown format exits 2 with nothing run or
  exported); Go ran the operation first and failed at render time.
- Unknown `--preset` message: `no preset "<n>" for <op> (have: a, b); save one with
  --save-preset` / `(none saved); …`; Go said `<op> has no preset "<n>" (have: …)` /
  `<op> has no saved presets (save one with --save-preset)`.
- `--export` write failures read `export <path>: <cause>`; Go said `export: <cause>`.
- `-v` step durations of a minute or more print as seconds (`90s`); Go printed `1m30s`.
- Text-table lines are right-trimmed (no trailing padding after the last column).
- SIGTERM and SIGHUP cancel like Ctrl-C (step cancelled, run audited, one error line, exit
  130). Go cancelled on SIGTERM but exited 1, and left SIGHUP at its default (killed by the
  signal, no error line).
- Config read errors (EACCES, EISDIR) exit 1; config parse errors exit 2. Go exited 1 for both.
- Bad op-level param values (e.g. `--tolerance abc`) exit 2 with the same message; Go exited 1.
- `--save-preset` with nothing to save (no non-empty param after merging the preset and flags)
  exits 2 before the operation runs; Go ran the operation, then failed to save (exit 1).
  `--save-preset ""` still counts as not given, as in Go.
- A failed audit-log write prints `warning: audit log: <cause>` on stderr; the run's outcome and
  exit code are unchanged. Go ignored the failure silently.
- When the confirmation prompt ends without an answer line (EOF, or the run cancelled while
  waiting), a newline is written to stderr so the error starts on its own line.
- Invariant, not divergence: `diff::json` and `jsonutil::canonical` assume inputs came through
  `jsonutil::parse`/`normalize`; a `Value` built by `serde_json::to_value` on an SDK struct
  (Phase 4/5) must be normalized first or large integers hash differently from Go.

## 5. Self-review notes

- Spec coverage: every Go package has a home above (`cmd`→Phase 2/7, `internal/ops/*`→2/5,
  `diff/jsonutil`→1, `s3svc/ddbsvc/kubectl`→3/4, `tui/*`→6, `config/guard/audit/cache/preset/fuzzy/render/report`→2, `awsx`→4, `hack/*`→reused as-is, `testutil/golden`→insta, `tuitest`→TestBackend helpers).
- Type consistency: `Delta`, `View`, `Input`, `Runner`, `SvcError` names are used
  identically across phases.
- Deliberate divergences from Go (document in DESIGN rev 4): explicit registry vs `init()`
  registration; `Option<&'static str>` metadata vs empty strings; `BTreeMap` input vs
  Go map (deterministic param order in audit/report); exit codes 0/1/2/130 vs uniform 1;
  `deny_unknown_fields` on config; colon-aligned summary layout; `ss completion`. All four
  user-visible ones come from the redliner house style (§2a) and are called out in the
  README migration note.
