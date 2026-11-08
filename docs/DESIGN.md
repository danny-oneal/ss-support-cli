# ss-support-cli — High-Level Design

Status: **implemented through P6** · Owner: Danny O'Neal · Last updated: 2026-08-01 (rev 3)

> **rev 3** — Roadmap split: P6 is now saved param presets + fuzzy filter (shipped);
> self-update, goreleaser, and the Homebrew tap move to a new **P7 — Distribution**.
> Also records an implementation change to §8: class-based confirmation happens at the
> first mutating step inside the Runner (not pre-run), so the prompt carries a preview of
> the exact change — e.g. restore shows the diff it is about to apply. The environment
> allowlist remains a pre-run hard stop.
>
> **rev 2** — S3/DynamoDB work is composite, not CRUD (e.g. fetch two object versions and
> diff them). Adds an **Operations layer** (§5), a **JSON diff engine** (§6), and a
> **View/renderer seam** (§7) so one operation renders to TUI, terminal, and a pasteable
> report.

---

## 1. Purpose

A single terminal tool for support engineers to diagnose and remediate **non-production**
resources. The unit of work is a *support operation* — a named, parameterized diagnostic
that may touch several AWS services and produce a structured result — not a raw API call.

**Goals**

- One binary; no runtime deps beyond `kubectl` and a configured AWS profile.
- Complex multi-step operations as first-class, testable, reusable units.
- Interactive TUI for exploration; the same operations runnable headless in runbooks.
- Results readable in-terminal *and* exportable for tickets.
- Guardrails that make production changes structurally impossible.

**Non-goals (v1)**

- Production write access.
- Replacing Grafana/CloudWatch dashboards.
- A general-purpose AWS client — if an engineer needs raw `aws s3api`, they should use it.

---

## 2. Invocation model

Hybrid: Cobra owns the process, Bubble Tea owns the interactive path. **Operations are the
command surface** — each registered operation is automatically both a subcommand and a TUI
menu entry.

```
ss                                      # TUI: operation picker
ss ops list                             # every registered operation
ss run s3.version-diff \                # headless, same code path
    --bucket cfg-nonprod --key tenants/acme.json \
    --a 3HL4kqtJ... --b null            # "null" = current version
ss run s3.version-diff ... -o json      # machine-readable delta
ss run s3.version-diff ... --export d.md
ss run record.trace --tenant acme       # cross-service op (DDB → S3)
```

Persistent flags: `--profile`, `--region`, `--env`, `--context`, `-o/--output`,
`--export`, `--dry-run`, `--yes`, `--verbose`.

Precedence: **flag > env (`SS_*`) > `~/.config/ss/config.yaml` > default**.

Cobra flags and TUI form fields are both **generated from the operation's declared
params** (§5.2). One definition, two front-ends, plus shell completion for free.

---

## 3. Architecture

```
      ┌──────────────┐              ┌──────────────┐
      │ cmd/ (Cobra) │              │ tui/ (Charm) │        front-ends
      └──────┬───────┘              └──────┬───────┘
             └───────────┬─────────────────┘
                  ┌──────▼───────┐
                  │  ops.Registry│                          application layer
                  │  Operation   │   ← the interesting code lives here
                  └──────┬───────┘
          ┌──────────────┼──────────────┬──────────────┐
     ┌────▼────┐   ┌─────▼────┐   ┌─────▼────┐   ┌─────▼────┐
     │  diff   │   │ jsonutil │   │  report  │   │  render  │  domain services
     └─────────┘   └──────────┘   └──────────┘   └──────────┘
                  ┌──────▼───────┐
                  │ Deps (ports) │                          infrastructure
                  └──────┬───────┘
      ┌───────┬──────────┼─────────┬─────────┐
   ┌──▼───┐ ┌─▼────┐ ┌───▼────┐ ┌──▼───┐ ┌───▼───┐
   │s3svc │ │ddbsvc│ │kubectl │ │guard │ │ audit │
   └──┬───┘ └─┬────┘ └───┬────┘ └──────┘ └───────┘
   AWS SDK  AWS SDK  exec kubectl
```

