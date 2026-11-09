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
dependencies) and `crates/ss` (the binary: CLI and AWS adapters).
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

## S3

Three operations read from S3. All are read-only: they never write, copy, or
delete.

```bash
# semantic diff of two versions of one object ("null" = current)
ss run s3.version-diff --bucket my-bucket --key cfg/tenants/acme.json --a <version-id> --b null

# metadata, version history, and a body preview
ss run s3.object-inspect --bucket my-bucket --key cfg/tenants/acme.json

# object count, size, age spread, and the largest objects under a prefix
ss run s3.prefix-audit --bucket my-bucket --prefix cfg/ --top 5
```

`ss ops describe <id>` lists each operation's params.

**Identity.** The selected environment (`--env`, `SS_ENV`, or `default_env`)
supplies the AWS profile and region; `--profile` / `SS_PROFILE` and
`--region` / `SS_REGION` override them. When the environment sets
`account_id`, the caller identity is checked against it before the first S3
call, and a mismatch stops the run. An expired or never-started SSO session,
or no credentials at all, is reported with the `aws sso login --profile
<name>` command to run.

**Cache and limits.** Fetched object versions are cached locally; `--no-cache`
bypasses the cache. Bodies over 32 MiB are refused.

**S3-compatible endpoints.** With `SS_S3_ENDPOINT` set, S3 calls go to that
endpoint using path-style addressing, and the identity check is skipped.

### Trying it against MinIO

```bash
docker run -d --rm --name ss-minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio server /data

AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin \
  SS_REGION=us-east-1 SS_S3_ENDPOINT=http://127.0.0.1:9000 \
  ss run s3.prefix-audit --bucket <bucket>
```

`SS_S3_TEST_ENDPOINT=http://127.0.0.1:9000 make contract` runs the S3
contract suite against the same container.
