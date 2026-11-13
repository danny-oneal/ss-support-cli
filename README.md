# ss-support

Support operations CLI for non-production AWS and Kubernetes resources,
ported from an earlier implementation.

## Build

```bash
make check                          # fmt + clippy (-D warnings) + tests
cargo build --release               # target/release/ss-support
cargo install --path crates/ss-support      # install ss-support into ~/.cargo/bin
cargo run -p ss-support -- --version
```

Crates: `crates/ss-core` (diff engine, operations layer, ports; no AWS
dependencies) and `crates/ss-support` (the binary: CLI, AWS adapters, and the
kubectl adapter).
`make help` lists every target; `docs/` holds the design notes, testing
plan, and an example config.

## CLI

```bash
ss-support ops list                                   # every operation
ss-support ops describe demo.json-diff                # its params, defaults, help
ss-support run demo.json-diff --left a.json --right b.json -o json
ss-support presets list                               # saved parameter sets
```

Global flags work before or after the subcommand.

| flag | overrides | notes |
|---|---|---|
| `--env NAME` | `SS_SUPPORT_ENV`, then `default_env` in the config | writes are refused unless `NAME` is in the config's `environments` |
| `--profile NAME` | `SS_SUPPORT_PROFILE`, then the environment's `profile` | AWS profile |
| `--region REGION` | `SS_SUPPORT_REGION`, then the environment's `region` | AWS region |
| `--context KUBE_CONTEXT` | the environment's `kube_context` | Kubernetes context; every kubectl call is pinned to it. Without one, any step that needs Kubernetes fails (exit 1) and kubectl is not started |
| `-o, --output FORMAT` | `text` | `text` or `json` (`{"views":[...]}`); anything else exits 2 before running |
| `--export FILE` | | after a successful run, writes a markdown report with the resolved params |
| `--dry-run` | | stops before the first mutating step; exits 0 |
| `--yes` | | auto-confirms mutating operations, never destructive ones |
| `-v, --verbose` | | step progress on stderr |
| `--no-cache` | | bypasses the local object-version cache |
| `--preset NAME` (`ss-support run`) | | loads a saved param set; flags given on the command line win |
| `--save-preset NAME` (`ss-support run`) | | after a successful run, saves the params given (not the defaults); with none given, exits 2 before running |

Op flags take values that start with a hyphen (`--repeat -1`); a flag given
twice takes its last value.

Exit codes: `0` ok (including `--dry-run`) · `1` the operation failed, was
denied, or was declined, or a file could not be read or written · `2` usage
error (bad flag or param value, including a DynamoDB key that is malformed or
does not match the table's key, an unknown `record.trace` domain, malformed
`--vars`, missing sort vars, an ambiguous or unknown ConfigMap data key, an
empty or `-`-leading ConfigMap name, unknown operation or format, missing
required param, unknown preset, nothing to save with `--save-preset`) or an
unparsable
config/presets file · `130` interrupted by Ctrl-C, SIGTERM, or SIGHUP (a second
signal exits at once). A failed audit-log write is a `warning:` line on stderr
and does not change the exit code.

## TUI

`ss-support` with no subcommand opens an interactive front-end when stdin and
stdout are both terminals; piped or redirected, it prints the help instead.
It runs the same operations through the same environment allowlist,
confirmation, and audit log as `ss-support run`.

```bash
ss-support                      # pick an operation, fill in its params, run it
ss-support --env staging        # the same, against another environment
ss-support --dry-run            # every run stops before its first mutating step
```

`--env`, `--profile`, `--region`, `--context`, `--no-cache`, `--dry-run`, and
`--yes` apply as they do to `ss-support run`. `-o`, `--export`, and `-v` have
no effect here: results are shown on screen and exported with `e`.

