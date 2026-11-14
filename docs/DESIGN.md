# ss-support — Design

How the program is built and why. For how to use it, see the README; for
every configuration key, `configuration.md`.

## 1. Purpose

One terminal tool for support engineers to diagnose, and in two narrow cases
repair, **non-production** AWS and Kubernetes resources. The unit of work is
a *support operation*: a named, parameterised task that may touch several
services and returns a structured result. It is not a wrapper around single
API calls.

Goals:

- One binary. The only outside programs it needs are `kubectl` and a working
  AWS profile.
- Multi-step operations as first-class units that can be tested without a
  cloud.
- An interactive front-end for exploring, and the same operations runnable
  from a script.
- Results that read well in a terminal and can be pasted into a ticket.
- Guardrails that make a write outside the listed environments impossible,
  and a write inside them deliberate.

Not goals: production access, a general AWS client, or anything that changes
a Kubernetes cluster.

## 2. Two front-ends, one set of operations

```
ss-support run <operation> --flag value …      command line, for scripts
ss-support                                     TUI, when stdin and stdout are terminals
```

Both build the same `Runner` around the same `Operation`, so the environment
allowlist, the confirmation, `--dry-run`, and the audit log behave the same
way in both. They differ in one deliberate place: `--yes` can confirm a
mutating operation on the command line, and never in the TUI, where the
operator is present and is always asked.

`ss-support ops list|describe`, `presets list|show|delete`, and `completion
<shell>` are the fixed subcommands beside `run`.

## 3. Crates and layers

```
crates/ss-core      everything testable without a terminal or a cloud
  diff/             the comparison engine
  ops/              Operation, Param, Input, View, Registry, Runner, the operations
  svc/              ports: S3, DynamoDB, kubectl; fakes; contract suites; decorators
  config/ guard audit cache preset vars render report jsonutil fuzzy

crates/ss-support   the binary
  cli/ cmd/ run     clap tree, subcommands, exit codes, signals
  aws/ kubectl      adapters that implement the ports
  confirm io        the command-line confirmation and injectable stdio
  tui/              shell, screens, components
```

`ss-core` has no AWS dependency. The binary's `main.rs` only parses and
calls into the library, so tests drive the whole program in-process.

Dependencies point one way: operations depend on ports, adapters implement
ports, front-ends depend on operations. Nothing in `ss-core` reads the
process environment; it reads a `Vars` snapshot that tests replace.

## 4. Ports and adapters

Each outside system is a trait in `ss-core/src/svc`:

| Port | Reads | Writes |
|---|---|---|
| `S3Port` | buckets, listings, version history, a version's body, metadata, whether the bucket keeps versions | `copy_version`: a server-side copy of one version onto its key |
| `DdbPort` | tables, table description, an item, key-condition queries on a table or index | `update_item`: set and remove attributes of one item under a condition |
| `KubectlPort` | one resource as JSON, a listing as JSON, a pod's log as a stream | none, by design |

Around each port:

- a **fake** (`FakeS3`, `FakeDdb`, `FakeKubectl`) that keeps data in memory;
- an **adapter** in the binary (`aws/s3.rs`, `aws/ddb.rs`, `kubectl.rs`);
- for S3 and DynamoDB, a **contract suite**: a set of assertions any
  implementation must pass, run against the fake in unit tests and against
  MinIO and dynalite in CI;
- for S3 and DynamoDB, **decorators**: `Lazy` builds the adapter (and
  resolves credentials) only when an operation first needs it, and `Cached`
  keeps fetched S3 versions on disk, since a version id always names the
  same content.

The kubectl adapter runs the `kubectl` binary, always with an explicit
context, and only ever `get` and `logs`. Every child process is tracked, and
every path that ends the program stops the ones still running.

A port error is `SvcError`: `NotFound`, `Changed` (what a write was based on
is no longer true), or anything else kept whole.

## 5. Operations

An `Operation` declares what it is and what it takes, and implements one
method:

```rust
fn meta(&self) -> Meta;              // id, name, summary, category, class
fn params(&self) -> Vec<Param>;      // name, label, type, default, help, suggestions
async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult>;
```

- **Registration is explicit**: `ops::builtin()` lists every operation.
- **Params drive both front-ends.** The command line generates one `run
  <id>` subcommand per operation with one flag per param; the TUI builds a
  form from the same list. Defaults and required params are enforced by the
  runner, after a preset has been merged, not by the argument parser.
- **`Input` is a lenient string map**, which is what a command line, a form,
  and a presets file can all produce.
- **Suggestions** are functions from the dependencies and the input so far
  to a list of choices, which lets a form offer buckets, then keys under the
  typed prefix, then that key's versions.

The operations, by what they are for:

