# ss-support — Testing

What is tested, at which layer, how to run it, and what is not tested.

## Running the tests

```bash
make check                                   # what CI runs: fmt --check, clippy -D warnings, every test
make msrv                                    # type-check on the minimum supported Rust
cargo test -p ss-core <name>                 # one library test
cargo test -p ss-support --test e2e <name>   # one end-to-end test (also: s3, ddb, flow, writes, tui, signals, docs)
make contract                                # S3 contract suite against MinIO      (SS_SUPPORT_S3_TEST_ENDPOINT)
make contract-ddb                            # DynamoDB contract suite against dynalite (SS_SUPPORT_DDB_TEST_ENDPOINT)
```

CI runs four jobs on every pull request: test and lint, the minimum Rust
version, the S3 contract against MinIO, and the DynamoDB contract against
dynalite. The two contract suites are skipped locally unless their endpoint
variable is set.

## The layers

| Layer | Where | What it proves |
|---|---|---|
| Unit tests | beside the code in both crates | one function or type: path parsing, glob matching, key parsing, the guard, presets, text cleaning, every screen and component |
| Diff fixtures | `crates/ss-core/tests/diff_golden.rs` and `tests/fixtures/diff/` | the comparison engine's output for whole documents, byte for byte |
| Runner tests | `crates/ss-core/tests/runner.rs` | the safety model with scripted operations: defaults, the guard, confirmation by class, dry run, cancellation, the audit entry for each outcome |
| Operation tests | beside each operation | the operation against an in-memory port, including what it previews and what it refuses |
| Port contracts | `ss-core/src/svc/*/contract.rs` | what any implementation of a port must do; run against the fakes, the decorators, MinIO, and dynalite |
| Adapter tests | `ss-support/src/aws/*.rs`, `kubectl.rs` | request building and error mapping; the kubectl adapter against a stand-in script |
| End-to-end, command line | `ss-support/tests/{e2e,s3,ddb,flow,writes}.rs` | the whole pipeline in-process: arguments in, exit code, stdout, and stderr out |
| End-to-end, TUI | `ss-support/tests/tui.rs` | the whole event loop on an in-memory terminal, fed keys, waiting for text |
| Real binary | `ss-support/tests/signals.rs` | the built program under signals, and on a pseudo-terminal with a stand-in kubectl |
| Documents | `ss-support/tests/docs.rs` | every example in the configuration reference loads, and every config key is in it |

## How the hard parts are tested

**Without a cloud.** Operations depend on ports, and every port has a fake
that keeps its data in memory. `Services::isolated(dir)` points the home
directory at a temp dir, so the config file, presets, audit log, and cache
of a test land there and nowhere else.

**The fakes are held to the adapters' standard.** A port's contract suite is
one set of assertions run against the fake and against a real server through
the adapter. A fake that drifts from the service fails the same suite.

**Writes.** Each write has a test that changes the stored state between the
preview and the write and expects a refusal with nothing written. The
allowlist tests count whether the port was even built, so they show that a
refused run reaches no service. `tests/writes.rs` pins the list of
operations that are not reads. For DynamoDB, one test sends a request built
from a stale reading to the server, so the server's own refusal is seen.

**The TUI.** Screens are state machines, so most tests send messages and
read the rendered text; snapshots under `snapshots/` are plain text and are
read when they change. The end-to-end tests run the real loop and wait for
text to appear, never for a fixed time. Tests that depend on timing run on a
paused clock.

**Processes and signals.** `tests/signals.rs` starts the built binary, sends
it signals, and checks the exit code and the output. On Linux it also runs
the TUI on a pseudo-terminal (from `script`, part of util-linux) with a
stand-in `kubectl` first on `PATH`, and checks that the stand-in is gone when
the TUI ends. The stand-in ignores hangups, so that only the program
stopping it can end it.

**A test must be able to fail.** A test added for a defect is run against
the code without the fix first. Where a review found a test that passed with
the guarded code removed, the test was rewritten until removing the code
failed it.

## What is not tested

- **Real AWS.** No test uses an AWS account. MinIO stands in for S3 and
  dynalite for DynamoDB.
- **Conditions on map and list attributes in DynamoDB.** dynalite never
  finds two maps or two lists equal in a condition, so an edit of such an
  attribute is tested against the in-memory store only.
- **A real cluster.** The kubectl adapter is tested against a script that
  stands in for `kubectl`. How a real `kubectl` words its failures rests on
  knowledge of it.
- **zsh and fish completion behaviour.** The bash script is asked for
  completions by bash itself. The zsh script is loaded by zsh but not asked;
  the fish script is checked for content only.
- **Windows.** The signal and pseudo-terminal tests are Unix or Linux only.
- **Colours and real terminals.** The TUI is tested on an in-memory terminal;
  how it looks in a particular terminal emulator is not.
- **Coverage** is not measured.