| screen | keys |
|---|---|
| Operations | `↑` `↓` move · `enter` open · `/` filter by name (then `↑` `↓`, `enter`, `esc` to clear) · `b` browse S3 · `q` or `esc` quit |
| Form | `tab` `shift+tab` `↑` `↓` move between params · `↓` into the suggestions, `enter` to take one · `enter` run · `ctrl+p` load a preset · `ctrl+s` save a preset · `esc` back |
| Run | `ctrl+c` or `esc` cancel · `esc` back once it has ended |
| Confirm | `y` / `n` for a mutating operation · type the resource name and `enter` for a destructive one · `esc` decline |
| Result | `tab` `shift+tab` next/previous view · `↑` `↓` `←` `→` `pgup` `pgdn` `g` scroll · `e` export a markdown report · `ctrl+s` save a preset · `esc` back |
| Diff view | `n` `p` next/previous change · `pgup` `pgdn` ten changes · `g` first change · `s` unified or side-by-side · `c` one line per change · `/` filter by path |
| S3 browser | `↑` `↓` move · `enter` open a bucket, prefix, or an object's versions · `space` mark a version · `d` compare the two marked versions · `r` reload · `esc` up |

`ctrl+c` cancels a run, declines a confirmation, quits from the operation
list, and goes back one screen anywhere else. A form shows a param's default
as `default: …` rather than filling it in, so a saved preset holds only the
values that were typed. `e` writes `ss-support-<operation>-<timestamp>.md`
into the working directory. `d` in the S3 browser opens `s3.version-diff`
with the bucket, key, and both version ids filled in, the older version as
`a`.

Leaving the TUI exits `0`; SIGTERM or SIGHUP exits `130`. The terminal is
restored on every way out. A terminal smaller than 20x6 shows only
`terminal too small`.

## S3

Three operations read from S3. All are read-only: they never write, copy, or
delete.

```bash
# semantic diff of two versions of one object ("null" = current)
ss-support run s3.version-diff --bucket my-bucket --key cfg/tenants/acme.json --a <version-id> --b null

# metadata, version history, and a body preview
ss-support run s3.object-inspect --bucket my-bucket --key cfg/tenants/acme.json

# object count, size, age spread, and the largest objects under a prefix
ss-support run s3.prefix-audit --bucket my-bucket --prefix cfg/ --top 5
```

`ss-support ops describe <id>` lists each operation's params.

**Identity.** The selected environment (`--env`, `SS_SUPPORT_ENV`, or `default_env`)
supplies the AWS profile and region; `--profile` / `SS_SUPPORT_PROFILE` and
`--region` / `SS_SUPPORT_REGION` override them. When the environment sets
`account_id`, the caller identity is checked against it before the first S3
call, and a mismatch stops the run. An expired or never-started SSO session,
or no credentials at all, is reported with the `aws sso login --profile
<name>` command to run.

**Cache and limits.** Fetched object versions are cached locally; `--no-cache`
bypasses the cache. Bodies over 32 MiB are refused.

**S3-compatible endpoints.** With `SS_SUPPORT_S3_ENDPOINT` set, S3 calls go to that
endpoint using path-style addressing, and the identity check is skipped;
credentials are still resolved first.

### Trying it against MinIO

MinIO publishes no anonymously pullable image; build the server from source
(needs Go 1.24 or newer):

```bash
go install github.com/minio/minio@RELEASE.2025-10-15T17-29-55Z
mkdir -p /tmp/ss-minio
MINIO_ROOT_USER=minioadmin MINIO_ROOT_PASSWORD=minioadmin \
  "$(go env GOPATH)/bin/minio" server /tmp/ss-minio --address 127.0.0.1:9000 &

export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1

# ss-support only reads: create and seed a versioned bucket with the AWS CLI
aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://demo
aws --endpoint-url http://127.0.0.1:9000 s3api put-bucket-versioning \
  --bucket demo --versioning-configuration Status=Enabled
aws --endpoint-url http://127.0.0.1:9000 s3 cp ./acme.json s3://demo/cfg/acme.json

SS_SUPPORT_REGION=us-east-1 SS_SUPPORT_S3_ENDPOINT=http://127.0.0.1:9000 \
  ss-support run s3.prefix-audit --bucket demo --prefix cfg/
```

`SS_SUPPORT_S3_TEST_ENDPOINT=http://127.0.0.1:9000 make contract` runs the S3
contract suite against the same server.

