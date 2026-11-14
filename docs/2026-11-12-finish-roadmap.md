# Finish Roadmap: 0.1.0-alpha.4 → 0.1.0

> **For agentic workers:** This is a roadmap. Each milestone is executed from its own task-level plan written with `superpowers:writing-plans` and run with `superpowers:subagent-driven-development` or `superpowers:executing-plans`. M1's task plan exists (`docs/2026-11-12-tui-core.md`). Before writing the task plan for a later milestone, ask the user that milestone's open decisions; do not settle them yourself.

**Goal:** Take `ss-support` from 0.1.0-alpha.4 to a finished 0.1.0: an interactive TUI, the two guarded write operations, and a minimal, documented way to install it.

**Architecture:** Unchanged from `docs/DESIGN.md` and the master plan: front-ends (clap CLI, ratatui TUI) → operations layer (`Operation`, `Registry`, `Runner`, `View`) → ports (S3, DynamoDB, kubectl) with fakes and adapters. The TUI is a second front-end over the same `Runner`, so every safety rule (environment allowlist, confirmation, audit) is shared rather than re-implemented.

**Tech Stack:** Rust 2024 (MSRV 1.88), tokio, clap 4, ratatui 0.30 + crossterm 0.29, aws-sdk-rust, serde, insta, clap_complete.

**Spec:** `docs/2026-11-07-rust-port-master-plan.md` (Phases 5–7), as re-sequenced and cut by the decisions below. `docs/DESIGN.md` for behavior.

## Where the project stands (verified against `origin/main` at `9461619`, 2026-11-12)

| Master-plan phase | State |
|---|---|
| 0 Foundation, 1 Diff engine, 2 Ops core + headless CLI | Landed (0.1.0-alpha.1, .2) |
| 3 Ports, fakes, decorators, kubectl adapter | Landed, read-only: S3, DynamoDB, kubectl ports with fakes and contract suites |
| 4 AWS adapters | Landed, read-only: S3, DynamoDB, STS account guardrail |
| 5 Operations | 8 of 10 real operations landed: `s3.version-diff`, `s3.object-inspect`, `s3.prefix-audit`, `ddb.item-compare`, `ddb.item-history`, `ddb.gsi-consistency`, `config.drift`, `record.trace` (plus `demo.hello`, `demo.json-diff`). Missing: `s3.restore-version`, `ddb.item-edit` |
| 6 TUI | Not started. No ratatui dependency yet |
| 7 Cutover and distribution | Partly moot (the repository is already standalone, fixtures are vendored). No completions, version stamping, or configuration reference |

## Decisions

User-confirmed on 2026-11-12:
- **The TUI comes next**, ahead of the remaining operations.
- **Order:** M1 TUI core → M2 pods and logs screens → M3 write operations → M4 distribution.
- **Write operations are in scope:** `s3.restore-version` and `ddb.item-edit`.
- **Go parity is dropped.** The differential harness and "Go retirement" steps are removed; this repository has no Go source or binary. Correctness rests on the golden diff fixtures, the port contract suites, and the end-to-end tests.
- **Distribution is minimal:** static shell completions, version stamping, a configuration reference, and `cargo install` instructions. No release workflow, no install script, no prebuilt binaries.

Fixed by the project rules (`CLAUDE.md`, and a test in `svc/kubectl/mod.rs` that fails the build if the port grows a write method):
- **Kubernetes stays read-only.** The master plan's rollout-restart and scale operations are permanently out, so the operation count ends at ten real operations, not twelve.

Assistant's choices (redirect freely):
- One pre-release per milestone, continuing the existing pattern: M1 → 0.1.0-alpha.5, M2 → 0.1.0-alpha.6, M3 → 0.1.0-alpha.7, M4 → 0.1.0.
- Each milestone lands as its own branch and pull request; pushing and merging stay with the user.

## Global Constraints

- `make check` (fmt, clippy `-D warnings`, tests) and `make msrv` (1.88) pass at every commit.
- Every public item has a doc comment. Source comments, doc comments, test names, and manifests describe behavior only.
- Library code never reads `std::env`; anything that reaches output uses `BTreeMap`; config structs keep `deny_unknown_fields`.
- `async_trait` only on dyn-dispatched traits.
- Every write goes through `Runner::mutate`: environment allowlist, confirmation (`--yes` never covers a destructive operation), dry-run stop, audit entry.
- `CHANGELOG.md` `[Unreleased]` and the README flag table, key table, and exit-code list match behavior at every milestone.
- Exit codes stay `0` ok · `1` failed · `2` usage · `130` interrupted, in both front-ends.

## Review Focus

Cross-milestone failure modes most likely to hurt an operator; each gets a test in the milestone that owns the code.

