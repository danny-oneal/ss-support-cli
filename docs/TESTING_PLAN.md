# ss-support-cli — Test Coverage Plan

Status: **in progress** · Owner: Danny O'Neal · Last updated: 2026-08-02 (rev 2)

> Baseline measured 2026-08-02: `go test -coverprofile ./...` reports **34.1%**
> (matches the reported figure). A `-coverpkg=./...` cross-package profile — which
> correctly credits `internal/ops` code paths only exercised by sibling packages like
> `k8sops` — reports **39.1%**, the more accurate "real" number. Gap between the two
> is itself a finding: coverage tooling should switch to `-coverpkg=./...` (T0 below).
>
> **rev 2 (2026-08-02):** T0 landed; both items previously deferred in §5 are now
> adopted and their harnesses are in the tree. Reported coverage is **41.1%**
> (`make cover`). Rev 2 also corrects a factual error in rev 1 — see the note in §1.

---

## 1. Baseline by package

| Package | Coverage | Status |
|---|---|---|
| internal/fuzzy | 97.5% | strong |
| internal/report | 94.3% | strong |
| internal/ops/k8sops | 85.5% | strong |
| internal/diff | 83.7% | strong |
| internal/guard | 81.8% | strong |
| internal/ops/flowops | 74.6% | adequate |
| internal/ops/ddbops | 74.5% | adequate |
| internal/ops | 72.1% | adequate |
| internal/ops/s3ops | 72.2% | adequate |
| internal/preset | 71.3% | adequate |
| internal/kubectl | 66.7% | adequate |
| internal/jsonutil | 66.7% | adequate |
| internal/config | 39.4% | weak |
| internal/s3svc | 13.7% | weak (real AWS adapter untested) |
| internal/ddbsvc | 0.0%* | *has a test file, but it's a skip-by-default integration test |
| cmd | 0.0% | no test files |
| internal/audit | 0.0% | no test files |
| internal/awsx | 0.0% | no test files |
| internal/cache | 0.0% | no test files |
| internal/render | 0.0% | no test files |
| internal/ops/demoops | 0.0% | no test files (example ops, low priority) |
| internal/tui + all subpackages | 0.0% | zero tests anywhere in the TUI layer |
| hack/* | 0.0% | standalone scripts, excluded from scope |

**Existing patterns to extend (not replace):** stdlib `testing`, table-driven, no
mocking library or assertion framework. Ports & Adapters throughout `s3svc` /
`ddbsvc` / `kubectl`, with hand-written `Fake` structs (not generated mocks). The
`ops` package tests use a hand-written fake `Confirmer` recording `ConfirmSpec`s
and returning a scripted answer (`internal/ops/runner_test.go`,
`internal/ops/k8sops/deploy_test.go`) — this is the pattern for confirmation-flow
coverage. `kubectl` tests stub the real binary with a temp-dir shell script.

> **Correction (rev 2).** Rev 1 stated "No golden/snapshot testing exists yet."
> That was wrong. `internal/diff/json_test.go` has had golden-file testing since
> P1 — a `-update` flag over `testdata/<case>/{a,b}.json` + `expected.txt`, with a
> `make golden` target. The pattern was not new to the codebase, only unextracted,
> which materially changes the §5 argument against adopting it more widely.

---

## 2. Findings that shape prioritization

- The confirmation guard (flagged in prior work as "env-only") has two independent,
  **completely untested** production implementations: `cmd/root.go`'s
  `cliConfirmer`/`stdinConfirm`/`stdinConfirmTyped` (real CLI, reads operator stdin,
  requires typing the resource name back for Destructive ops) and
  `internal/tui/screens/opsresult.go`'s `tuiConfirmer`/`waitConfirm`/`handleConfirmKey`.
  Both implement the same security-relevant contract; neither has a single test.
- `internal/ops/runner.go`'s `confirm()` has one clear gap: the branch where the
  `Confirmer` itself returns a hard error (distinct from a declined answer) is never
  exercised.
- Exactly one code path in the whole repo reaches `guard.Destructive`:
  `k8sops.ScaleDeployment.Run` scaling to zero, via `Runner.Escalate()`. That path
  *is* tested (`TestScaleToZeroEscalatesToDestructive`, confirms `--yes` can't bypass
  it). No operation is statically registered as Destructive — it's reached only
  dynamically, which makes this a thin, single-threaded piece of safety-critical logic
  worth extra scrutiny as more Destructive ops get added.
- `s3svc`/`ddbsvc` real AWS adapters (`aws.go` in each) already sit behind a `Port`
  interface with existing `Fake` implementations used elsewhere — no refactor needed,
  just unit tests around request-building/response-mapping, separate from the
  env-gated integration tests that already exist.
- The TUI layer (~1,693 loc in `internal/tui/screens` alone) is 100% untested but
  bubbletea's Model/Update/View pattern is directly testable by feeding `tea.Msg`
  values into `Update` and asserting on the resulting model/`View()` string — no
  golden files required to start.

---

## 3. Phased plan

**T0 — Tooling fix (trivial, do first) — ✅ landed rev 2**
Switch CI/make targets to `go test -coverpkg=./... -coverprofile=...` so cross-package
exercised code (e.g. `internal/ops` via `k8sops`) is credited correctly. This alone
moves the reported baseline from 34.1% → 39.1% with no new tests, and prevents future
work from being misjudged.

**TH — Test harness (do before the codebase grows) — ✅ landed rev 2**
Both conventions below were deferred in rev 1 §5 on the grounds that they were
premature. They were brought forward deliberately: adopting a test convention is
cheap while there are 21 test files and expensive once there are 80, and the
first thing a convention has to survive is the growth spurt, not the quiet period.

- `internal/testutil/golden` — the `internal/diff` `-update` pattern extracted into
  one implementation, one flag, one normalization policy (CRLF → LF, trailing
  whitespace stripped, single trailing newline). `Assert` for
  `testdata/<name>.golden`, `AssertFile` for `diff`'s fixture-directory layout,
  `AssertView` for terminal output. `internal/diff` migrated with zero fixture churn.
- `internal/testutil/tuitest` — `Key`/`Type`/`Msg`/`RequireMsg` plus pinned
  `Width`/`Height`, for feeding `tea.Msg` values into `Update` synchronously.
  Deliberately not `teatest`: no `tea.Program`, no goroutines, no timing.
- `internal/s3svc/porttest` — the `s3svc.Port` conformance suite, run against
  `s3svc.Fake` in unit tests and against the AWS adapter under
  `SS_S3_TEST_ENDPOINT`. Replaces ~120 lines of assertions that previously existed
  only on the integration side.
- `make golden` now regenerates every golden in the repo (package list derived from
  importers of `testutil/golden`); the helper refuses `-update` when `CI` is set.
  A new `contract` CI job runs the port suite against MinIO.

Reference examples to copy: `internal/tui/components/confirm/confirm_test.go`
(behavior assertions + goldens, and the comment explaining which is which) and
`internal/s3svc/fake_contract_test.go`.

**T1 — Confirmation & guard flow (highest priority: safety-critical, zero coverage)**
- `cmd/root.go`: `cliConfirmer`, `stdinConfirm`, `stdinConfirmTyped` — test via
  injected `io.Reader`/`io.Writer` for stdin/stdout, covering: accept, decline,
  Destructive typed-confirmation match/mismatch, EOF/interrupted input.
- `internal/tui/screens/opsresult.go`: `tuiConfirmer`, `waitConfirm`,
  `handleConfirmKey`, `confirmView` — drive via `Update(tea.KeyMsg{...})`, assert
  resulting state and rendered view text.
- `internal/ops/runner.go`: add the missing case where `Confirmer` returns a hard
  error inside `confirm()`.
- Target: both confirmer implementations and the runner's confirm branch at ~90%+.

**T2 — Real AWS adapters (`internal/s3svc`, `internal/ddbsvc`) — partially landed**
Add unit tests for `aws.go` in each package around request construction and
response/error mapping, using the AWS SDK's documented testing seams (no new mocking
library — same "interface + hand-written fake" pattern already used for the `Port`
consumers). Leave the existing env-gated integration tests as-is; these are additive.
Target: both packages to ~70%+, matching sibling `ops` packages.

`s3svc` is at 34.5% (from 13.7%) via the porttest suite. `ddbsvc` still needs a
`ddbsvc/porttest` sibling — the same treatment, and the higher-value one, since
`ddbsvc` is at 0.0%.

**T3 — Untested small packages, easy wins**
`internal/audit`, `internal/cache`, `internal/awsx`, `internal/render` — all small
(60-140 loc), already interface-friendly, no refactor needed. Straightforward
table-driven tests. `internal/awsx` needs a fake STS client for `CheckIdentity`.
Target: 80%+ each, low effort/high ROI.

**T4 — `cmd/` package (cobra wiring)**
Beyond the confirmer (covered in T1), test flag parsing, precedence
(flag > env > config > default per §2 of DESIGN.md), and command registration from
operations. Target: 60%+ (full coverage of cobra scaffolding has diminishing returns).

**T5 — `internal/config` (currently 39.4%)**
Fill gaps in config file loading/merging and env var precedence resolution.
Target: 75%+.

**T6 — TUI layer (`internal/tui` and subpackages) — started**
Largest volume of untested code. `internal/tui/components/confirm` is done (100%)
and is the reference example. Next: `errorpanel` and `progresslist` (small, pure
render functions — near-free goldens), then `diffviewer`, then the screens, ending
with `opsresult.go`'s confirmation flow, which overlaps T1.

Split every screen's tests two ways, and keep the split honest:

- **Behavior** — plain table-driven assertions on the model after `Update` and on
  the message the returned `tea.Cmd` produces. Anything a mutation decision depends
  on lives here. Never snapshot it.
- **Layout** — `golden.AssertView`. Goldens are ANSI-stripped and rendered at
  `tuitest.Width`, so they pin structure and content, not color.

The consequence of ANSI-stripping is real and was hit immediately: `confirm`
encodes the selected button purely as a lipgloss style, so a stripped snapshot of
the Yes state is byte-identical to the No state. A golden pair there would have
looked like coverage while asserting nothing. Rule of thumb — **golden-test what
the operator reads, plain-assert what the operator's choice depends on.** If a
view's state genuinely lives in styling, assert the raw string directly rather
than reaching for a second golden format.

**Explicitly out of scope:** `internal/ops/demoops` (example code, not production
risk), `hack/*` scripts.

---

## 4. Estimated impact

Rough coverage contribution if T0–T5 are completed (T6 TUI work sized separately
given its volume):

| Phase | Approx. LOC newly covered | Notes |
|---|---|---|
| T0 | 0 (measurement fix only) | 34.1% → 39.1% reported, no new tests |
| T1 | ~100-150 | small but highest safety value |
| T2 | ~200-300 | two packages, request/response mapping |
| T3 | ~330 | four small packages |
| T4 | ~350 | partial cobra coverage |
| T5 | ~150 | config gap-fill |

T0-T5 combined should plausibly bring overall reported coverage into the
**55-65%** range without touching the TUI. Closing TUI coverage (T6) is the
remaining path to anything higher and should be scoped as its own follow-on
effort once T1-T5 land.

---

## 5. Decisions on the rev 1 alternatives

### 5.1 Golden-file/snapshot testing — **adopted** (was: deferred)

Rev 1 deferred this on the grounds that it "adds a new pattern the codebase doesn't
use yet." It doesn't: `internal/diff` has had golden tests since P1. The real
choice was between one shared implementation and two divergent ones, and the cost
of choosing only rises with the number of test files.

Rejected in favour of extraction:

- `charmbracelet/x/exp/golden` — already present transitively, but its flat layout
  doesn't fit `diff`'s fixture directories, so adopting it means running two styles
  or migrating existing fixtures for no behavioral gain.
- `teatest` — drives a real `tea.Program` and snapshots final output. Catches
  integration bugs the `Update`-driven approach can't, but it is slow, timing
  sensitive, and a poor default. Worth revisiting for a small number of
  end-to-end screen flows once the screens stabilize; not the workhorse.

Guard rails, because snapshot suites rot in predictable ways: goldens are
normalized and ANSI-stripped so diffs stay reviewable; `-update` is refused when
`CI` is set; `make golden` prints a reminder to read the diff.

### 5.2 Mocking library (gomock/testify) — **still rejected**, with a replacement

Not adopted, and the rev 1 reasoning holds, but it was incomplete — it argued from
convention rather than from what the tests need:

- The fakes are not stubs, they are in-memory implementations. `s3svc.Fake` stores
  `bucket → key → []Version`, which is what lets an op test assert on end state.
  Scripted `EXPECT().Put(...).Return(...)` calls would re-encode that behavior once
  per test.
- The interaction-assertion half already exists by hand: `kubectl.Fake.Calls`, and
  the `ops` fake `Confirmer` recording `ConfirmSpec`s. That is what proves
  confirm-before-mutate.
- `var _ Port = (*Fake)(nil)` already gives the compile-time sync guarantee that is
  mockgen's main maintenance argument.
- Expectation-based tests assert *how* code works, so with ports this narrow
  (~6 methods) they mostly buy brittleness, plus codegen in the build. Note also
  that `golang/mock` is archived; the live fork is `go.uber.org/mock`.

**The actual risk was never missing mocks — it was fake/real divergence**, and
generated mocks make that worse, not better. Replacement: `internal/s3svc/porttest`,
a conformance suite run against both `s3svc.Fake` and the AWS adapter.

It paid for itself on its first run against MinIO. `GetVersion` with a malformed
version ID returned a raw smithy `InvalidArgument` (HTTP 400) from the adapter,
while `Fake` returned `ErrNotFound` — so a stale or mistyped version ID, the most
likely operator error in the restore flow, produced an SDK error string instead of
a clean "no such version". Fixed via `normalizeVersionLookup` in `s3svc/aws.go`,
scoped to lookups that actually carried a version ID: the same error code and
message is what S3 returns for a *non-versioned bucket*, and reporting "not found"
there would hide the most useful diagnosis this tool can give.

To be precise about the severity, since the first draft of this section overstated
it: **nothing outside the test tree currently branches on
`errors.Is(err, s3svc.ErrNotFound)`.** The bug's blast radius today is error text,
not control flow. It is still worth fixing — the Port contract should mean the same
thing on both sides before callers start depending on it, which is exactly the
window we are in.

Known remaining divergences are enumerated in the "Known gaps" comment in
`porttest.go` rather than left implicit: delete markers, prefix rollup, delete-marker
position under tied timestamps, and the unrun `Cached`/`Lazy` Port decorators. That
list should shrink. Adding a `Fixture` flag to make a contract test pass is a
decision for review, not for a test file.

### 5.2.1 Pagination — closed

The largest gap on that list is now closed. Both fakes ignored pagination entirely:
`s3svc.Fake.List` discarded its `Token` and never set `Page.Next`, and
`ddbsvc.Fake.Query` did the same with `ExclusiveStart`/`Next`. Four production drain
loops depended on it — two in `s3ops/prefixaudit.go`, one each in
`ddbops/itemhistory.go` and `ddbops/gsiconsistency.go` — and every one of them ran
exactly once in every test. An adapter that stopped honoring a continuation token
would have shipped green.

- Both fakes take an opt-in `PageSize`. Zero keeps the old single-page behavior, so
  no existing test changed; any test touching a drain loop sets it.
- `s3svc.WithPageSize` sets `MaxKeys`, so the contract suite can force truncation
  against real S3 without seeding a thousand objects.
- `porttest` gained a `Pagination` case that drains under a hard iteration cap —
  the failure mode is an infinite loop, so the test must not itself rely on the
  loop terminating — and asserts more than one round trip actually happened.
- `s3ops/pagination_test.go` and `ddbops/pagination_test.go` cover the production
  loops directly, including the property that page size must not change the answer.

That work surfaced one more divergence: an unknown continuation token made real S3
return `400 InvalidArgument` while `Fake` silently returned an empty page. Silently
accepting a bad token is the subtle route to an infinite drain, so `Fake` now rejects
it too, and the contract requires rejection from both. The error *kind* is
deliberately not unified — no caller fabricates a token, so a shared sentinel would
be ceremony with no consumer.

All seven mutants tried against this — dropping `Next`, ignoring the token,
accepting a bad token, and stopping each of the four production loops after page one
— are caught.

Revisit gomock if the Port count passes ~10, or when a collaborator appears that is
genuinely stateless (a clock, a notifier, a metrics sink) — those are the cases
where a stub really is the right shape.

### 5.3 Still deferred

- **Coverage threshold gate in CI** (fail the build if coverage drops) — worth
  adding once T1-T5 land and the number stabilizes, not before. A ratchet on a
  number that is still moving 5 points per phase just produces noise.
- **`ddbsvc/porttest`** — the same contract treatment for DynamoDB. Higher value
  than anything left in T2 (`ddbsvc` is at 0.0%), and now a mechanical exercise:
  copy the s3svc shape. Pagination is already covered at the ops layer, so what
  remains is error mapping and key-schema handling.
- **Run `Cached` and `Lazy` through `porttest`** — ten lines each, and `Cached`
  rebuilds `Object` from JSON, which is exactly where a round trip could drop
  `Meta` fields or mangle an error.