## DynamoDB

Three operations read from DynamoDB. All are read-only: they never put,
update, or delete an item. Item fetches and base-table queries (including the
version listing in `ddb.item-history`) are strongly consistent reads; index
(GSI) queries are eventually consistent, because DynamoDB does not support
consistent reads on a GSI, and that lag is what `ddb.gsi-consistency` exposes.
Read-only operations are not written to the audit log.

```bash
# list an item's versions (V0 = latest) and diff two of them (default: V1 -> V0)
ss-support run ddb.item-history --table tenants --pk acme --a V2 --b V0

# semantic diff of two items; a key is a JSON object ...
ss-support run ddb.item-compare --table tenants \
  --key-a '{"TenantId":"acme","Version":"V0"}' --key-b '{"TenantId":"acme","Version":"V1"}'

# ... or Name=value pairs separated by commas (values containing commas need the JSON form)
ss-support run ddb.item-compare --table tenants \
  --key-a TenantId=acme,Version=V0 --key-b TenantId=acme,Version=V1

# does a global secondary index entry match its base-table item?
ss-support run ddb.gsi-consistency --table tenants --index TierIndex --key TenantId=acme,Version=V0
```

`ss-support ops describe <id>` lists each operation's params. A missing
required param, or a key that is malformed or does not hold exactly the table's
key attributes (hash key, plus sort key if the table has one), exits 2; a
missing item exits 1. Either way there is a single error line. Tables whose
key has a binary attribute are not supported. `ddb.item-compare` also takes
`--table-b` to compare across tables and `--ignore` for extra ignore globs.

**Versions.** `ddb.item-history` treats the sort key as a version history:
the version label lives in the `Version` attribute (`--version-attr` names a
different one), and `V0` is the latest. All versions under the key are read
(up to 10 000), and the table shows the first `--limit` of them after sorting
latest-first (default 50, clamped to 1..=10000). `--a` and `--b` pick the two
versions to diff, and a requested version is found even when it falls beyond
the rows shown.

**Index checks.** `ddb.gsi-consistency` finds the item's entry in the index
and compares it with the base item: the whole item for an `ALL` index, the
table and index keys for `KEYS_ONLY`, and the attributes the entry carries
otherwise. The answer is `consistent`,
`DRIFT` (the entry differs from the item), `MISSING` (no entry for the item),
`INCONCLUSIVE` (no entry among the first 10 000 candidates under the index
key, so one may exist further on), or a note that the item lacks the index's
key attribute and so is not projected into it.

**Output.** Sets are shown as plain arrays and binary values as base64
strings.

**Identity.** The selected environment (`--env`, `SS_SUPPORT_ENV`, or `default_env`)
supplies the AWS profile and region; `--profile` / `SS_SUPPORT_PROFILE` and
`--region` / `SS_SUPPORT_REGION` override them. When the environment sets
`account_id`, the caller identity is checked against it before the first
DynamoDB call, and a mismatch stops the run. An expired or never-started SSO
session, or no credentials at all, is reported with the `aws sso login
--profile <name>` command to run.

**DynamoDB-compatible endpoints.** With `SS_SUPPORT_DDB_ENDPOINT` set,
DynamoDB calls go to that endpoint and the identity and account check is
skipped; credentials are still resolved first.

### Trying it against dynalite