1. **A write lands on state that changed after the operator approved the preview** (another writer, or a slow confirmation). The write must be conditional on what was previewed, or re-checked, and refuse otherwise. (M3)
2. **A restore silently drops object metadata** (content type, user metadata, encryption settings) because it re-uploads a body instead of copying the version. (M3)
3. **The terminal is left in raw mode** after a panic, a signal, or a double signal. (M1, re-checked in M2 for the log stream)
4. **Untrusted text reaches the terminal raw**: escape sequences in pod logs, object keys, or item values move the cursor or repaint the screen. (M1 result views, M2 logs)
5. **Unbounded growth in a long session**: a followed log stream or a huge listing exhausts memory. (M2)
6. **The TUI and the headless CLI disagree** on what an operation does: different defaults, a skipped guard, a missing audit entry. (M1, M3)
7. **Completions and help drift from the dynamic `run <op>` subcommands**, which are generated from the registry at runtime. (M4)

---

## M1 — TUI core → 0.1.0-alpha.5

**Task plan:** `docs/2026-11-12-tui-core.md` (9 tasks, validated against the code and a compiled ratatui 0.30 spike).

**Deliverable:** a bare `ss-support` on a terminal opens the picker; every registered operation can be filled in, run, confirmed, and viewed; the S3 browser marks two versions and opens `s3.version-diff`.

**Exit criteria:** the plan's end-to-end tests are green; the manual terminal-restore walkthrough is done and recorded; README has the key table; `[Unreleased]` lists the TUI.

**Status (2026-11-13): landed on branch `tui-core` as 0.1.0-alpha.5, reviewed by a fresh reviewer, review findings fixed.** 19 end-to-end tests drive the real event loop on an in-memory terminal; about 230 unit tests cover the screens and components. The real binary was driven on a pseudo-terminal: picker, filter, form, and a `demo.hello` run work, and the terminal is restored after `q`, after SIGTERM (exit 130), and after a panic in an operation (30 of 30 runs of a debug probe build: exit 101, cursor shown, nothing drawn afterwards). Not checked by hand or machine: a second signal during shutdown, a panic in a release build, the S3 browser against MinIO on a real terminal, and how the colours look.

Decisions taken during the work that differ from the task plan:
- Ctrl-C goes back one screen everywhere except the operation list (where it quits) and a run (where it cancels or declines). Esc goes back and never quits.
- A confirmation is always typed and ended with enter (`y` for a mutating operation, the resource name for a destructive one), and `--yes` has no effect in the TUI. Keys and pastes are dropped for 250 ms after a question or a result appears.
- A param's default is shown as a placeholder, not filled in, so presets keep holding only chosen values.
- The S3 browser passes both marked versions by id; the newer one is not turned into `null` even when it is the latest. A version whose id is `null` and which is not the latest is refused.
- A panic while the TUI holds the terminal restores it and ends the process, in debug builds as in release builds.
- Leaving the TUI cancels runs in flight and waits up to five seconds for their audit entries.
- Bare `&str` widgets are never drawn directly: ratatui does not clip them to an empty area, which panicked on very small terminals.

Carried into later milestones from the review: keep a run's warnings (a failed audit write) on the result screen for writes, and consider an audit entry written when a write starts, not only when it ends (M3); drain several background results per redraw before adding a log stream (M2).

## M2 — Pods and logs screens → 0.1.0-alpha.6

**Deliverable:** `p` on the picker opens a pod list for a namespace; enter on a pod tails its log.

**Modules (`ss-support/src/tui`)**: `screens/k8spods.rs`, `screens/logstail.rs`; additions to `screen.rs`.

**Interfaces to pin in the task plan:**
- Consumes (all exist): `KubectlPort::list_json(namespace, "pods")`, `ss_core::svc::kubectl::parse_pods(&[u8]) -> Result<Vec<Pod>, SvcError>`, `Pod { name, phase, ready, restarts, created }`, `KubectlPort::logs(namespace, pod, follow) -> Result<mpsc::Receiver<String>, SvcError>` (dropping the receiver stops the stream), `FakeKubectl::{insert_list, set_log_lines}`.
- Produces: `Loaded::Pods(Vec<Pod>)`, `Msg::LogLine(String)`, `Msg::StreamEnded(Option<String>)`, `Cmd::Stream(u64, BoxFuture<Result<mpsc::Receiver<String>, String>>)` whose lines the shell forwards to the issuing screen and whose receiver it drops when that screen leaves the stack.

**Must hold:** log lines are stored in a bounded buffer (oldest dropped, with a "N lines dropped" marker); control and escape characters in log text are rendered inert; leaving the screen stops kubectl; an environment without a `kube_context` shows the port's error and leaves the screen usable; the port stays read-only.