| Id | Class | Does |
|---|---|---|
| `s3.version-diff` | read | compares two versions of one object |
| `s3.object-inspect` | read | metadata, history, and a preview of one object |
| `s3.prefix-audit` | read | counts, sizes, and ages under a prefix |
| `s3.restore-version` | mutate | makes an earlier version current again |
| `ddb.item-compare` | read | compares two items |
| `ddb.item-history` | read | lists an item's versions and compares two |
| `ddb.gsi-consistency` | read | compares an item with its projection in an index |
| `ddb.item-edit` | destructive | sets and removes attributes of one item |
| `record.trace` | read | follows a record from DynamoDB to the object it names |
| `config.drift` | read | compares an S3 config object with a live ConfigMap |
| `demo.hello`, `demo.json-diff` | read | exercise the pipeline without a cloud |

## 6. The runner is the safety model

Every run goes through `Runner::execute`:

1. Fill defaults; a missing required param is a usage error.
2. Ask the **guard**. An operation that is not a read runs only in an
   environment listed in the config file. Nothing overrides this: there is
   no flag, and it is checked before the operation runs, so neither service
   is reached.
3. Run the operation. It does its work through two calls:
   - `r.step(name, …)` for reads: progress, timing, cancellation, and the
     step's name on any error.
   - `r.mutate(name, …)` for the change. A read-class operation that calls
     it is stopped. A dry run stops here. Otherwise the first `mutate` of a
     run asks for confirmation, showing whatever the operation registered
     with `r.preview(…)`.
4. Record one **audit** entry for every run that is not a read: who, where,
   what, with which params, the outcome, and how long it took.

Confirmation depends on the operation's class:

| Class | Command line | TUI |
|---|---|---|
| read | never asked | never asked |
| mutate | `y`, or `--yes` | `y` and enter |
| destructive | the resource name typed back; `--yes` does not count | the resource name and enter |

The question is asked in the middle of the run, not before it, so that what
it shows is real: the diff between what is stored and what will be stored.

**Writes are made only on what was shown.** The restore reads the history
again after the answer and stops if the current version has changed; it runs
only where the bucket has versioning enabled, because elsewhere the copy
could overwrite content for good. The item edit is conditional in DynamoDB
on the stored values of the attributes it touches. Either refuses with
`changed since it was read`.

**A write's preview hides nothing.** The ignore paths and keyed array
matching that make read comparisons quiet are not applied to it.

Exit codes: `0` ok (a dry run included), `1` failed, denied, or declined,
`2` the input must be fixed, `130` interrupted.

## 7. The diff engine

`ss-core/src/diff` compares two JSON values semantically:

- **Typed paths** (`rules[2].limit`), and **ignore globs** over them
  (`**.updatedAt`).
- **Arrays** are aligned by a named identity field where the config says so
  (`$.rules[*]: id`), so that one insertion is one change, and by longest
  common subsequence otherwise.
- **Numbers** are compared as doubles, with an optional tolerance, and
  printed the same way everywhere. Null can be treated as absent.
- Bodies that are not both JSON fall back to a **line diff**.

JSON object keys are kept sorted, which is what makes the canonical form
canonical. The engine's output for a set of fixture pairs is pinned in
`crates/ss-core/tests/fixtures/diff/`.

## 8. Views and renderers

An operation returns `View`s, a closed set: `Summary` (fields), `Diff`,
`Table`, `Doc` (text with a language), and `Tree`. Because the set is
closed, adding a variant is a compile error in every renderer until it is
handled. Three renderers consume it: plain text, `-o json`
(`{"views":[…]}`), and the markdown report that `--export` and the TUI's `e`
write.

## 9. The TUI

`ratatui` on `crossterm`. `tui/shell.rs` owns a stack of screens and one
event loop.

- A screen is a small state machine: `init`, `update(msg) -> Vec<Cmd>`,
  `view`. **`update` never calls a service.** It returns commands (`Push`,
  `Pop`, `Replace`, `Task`, `Run`, `Stream`, …); the shell spawns the work
  and routes the result back to the screen that asked, or drops it if that
  screen has gone. Loads and streams end with their screen.
- Runs go through `tui/bridge.rs`, which builds the same runner as the
  command line; progress and the confirmation question arrive as messages.
- **Input already on its way never answers something new.** A confirmation
  is a typed answer and enter; keys are dropped for a moment after a
  question or a result appears; terminal events are handled before
  background results; a paste is only ever text for a focused field.
- **Streams** (a followed log) are read one batch at a time, and a screen
  that shows one keeps a buffer bounded in lines and in bytes. A screen that
  is only receiving lines is redrawn at most ten times a second.
- Text from outside is cleaned before it is drawn: tabs expanded, control
  characters dropped, colour codes stripped.
- The terminal is restored on every way out, including a panic and a second
  signal.

Screens: the operation picker, the form, the run with its confirmation, the
result (one view at a time, with a diff viewer), the S3 browser, the pod
list, and the log tail.

## 10. What is deliberately absent

- **No production access.** The allowlist is the only gate, and it is not a
  list of things to avoid but of places where writing is possible at all.
- **No Kubernetes writes.** A test fails if the port gains a write method or
  the adapter names a write verb.
- **No general write API.** Each port has exactly one write, shaped so the
  service does the careful part, and a test pins the list of operations
  that are not reads.
- **No plug-in or auto-registration mechanism.** The registry is one
  function that can be read top to bottom.
- **No release pipeline.** The program is installed from a checkout with
  `cargo install`.