The key change from rev 1: **front-ends no longer call resource ports.** They select an
operation, collect params, run it, and render its result. Ports are an implementation
detail of operations.

### Package layout

```
ss-support-cli/
├── main.go                   # cmd.Execute()
├── cmd/
│   ├── root.go               # persistent flags, Deps + Registry construction
│   ├── run.go                # `ss run <op>` — flags generated from op params
│   ├── ops.go                # `ss ops list|describe`
│   └── tui.go
├── internal/
│   ├── ops/                  # ★ application layer
│   │   ├── operation.go      # Operation, Meta, Param, Input, Result
│   │   ├── view.go           # View union: Diff, Table, Doc, Summary, Tree
│   │   ├── registry.go       # ID → Operation, categories, lookup
│   │   ├── runner.go         # Step(), progress emit, timing, audit, guard
│   │   ├── s3ops/            # version-diff, object-inspect, prefix-audit
│   │   ├── ddbops/           # item-history, gsi-consistency, item-compare
│   │   └── flowops/          # ★ cross-service: record.trace, config.drift
│   ├── diff/                 # ★ semantic JSON diff engine
│   │   ├── json.go  path.go  opts.go  stats.go
│   ├── jsonutil/             # tolerant parse, canonicalize, redact, DDB AV → JSON
│   ├── report/               # Result → markdown / HTML / patch
│   ├── render/               # Result → terminal table / JSON (headless)
│   ├── config/  awsx/  guard/  audit/
│   ├── s3svc/                # port.go + aws.go + fake.go   (version-aware)
│   ├── ddbsvc/               # port.go + aws.go + fake.go
│   ├── kubectl/              # port.go + exec.go + fake.go
│   └── tui/
│       ├── app.go  screen.go  msgs.go
│       ├── keys/  theme/
│       ├── components/       # statusbar, confirm, spinner, errorpanel,
│       │                     #   diffviewer ★, jsontree, progresslist
│       └── screens/          # opspicker, opsform, opsresult, s3browse, k8spods, logstail
└── docs/DESIGN.md
```

### Dependencies

```
charm.land/bubbles/v2         charm.land/lipgloss/v2       spf13/cobra
github.com/aws/aws-sdk-go-v2  (config, service/s3, service/dynamodb,
                               feature/dynamodbav, credentials/stscreds)
golang.org/x/sync/errgroup    # parallel fetches inside an operation
```

Diff is hand-rolled (§6) — the JSON structural diff is ~300 lines and the domain-specific
ignore/array-key rules are the whole value; a library would fight you on those.

---

## 4. Resource ports (infrastructure)

Narrow, consumer-owned interfaces returning **our** types, never SDK types.

```go
// internal/s3svc/port.go — version-aware, because that's the point
type Port interface {
    ListBuckets(ctx context.Context) ([]Bucket, error)
    List(ctx context.Context, bucket, prefix string, tok Token) (Page, error)
    ListVersions(ctx context.Context, bucket, key string) ([]Version, error)
    GetVersion(ctx context.Context, bucket, key, versionID string) (Object, error)
    Head(ctx context.Context, bucket, key string) (ObjectMeta, error)
}

type Version struct {
    ID        string    // "" or "null" = current
    Modified  time.Time
    Size      int64
    ETag      string
    IsLatest  bool
    IsDelete  bool      // delete markers show up in history
}
```

```go
// internal/ddbsvc/port.go
type Port interface {
    ListTables(ctx context.Context) ([]string, error)
    DescribeTable(ctx context.Context, table string) (TableInfo, error)  // key schema, GSIs
    GetItem(ctx context.Context, table string, key Key) (Item, error)
    Query(ctx context.Context, q QuerySpec) (ItemPage, error)
    QueryIndex(ctx context.Context, table, index string, q QuerySpec) (ItemPage, error)
}
```