**Open decisions for the task plan:**
- Where the namespace comes from: typed each time, remembered per environment, or a new `namespace` field in the environment config.
- Log screen keys (proposed: `f` follow on/off, `/` filter, `g`/`G` top/bottom, `pgup`/`pgdn`, `esc`), and the buffer size.
- Whether the pod list auto-refreshes or only on `r`.

**Exit criteria:** screens snapshot-tested with `FakeKubectl`; an end-to-end test goes picker → pods → logs → back; a manual run against a real cluster confirms kubectl exits when the screen closes; README key table updated.

**Status (2026-11-13): landed on branch `tui-pods-logs` as 0.1.0-alpha.6** (task plan `docs/2026-11-12-tui-pods-logs.md`). The open decisions were settled as the plan's Decisions section records: the namespace is typed with `n` and not stored, the pod list reloads only on `r`, `f` toggles following in the view rather than restarting kubectl, the buffer is 5,000 lines, and colour codes are stripped. The whole-branch review found that three Review Focus items did not hold on the real adapter, and they were fixed before the pull request: a log line is at most 16 KiB and the buffer at most 8 MiB, so a log with no line breaks no longer grows memory; every kubectl still running is stopped on every path that ends the process, and a load is aborted when its screen leaves; a named namespace that lists empty is looked up, so a mistyped one is reported. The same pass made STATUS what `kubectl get pods` shows, capped redraws while only log lines arrive, and replaced tests that could not fail. "kubectl exits when the screen closes or the process ends" is now checked on the built binary with a stand-in kubectl on a pseudo-terminal (`tests/signals.rs`, Linux). Not checked: a run against a real cluster, so how a real `kubectl` words its failures, and that it lists a missing namespace as empty, rest on knowledge of kubectl rather than on a test.

## M3 — Guarded write operations → 0.1.0-alpha.7

**Deliverable:** `s3.restore-version` and `ddb.item-edit`, headless and in the TUI, each showing the exact change as its confirmation preview.

**Modules:** `ss-core/src/svc/s3/*` and `svc/ddb/*` (port, fake, lazy, cached, contract), `ss-core/src/ops/s3/restore.rs`, `ss-core/src/ops/ddb/item_edit.rs`, `ss-support/src/aws/{s3,ddb}.rs`, `ops::builtin()`.

**Interfaces to pin in the task plan:**
- Port writes (one per service; shape is an open decision, see below), implemented by the fake, the `Lazy` and `Cached` decorators, the AWS adapter, and exercised by the contract suite against MinIO and dynalite.
- Operations use what exists: `Runner::step` for reads, `Runner::preview(views)`, `Runner::set_resource(name)`, `Runner::mutate(name, ..)`; `Class::Mutate` or `Class::Destructive` in `Meta`.
- `s3.restore-version`: params `bucket`, `key`, `version` (suggestions reuse the existing bucket → key → version chain). Preview is the `View::Diff` from current to the chosen version. Restoring the current version is a no-op that changes nothing and says so.
- `ddb.item-edit`: params `table`, `key` (same key syntax as `ddb.item-compare`), and the change. Refuses any change to a key attribute. Preview is the `View::Diff` from the stored item to the edited item. Items read through the SDK are normalized with `jsonutil` before diffing.

**Must hold:** writes are refused outside allowlisted environments before any confirmation; the write is conditional on the previewed state (Review Focus 1); a restore keeps the object's metadata (Review Focus 2); `--dry-run` stops before the write and still shows the preview; one audit entry per run with the outcome; README's "only reads" statements are corrected and the write section documents the allowlist.

**Open decisions for the task plan:**
- The class of each operation (`Mutate` with `y/n`, or `Destructive` with the typed resource name). A restore on a versioned bucket is itself reversible; an item edit is not.
- The S3 port write: a server-side copy of the chosen version (`restore(bucket, key, version_id) -> new_version_id`, keeps metadata; recommended) or the master plan's `put(bucket, key, body)`.
- How `ddb.item-edit` takes the change: a JSON object of attributes to set, a JSON patch, or `name=value` pairs; whether attributes can be removed.
- The concurrency rule for the item write: condition on the previewed attribute values, or on a version attribute when the table has one.
- Whether restoring to a delete marker (deleting the object) is allowed, and whether the S3 browser gets a restore key.

**Exit criteria:** fake-backed operation tests including the no-op restore, the refused key-attribute edit, the changed-since-preview refusal, and the dry run; contract suites green against MinIO and dynalite (`make contract`, `make contract-ddb`); a TUI end-to-end test confirms and runs each write; `ss-support ops list` shows ten real operations.

