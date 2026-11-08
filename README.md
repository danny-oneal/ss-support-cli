# ss

Support operations CLI for non-production AWS and Kubernetes resources,
ported from an earlier implementation.

## Build

```bash
make check                          # fmt + clippy (-D warnings) + tests
cargo build --release               # target/release/ss
cargo install --path crates/ss      # install ss into ~/.cargo/bin
cargo run -p ss -- --version
```

Crates: `crates/ss-core` (diff engine, operations layer, ports; no AWS
dependencies) and `crates/ss` (the binary: CLI, TUI, AWS adapters).
`make help` lists every target; `docs/` holds the design notes, testing
plan, and an example config.

## CLI

```bash
ss ops list                                   # every operation
ss ops describe demo.json-diff                # its params, defaults, help
ss run demo.json-diff --left a.json --right b.json -o json
ss presets list                               # saved parameter sets
```

Global flags work before or after the subcommand.

| flag | overrides | notes |
|---|---|---|
| `--env NAME` | `SS_ENV`, then `default_env` in the config | writes are refused unless `NAME` is in the config's `environments` |
| `--profile NAME` | `SS_PROFILE`, then the environment's `profile` | AWS profile |
| `--region REGION` | `SS_REGION`, then the environment's `region` | AWS region |
| `--context KUBE_CONTEXT` | the environment's `kube_context` | Kubernetes context |
| `-o, --output FORMAT` | `text` | `text` or `json` (`{"views":[...]}`); anything else exits 2 before running |
| `--export FILE` | | after a successful run, writes a markdown report with the resolved params |
| `--dry-run` | | stops before the first mutating step; exits 0 |
| `--yes` | | auto-confirms mutating operations, never destructive ones |
| `-v, --verbose` | | step progress on stderr |
| `--no-cache` | | bypasses the local object-version cache |
| `--preset NAME` (`ss run`) | | loads a saved param set; flags given on the command line win |
| `--save-preset NAME` (`ss run`) | | after a successful run, saves the params given (not the defaults); with none given, exits 2 before running |

Op flags take values that start with a hyphen (`--repeat -1`); a flag given
twice takes its last value.

Exit codes: `0` ok (including `--dry-run`) · `1` the operation failed, was
denied, or was declined, or a file could not be read or written · `2` usage
error (bad flag or param value, unknown operation or format, missing required
param, unknown preset, nothing to save with `--save-preset`) or an unparsable
config/presets file · `130` interrupted by Ctrl-C, SIGTERM, or SIGHUP (a second
signal exits at once). A failed audit-log write is a `warning:` line on stderr
and does not change the exit code.