[dynalite](https://github.com/architect/dynalite) is a local DynamoDB
implementation (needs Node.js):

```bash
npx --yes dynalite@3.2.2 --port 8000 &

export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1

# ss-support only reads: create and seed the table with the AWS CLI
aws --endpoint-url http://127.0.0.1:8000 dynamodb create-table --table-name tenants \
  --attribute-definitions AttributeName=TenantId,AttributeType=S \
    AttributeName=Version,AttributeType=S AttributeName=Tier,AttributeType=S \
  --key-schema AttributeName=TenantId,KeyType=HASH AttributeName=Version,KeyType=RANGE \
  --global-secondary-indexes 'IndexName=TierIndex,KeySchema=[{AttributeName=Tier,KeyType=HASH}],Projection={ProjectionType=ALL}' \
  --billing-mode PAY_PER_REQUEST
aws --endpoint-url http://127.0.0.1:8000 dynamodb put-item --table-name tenants \
  --item '{"TenantId":{"S":"acme"},"Version":{"S":"V0"},"Tier":{"S":"platinum"},"limits":{"M":{"rps":{"N":"200"}}}}'
aws --endpoint-url http://127.0.0.1:8000 dynamodb put-item --table-name tenants \
  --item '{"TenantId":{"S":"acme"},"Version":{"S":"V1"},"Tier":{"S":"gold"},"limits":{"M":{"rps":{"N":"100"}}}}'

SS_SUPPORT_REGION=us-east-1 SS_SUPPORT_DDB_ENDPOINT=http://127.0.0.1:8000 \
  ss-support run ddb.item-history --table tenants --pk acme
```

`SS_SUPPORT_DDB_TEST_ENDPOINT=http://127.0.0.1:8000 make contract-ddb` runs
the DynamoDB contract suite against the same server.

## Cross-service

Two operations follow a configuration across services. Both are read-only:
they never write to S3, DynamoDB, or the cluster, and, like every read, they
are not written to the audit log.

```bash
# does the cluster run the config stored in S3? (--configmap name[:data-key])
ss-support run config.drift --bucket my-bucket --key menu/12345.json \
  --configmap menu-config:menu.json --namespace menu

# a specific S3 version instead of the current one
ss-support run config.drift --bucket my-bucket --key menu/12345.json --version <version-id> \
  --configmap menu-config:menu.json --namespace menu

# follow a DynamoDB record to its S3 config and the version before it;
# --configmap also diffs the live ConfigMap, --version picks the record version (V0 = latest)
ss-support run record.trace --domain menu --id 12345 \
  --vars locale=en-US,channel=mobile --configmap menu-config:menu.json
```

`config.drift` reports `in sync` or `DRIFT (...)` with the diff. `record.trace`
reads the record, fetches the S3 object version the record names, diffs it
against the previous version (delete markers are skipped), and, with
`--configmap`, against the live ConfigMap. `--namespace` defaults to
`default` for `config.drift` and to the domain's `namespace` for
`record.trace`; an empty namespace uses the context's default.
`--configmap name` works when the ConfigMap holds one data key; use
`name:data-key` when it holds several. `ss-support ops describe <id>` lists
each operation's params.

**Record domains.** `record.trace` reads its record shapes from
`trace.domains` in `config.yaml`; `docs/config.example.yaml` shows the
fields. Only deviations from the common shape need spelling out:
`hash_attr` defaults to `hash`, `version_attr` to `version`, `s3_key_attr` to
`key`, and `s3_version_attr` to `versionId`. `table` and `bucket` may use an
`{env}` template, and per-environment `tables` and `buckets` maps override it.
A `sort_format` such as `{locale}#{channel}` is filled from `--vars`
(`name=value` pairs separated by commas).

**Kubernetes access.** ConfigMaps are read through `kubectl`, which must be on
`PATH` (otherwise the run stops with `kubectl not found at "kubectl" (install
kubectl or put it on PATH)`). Every call is pinned to the environment's
`kube_context` (or `--context`); kubectl's own current context is never used.
With neither, the run exits 1 with `no Kubernetes context for environment
"<env>": set kube_context in config.yaml or pass --context`, and kubectl is
not started. Only `kubectl get` and `kubectl logs` are ever executed, and
kubectl output over 32 MiB is refused. Resource, ConfigMap, and pod names that
are empty or start with `-` are refused before kubectl runs, so a name can
never act as a kubectl flag. A kubectl failure is reported as one line: the
last non-empty line of its stderr.

**Exit codes.** A bad domain, malformed `--vars`, missing sort vars, an
ambiguous or unknown ConfigMap data key, an empty or `-`-leading ConfigMap
name, or a missing required param exits 2. A missing record, S3 object, or ConfigMap, or
ConfigMap data that is not JSON, exits 1. Either way there is a single error
line.