**Status (2026-11-13): landed on branch `write-ops` as 0.1.0-alpha.7** (task plan `docs/2026-11-13-guarded-writes.md`, whose Decisions section settles the open decisions above). `s3.restore-version` is `Mutate` and a server-side copy; `ddb.item-edit` is `Destructive`, takes `--set` (a JSON object) and `--remove`, and is conditional on the previewed values of the attributes it touches. Decided while building: a dry run now returns the preview; a write's preview ignores `diff.ignore`; the DynamoDB adapter reads the item and conditions on its stored native values, and refuses to set an attribute that holds a set or binary data; a whole number of 2^53 or more is refused. The whole-branch review found that a restore in a bucket with versioning suspended could overwrite an older version without an id, so the restore now asks the bucket and runs only where versioning is enabled; it also found previews that showed no change for a reordered array, which now use neither the ignore paths nor the keyed matching of the config. The S3 write is checked against MinIO. The DynamoDB write is checked against dynalite for string and number attributes only: dynalite never finds two maps or lists equal in a condition, so an edit of a map or list attribute rests on the in-memory store and on DynamoDB's documented behaviour, not on a test against a server.

## M4 — Minimal distribution → 0.1.0

**Deliverable:** an operator can install, complete, configure, and identify the tool from the repository alone.

**Work:**
- `ss-support completion <bash|zsh|fish>` with `clap_complete`, generated from `cli::command(registry)` so the per-operation `run <op>` subcommands and their flags are included. Value completion for parameters with suggestions is a documented gap.
- Version stamping: `build.rs` puts `git describe --tags --always --dirty` into the version string, falling back to `CARGO_PKG_VERSION` when git or the repository is absent; the build stays deterministic for a clean tag.
- `docs/configuration.md`: reference for `config.yaml` and `presets.yaml`, one table per section, generated by hand from the config structs and checked by a test that parses every documented example.
- README: install with `cargo install --path crates/ss-support --locked`, completion setup, the final flag table, key table, and exit codes.
- `docs/DESIGN.md` and `docs/TESTING_PLAN.md` revised to describe this codebase; the master plan and this roadmap marked closed; `CLAUDE.md` "only demo operations exist so far" and the Docs section corrected.
- Final pass: `cargo audit`/`cargo deny` if available, `cargo doc` without warnings, a full manual walkthrough of both front-ends against MinIO, dynalite, and a cluster.

**Open decisions for the task plan:**
- Whether 0.1.0 is tagged straight from M4 or after a soak period on the last alpha.
- Whether to add `cargo install --git` instructions (requires the repository to be reachable by the operators).

**Exit criteria:** completions load in bash, zsh, and fish and list every operation; `ss-support --version` shows the tag; every documented config example parses; `make check` and `make msrv` green; `CHANGELOG.md` has a `[0.1.0]` section; tag `v0.1.0`.

**Status (2026-11-13): landed on branch `distribution` as 0.1.0, and this roadmap is closed** (task plan `docs/2026-11-13-distribution.md`). Completion scripts for bash, zsh, and fish; the bash script is asked for completions by bash in a test, the zsh script is loaded by zsh, and the fish script is checked for content only, fish not being installed where this was built. The version shows what git describes when the build is not at its own tag. `docs/configuration.md` is checked by a test. `docs/DESIGN.md` and `docs/TESTING_PLAN.md` describe this codebase. Left to the user: creating the tag, which in this repository has no `v` prefix (`0.1.0`). Not done: the walkthrough of both front-ends against MinIO, dynalite, and a cluster on a real terminal, and `cargo deny` (not installed).

---

## Removed from the master plan

| Item | Why |
|---|---|
| Differential parity harness against the Go binary; `go-freeze` / `go-final` tags; Go source deletion | No Go source or binary in this repository (user decision) |
| `k8s` rollout-restart and scale operations; class escalation | Kubernetes is read-only by design |
| Release workflow, cross-compiled binaries, checksums, install script, `make release-*` | Minimal distribution (user decision) |
| Moving the workspace to the repository root; relocating diff fixtures | Already done |
| Dynamic value completion | Documented gap; static completions only |

## Self-Review

- **Coverage of what remains:** Phase 5 remainder → M3; Phase 6 → M1 and M2; Phase 7 → M4 minus the removed rows. Nothing else in the master plan is unassigned.
- **Dependencies:** M2 needs M1's shell and `Screen` contract. M3 is independent of M2 but its TUI end-to-end tests need M1's confirmation modal. M4's completions need M3's operations registered so the generated script is final.
- **Unknowns stated, not settled:** every milestone lists its open decisions; none is assumed here.