`Item` is `map[string]any` already unmarshalled out of AttributeValue by `jsonutil` — so
DynamoDB items and S3 JSON objects hit the diff engine as the same shape. That symmetry is
what makes cross-service comparison cheap.

`Deps` is built once in `PersistentPreRunE` and injected into every operation:

```go
type Deps struct {
    S3    s3svc.Port
    DDB   ddbsvc.Port
    K8s   kubectl.Port
    Cache cache.Store        // content-addressed; see §6.4
    Env   config.Environment
}
```

---

## 5. The Operations layer ★

### 5.1 Interface

```go
// internal/ops/operation.go
type Operation interface {
    Meta() Meta
    Params() []Param
    Run(ctx context.Context, r *Runner, in Input) (Result, error)
}

type Meta struct {
    ID       string       // "s3.version-diff"  — stable, used in CLI + audit log
    Name     string       // "Compare object versions"
    Summary  string       // one line, shown in picker
    Category string       // "S3" | "DynamoDB" | "Cross-service" | "Kubernetes"
    Class    guard.Class  // Read | Mutate | Destructive
}
```

Operations are registered at init:

```go
func init() { ops.Register(s3ops.VersionDiff{}) }
```

### 5.2 Params drive both front-ends

```go
type Param struct {
    Name     string
    Label    string
    Type     ParamType    // String, Int, Bool, Enum, Duration, JSON
    Required bool
    Default  string
    Help     string
    // Suggest powers: TUI picker list, Cobra shell completion, and `ops describe`.
    // Later params can depend on earlier ones (bucket → key → versionID).
    Suggest  func(ctx context.Context, d Deps, in Input) ([]Choice, error)
}
```

This is the highest-leverage piece of the design. `s3.version-diff` declares:

```go
func (VersionDiff) Params() []Param {
    return []Param{
        {Name: "bucket", Required: true, Suggest: suggestBuckets},
        {Name: "key",    Required: true, Suggest: suggestKeys},      // depends on bucket
        {Name: "a",      Label: "Version A", Suggest: suggestVersions}, // depends on key
        {Name: "b",      Label: "Version B", Default: "null", Suggest: suggestVersions},
        {Name: "ignore", Label: "Ignore paths", Type: JSON, Help: "glob list"},
    }
}
```

…and gets, with no extra code: a Cobra flag set, `ss run` shell completion, and a TUI form
where "Version A" is an interactive list of that key's actual versions with timestamps.
Nobody copies a version ID by hand.

### 5.3 Runner — steps, progress, guard, audit

```go
func (o VersionDiff) Run(ctx context.Context, r *ops.Runner, in ops.Input) (ops.Result, error) {
    bucket, key := in.Str("bucket"), in.Str("key")

    var a, b s3svc.Object
    err := r.Step("fetch both versions", func(ctx context.Context) error {
        g, gctx := errgroup.WithContext(ctx)
        g.Go(func() (err error) { a, err = r.S3.GetVersion(gctx, bucket, key, in.Str("a")); return })
        g.Go(func() (err error) { b, err = r.S3.GetVersion(gctx, bucket, key, in.Str("b")); return })
        return g.Wait()
    })
    if err != nil { return ops.Result{}, err }

    left,  err := jsonutil.Parse(a.Body);  if err != nil { return ops.Result{}, err }
    right, err := jsonutil.Parse(b.Body);  if err != nil { return ops.Result{}, err }

    delta := diff.JSON(left, right, diff.Opts{
        Ignore:     in.Globs("ignore"),
        ArrayMatch: map[string]string{"$.rules[*]": "id"},
    })

    return ops.Result{Views: []ops.View{
        ops.SummaryView{Fields: []ops.KV{
            {"Object", bucket + "/" + key},
            {"A", a.VersionLabel()}, {"B", b.VersionLabel()},
            {"Changes", delta.Stats.String()},
        }},
        ops.DiffView{Left: a.VersionLabel(), Right: b.VersionLabel(), Delta: delta},
    }}, nil
}
```

