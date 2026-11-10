# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

*ALWAYS* adhere to the principles found in the "using:superpowers" for plan writing, plan execution, testing, documenting, deployment, and repo hygiene.

*ALWAYS* Rely on the "rust-skills" skill when creating, reading, or updating any rust code.

*ALWAYS* Clarify if you are permitted to use git commit, or any other modifying git command.

*ALWAYS* Ask clarifying questions during the planning stage. Never make assumptions or decisions on your own during the planning phase. Always declare a decision, and again if it changes, during the planning sessions.

`ss-support` is a support-operations CLI for non-production AWS and Kubernetes resources, written in Rust (edition 2024, MSRV 1.88). It is a port of an earlier Go implementation; only demo operations exist so far.

## Commands

```bash
make check                                   # what CI runs: fmt --check, clippy -D warnings, tests
make msrv                                    # cargo +1.88 check --workspace --all-targets --locked (CI job)
cargo test -p ss-core <name_substring>       # one unit test in the library
cargo test -p ss-support --test e2e <name>           # one end-to-end test (also: --test signals)
cargo test -p ss-core --test diff_golden     # diff golden fixtures
cargo run -p ss-support -- run demo.json-diff --left a.json --right b.json -o json
```

Clippy runs with `-D warnings` and the workspace sets `missing_docs = "warn"`, so every public item needs a doc comment. Dev builds optimize dependencies (`opt-level = 3`), so the first build is slow.

## Architecture

Two crates:

- **`crates/ss-core`** (lib `ss_core`) — everything testable without a terminal or cloud: the diff engine, the operations layer, config/guard/audit/cache/presets, renderers, and the markdown report. No AWS dependencies.
- **`crates/ss-support`** (binary + lib) — clap CLI, signal handling, confirmation prompt, I/O wiring. `main.rs` is a shim; logic lives in the lib so tests can call it in-process.

### Operations are the unit of work

An `Operation` (`ss-core/src/ops/operation.rs`) declares `Meta` (id like `demo.json-diff`, category, `guard::Class` blast radius) and a list of `Param`s, and implements `async fn run(&self, r: &mut Runner, input: &Input)`.

- **Registration is explicit**: add the op to `ops::builtin()` in `ss-core/src/ops/mod.rs` (no `inventory`-style auto-registration).
- **Params drive the CLI**: `ss-support/src/cli/dynamic.rs` generates one `ss-support run <op-id>` subcommand per op with a `--<param>` flag per `Param`. Clap does *not* enforce defaults or required params — the `Runner` does, after any `--preset` is merged. `ParamDoc` feeds both `--help` and `ss-support ops describe` so they stay in sync.
- **`Input`** is a lenient string map (`input.int()` returns 0 on a parse failure).
- **Ops return `OpResult` containing `View`s** (a closed enum in `ops/view.rs`); `render.rs` turns them into text or `-o json` (`{"views":[...]}`), and `report.rs` into the `--export` markdown.

### Runner = the safety model

`ops/runner.rs` wraps every run: fills defaults, checks required params, applies the `Guard` (writes refused unless the env is allowlisted in config), writes the JSONL audit entry, emits `Progress`, handles cancellation and `--dry-run`.

- Ops do work through `r.step(name, ..)` (read) and `r.mutate(name, ..)` (write). `mutate` triggers the confirmation (typed resource name for destructive ops; `--yes` never covers destructive ones), stops a dry run, and errors with `ReadOnlyMutation` in a `Class::Read` op.
- `execute` returns `Result<OpResult, RunFailure>`; `FailureKind` (Usage/Denied/Declined/Cancelled/Failed) is what `ss-support/src/run.rs` maps to exit codes.

### Exit codes and errors (`ss-support/src/run.rs`)

`Exit { Ok=0, Failed=1, Usage=2, Interrupted=130 }`. Error types expose `is_usage()` to split "user must fix input" (2) from "I/O failed" (1): an unparsable config/presets file is 2, an unreadable one is 1. Exactly one error line on stderr (`ss-support <subcommand>: {e:#}`); results go to stdout, `warning: …` lines to stderr (a failed audit write is only a warning). `thiserror` in library modules, `anyhow` at the run/CLI layer. SIGINT/SIGTERM/SIGHUP cancel → 130; a second signal exits at once.

### Environment and paths

Library code never reads `std::env` directly — it reads a `Vars` snapshot (`ss-core/src/vars.rs`). `Services` (`ss-support/src/services.rs`) bundles the registry and vars; `Services::isolated(root)` points HOME at a temp dir so config, presets, audit log, and cache all land there. Global flag precedence: flag → `SS_SUPPORT_ENV`/`SS_SUPPORT_PROFILE`/`SS_SUPPORT_REGION` → config file (`docs/config.example.yaml`).

### Diff engine (`ss-core/src/diff/`)

Semantic JSON diff with typed paths, structural ignore globs, keyed and LCS array alignment, float tolerance, null-as-absent, and a line-diff fallback. JSON numbers are normalized to f64 and formatted like the original implementation (exponent outside 1e-6..1e21) — preserve this. `serde_json` is deliberately built *without* `preserve_order`: sorted keys are what make `jsonutil::canonical` canonical. Golden cases live in `crates/ss-core/tests/fixtures/diff/<case>/{a.json,b.json,expected.txt}`.

## Testing

- `crates/ss-support/tests/e2e.rs` drives `ss_support::run_with(cli, Services::isolated(tmp), Io::capture(stdin))` in-process and asserts on `(Exit, stdout, stderr)` — no subprocess/`assert_cmd`. Custom stub `Operation`s can be registered into `services.registry` for a test.
- `crates/ss-support/tests/signals.rs` covers interrupt handling; `crates/ss-core/tests/runner.rs` covers the Runner with fakes.

## Conventions

- Source comments, doc comments, test names, and manifests describe behavior only: never mention Go/Golang, the port, plans, phases, reviews, or any other type of project or architecture related comment. Port/parity reasoning belongs in `docs/`.
- Anything that reaches output uses `BTreeMap` (deterministic ordering).
- Config structs use `serde_yaml_ng` with `#[serde(deny_unknown_fields, default)]`.
- `async_trait` only on dyn-dispatched traits (`Operation`, future resource ports); native `async fn` elsewhere.
- Kubernetes access is read-only by design — no rollout restart or scaling operations.
- Update `CHANGELOG.md` (Keep a Changelog, `[Unreleased]`) for user-visible changes; commits use Conventional Commits with a scope (`fix(cli): …`). The README flag table and exit-code list must match behavior.

## Docs

`docs/2026-11-07-rust-port-master-plan.md` is the port roadmap (read it before starting a new phase); phase task plans sit beside it. `docs/DESIGN.md` and `docs/TESTING_PLAN.md` describe the original design and still reference the Go implementation.