`Runner.Step` is a thin template: emits a progress message, times the step, wraps any
error with the step name (`fetch both versions: AccessDenied`), and enforces
context cancellation. Operations stay linear and readable; the cross-cutting concerns live
in one place.

`Runner` also holds the guard and audit hooks — `Class: Mutate` operations can't reach
`Run` without passing `guard`, and every non-`Read` run is written to the audit log with
its resolved params.

### 5.4 Cross-service operations

`flowops` is where the real support value is. Example, `record.trace`:

1. `ddb.GetItem` on the tenant record → yields `configS3Key` and `activeVersion`.
2. `s3.ListVersions` on that key → resolve the previous version.
3. Fetch both, diff.
4. Optionally `kubectl get configmap` and diff *that* against the S3 config to catch drift.

It's still one `Operation` with one `Result`. The front-ends don't know or care that four
services were involved.

### 5.5 Progress → TUI

`Run` executes in a goroutine. `Runner` emits `Progress` on a channel; a self-rescheduling
`tea.Cmd` pumps it into `ProgressMsg`, so a multi-step operation renders as a live
checklist instead of an opaque spinner. Terminating message is `ResultMsg` or `ErrMsg`.

**No operation ever runs inside `Update`.**

---

## 6. Diff engine ★

### 6.1 Model

```go
// internal/diff/
type Delta struct {
    Changes []Change   // sorted by path
    Stats   Stats      // Added, Removed, Changed, TypeChanged counts
}

type Change struct {
    Path     Path       // typed segments, NOT a string
    Kind     Kind       // Added | Removed | Changed | TypeChanged
    Old, New any
}

type Path []Segment    // Segment = Key(string) | Index(int) | MatchKey(field, value)
```

Path as **typed segments** rather than `"a.b[0].c"` is deliberate: it lets the TUI collapse
by subtree, lets ignore-rules match structurally, and lets the exporter render either
dotted notation or JSONPath from the same value.

### 6.2 Options — the part that makes it usable

```go
type Opts struct {
    Ignore     []PathGlob         // "**.updatedAt", "$.metadata.revision", "**.etag"
    ArrayMatch map[string]string  // path glob → identity field, e.g. "$.rules[*]": "id"
    Tolerance  float64            // float comparison epsilon
    TreatNullAsAbsent bool
}
```

Two rules carry almost all the weight:

- **Ignore globs.** Timestamps, revision counters, and checksums change on every write.
  Without suppression every diff is 100% noise. Defaults live in `config.yaml` per
  environment; operations add their own; `--ignore` appends at runtime.
- **Array matching by key.** Comparing arrays by index means inserting one element at
  position 0 marks every subsequent element as changed. Matching `$.rules[*]` by its `id`
  field turns that into a single `Added`. This is the difference between a diff engine
  people trust and one they ignore.

### 6.3 Algorithm

Recursive structural walk over parsed `any` trees. Objects: key set union, recurse on
intersection. Arrays: keyed matching when configured, otherwise LCS over element hashes so
pure insertions/deletions stay clean. Scalars: typed compare, with `TypeChanged` reported
separately from `Changed` — `"5"` → `5` is a schema bug, not a value edit, and support
engineers need to see the difference.

For non-JSON or unparseable bodies, fall back to a line-based unified diff so the operation
degrades instead of erroring.

### 6.4 Caching

S3 object versions are immutable, so fetched bodies are cached content-addressed by
`(bucket, key, versionID)` under `~/.cache/ss/`. Comparing A→B then B→C halves the
fetches, and re-running after tweaking `--ignore` is instant. `--no-cache` bypasses.

---

## 7. Views and renderers ★

One operation, three outputs. `Result.Views` is an ordered union:

```go
type View interface{ isView() }

type SummaryView struct { Fields []KV }
type DiffView    struct { Left, Right string; Delta diff.Delta }
type TableView   struct { Cols []string; Rows [][]string }
type DocView     struct { Title, Body, Lang string }
type TreeView    struct { Root Node }
```

Three renderers **visit** the same views:

| Renderer | Target | Notes |
|---|---|---|
| `tui/screens/opsresult` | interactive | `diffviewer` component: unified/side-by-side toggle, `n`/`p` hunk nav, `c` collapse unchanged, `/` filter by path |
| `render` | stdout | table or `-o json` (the `Delta` marshals directly) |
| `report` | file / clipboard | markdown with fenced diff blocks, or HTML; `--export d.md`, or `e` in the TUI |

Adding a new operation requires zero renderer changes as long as it composes existing view
types. That's the constraint that keeps this from sprawling.

---

## 8. AWS auth · Kubernetes · Safety

**AWS** — SSO / IAM Identity Center plus named-profile assume-role chains. `awsx` calls
`config.LoadDefaultConfig(ctx, WithSharedConfigProfile, WithRegion)`; the SDK already
resolves `sso_session` and `role_arn` chaining. The adapter's real job is failing well: on
startup `sts:GetCallerIdentity`, and on an expired-token error show *"SSO session expired —
run `aws sso login --profile foo`"* with an offer to shell out and do it. Resolved account
ID feeds the guardrail and the status bar (`profile · account · region · cluster`, always
visible).

**Kubernetes** — shell out to `kubectl` with the user's kubeconfig, always `-o json`,
unmarshalled into minimal structs. Streaming ops (`logs -f`) get a `Stream()` returning a
channel. Behind `kubectl.Port`, so a `client-go` implementation can replace it later.

**Safety**, all in `internal/guard`:

1. **Environment allowlist** — `config.yaml` enumerates permitted environments with account
   IDs and kube contexts. Resolved account ID + current kube context are checked at startup
   and again before any `Mutate`/`Destructive` operation. Mismatch is a hard stop with no
   override flag. This is the load-bearing control.
2. **Operation class** — declared in `Meta`. `Mutate` needs `y/n`; `Destructive` needs the
   resource name typed. `--yes` covers `Mutate` only, never `Destructive`.
3. **Audit log** — JSONL to `~/.local/state/ss/audit.log`: timestamp, user, env, account,
   operation ID, resolved params, outcome, duration. Written for every non-`Read` run.

`--dry-run` short-circuits in `Runner` before the first mutating step.

---

## 9. Design patterns used

- **Elm Architecture (Model-View-Update)** — Bubble Tea; state changes pure, effects are
  `tea.Cmd`.
- **Command / Use-Case object** — `Operation` is the application layer's command object:
  named, parameterized, independently testable, front-end agnostic. The single most
  important decision in rev 2.
- **Registry** — `ops.Register` at init; front-ends discover operations rather than
  hardcoding menus or subcommands.
- **Ports & Adapters (Hexagonal)** — `s3svc.Port` etc.; fakes let every operation be
  unit-tested with zero AWS calls.
- **Visitor** — three renderers over the `View` union; new outputs don't touch operations.
- **Template Method** — `Runner.Step` supplies timing, progress, error wrapping, and
  cancellation around each caller-supplied step.
- **Strategy** — diff array-matching (index / keyed / LCS); `kubectl` exec vs `client-go`.
- **Specification** — `diff.PathGlob` ignore rules as composable, configurable predicates.
- **Decorator / Middleware** — `guard` and `audit` wrap operation execution in `Runner`
  rather than each operation remembering to check.
- **Facade** — service layer collapses SDK paginators into `List(...) Page`.
- **Composite + Delegation** — TUI screen stack; also `Path`/`TreeView` are composites.
- **Flyweight / memoization** — content-addressed version cache.
- **Factory + functional options** — `awsx.NewConfig`, `s3svc.New(cfg, WithTimeout(...))`.
- **Null Object** — `audit.Nop`, `guard.AllowAll`, `cache.Nop` for tests.

---

## 10. Phased roadmap

| Phase | Scope | Exit criteria |
|---|---|---|
| **P0 — Foundation** | Cobra root, `config`, `Deps`, `awsx` w/ SSO expiry handling, `ops` interfaces + registry + `Runner`, TUI shell (screen stack, theme, status bar, help, error panel, confirm), `opspicker`/`opsform`/`opsresult` driven by a fake operation, `guard` + `audit` | A fake op appears in `ss ops list` **and** the TUI picker, prompts for params, renders a Summary + Table view. No AWS involved. |
| **P1 — Diff engine** | `diff` (walk, ignore globs, keyed arrays, LCS, stats), `jsonutil`, `diffviewer` component, `report` markdown export | Golden-file tests over fixture JSON pairs pass; `DiffView` renders and exports |
| **P2 — S3 ops** | version-aware `s3svc`, version cache, `s3.version-diff`, `s3.object-inspect`, `s3.prefix-audit`, `s3browse` | Diff two real versions end-to-end in TUI and headless; export to markdown |
| **P3 — DynamoDB ops** | `ddbsvc` + AttributeValue→JSON, `ddb.item-compare`, `ddb.item-history`, `ddb.gsi-consistency` | Compare two items / an item across GSI and base table using the same diff engine |
| **P4 — Cross-service** | `flowops`: `record.trace`, `config.drift`; `kubectl` adapter, `k8spods`, `logstail` | One operation spans DDB → S3 → kubectl and returns a single coherent result |
| **P5 — Remediation** | Guarded mutations: restore prior S3 version, edit DDB item, restart/scale deployment | Every mutation confirmed, audited, blocked outside allowlist; restore reuses the diff as its confirmation preview |
| **P6 — Ergonomics** | Saved param presets (`--preset`/`--save-preset`, `ss presets`, ctrl+p/ctrl+s in the TUI), fuzzy filter in picker and suggestion lists | A saved param set replays by name in both front-ends; "vdiff" finds `s3.version-diff` |
| **P7 — Distribution** | `--version`/self-update, goreleaser + Homebrew tap | Onboarding is `brew install` |

P0 and P1 are the spine and should land together-ish — P1 is pure logic with no I/O, so it
can be built in parallel with P0 by anyone. P2–P4 are independent verticals afterward.

---

## 11. Alternatives considered

Named only — ask if you want any expanded:

- **Declarative YAML operations** (steps + renderer in config, no rebuild to add an op).
  Rejected for v1: you end up writing an interpreter and lose type safety. The `Operation`
  interface is shaped so a YAML-backed implementation can be added later without reshaping
  callers.
- Off-the-shelf diff libraries (`go-cmp` for structural, `sergi/go-diff` for text) instead
  of hand-rolling.
- JSON Patch (RFC 6902) as the wire format for `Delta` instead of a custom type.
- `client-go` instead of shelling out to `kubectl`.
- `huh` forms for the param form instead of hand-rolled `textinput` groups.
- Wish (SSH-served TUI) for centralized, credential-free access.
- A thin backend service holding the credentials, CLI as client — moves guardrails
  server-side where local config edits can't bypass them.
- Plugin architecture (`ss-<verb>` sub-binaries, kubectl-style) instead of one monolith.

---

## 12. Open questions

1. **What are the top 5 real support operations?** The registry design pays off only if the
   op list is concrete. Naming them shapes `flowops` and the default ignore rules.
2. Which JSON fields are pure noise (timestamps, revisions, checksums)? These become the
   shipped default `Ignore` globs — and getting them right is most of whether engineers
   trust the diff.
3. Which arrays need identity-keyed matching, and what's the identity field for each?
4. Do S3 buckets have versioning enabled in every non-prod environment? If not,
   `s3.version-diff` needs a fallback (backup prefix convention? DDB-recorded history?).
5. Are DynamoDB tables discoverable via `ListTables`, or is there a curated set?
6. Do objects/items ever contain PII that must be redacted before export? `jsonutil.Redact`
   is stubbed for this but needs a field list.
7. Audit log local-only, or shipped to CloudWatch/Splunk?
