# Configuration reference

`ss-support` reads two files, both optional and both YAML: `config.yaml`,
which says where it may write and how it compares, and `presets.yaml`, which
holds saved parameters. It writes two things besides: an audit log and a
cache of object versions.

Every example on this page marked `yaml config` or `yaml presets` is a whole
file, and a test loads each one with the program's own loader
(`crates/ss-support/tests/docs.rs`).

## Where things live

| What | Location | Moved by |
|---|---|---|
| Config file | `~/.config/ss-support/config.yaml` | `SS_SUPPORT_CONFIG` (a file path), else `XDG_CONFIG_HOME` |
| Presets file | `~/.config/ss-support/presets.yaml` | `SS_SUPPORT_PRESETS` (a file path), else `XDG_CONFIG_HOME` |
| Audit log | `~/.local/state/ss-support/audit.log` | `XDG_STATE_HOME` |
| Cache of object versions | `~/.cache/ss-support/` | `XDG_CACHE_HOME`; `--no-cache` bypasses it for one run |

A config file that is missing, empty, or only comments is the same as no
configuration: every read works, and no environment is on the allowlist, so
nothing can be written. A file that does not parse, or that has a key this
page does not list, stops the program with exit code 2 and the offending key
or line. A file that exists and cannot be read is exit code 1.

## What decides a setting

For the environment, the AWS profile, and the AWS region, the first of these
that is set wins:

| Setting | Flag | Variable | Config file |
|---|---|---|---|
| Environment | `--env` | `SS_SUPPORT_ENV` | `default_env` |
| AWS profile | `--profile` | `SS_SUPPORT_PROFILE` | the environment's `profile` |
| AWS region | `--region` | `SS_SUPPORT_REGION` | the environment's `region` |
| Kubernetes context | `--context` | none | the environment's `kube_context` |

Two more variables point the AWS calls somewhere else, for a local stand-in:
`SS_SUPPORT_S3_ENDPOINT` (MinIO, for example) and `SS_SUPPORT_DDB_ENDPOINT`
(dynalite or DynamoDB Local). With either set, the account check described
below is skipped for that service.

## `config.yaml`

The smallest useful file names one environment, which puts it on the
allowlist:

```yaml config
default_env: dev
environments:
  dev: {}
```

### Top level

| Key | Type | Default | Meaning |
|---|---|---|---|
| `default_env` | string | none | The environment used when neither `--env` nor `SS_SUPPORT_ENV` names one. |
| `environments` | map of name to environment | empty | The allowlist. Only an environment named here may run `s3.restore-version` or `ddb.item-edit`; there is no flag that overrides this. |
| `diff` | map | empty | How comparisons are made; see below. |
| `trace` | map | empty | Where `record.trace` and `config.drift` find records. Its one key, `domains`, holds an entry per kind of record; see below. |

### `environments.<name>`

Every key is optional. An entry with none (`dev: {}`) still puts the
environment on the allowlist.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `profile` | string | the SDK's default chain | The AWS named profile. SSO and assume-role chains are resolved by the SDK. |
| `region` | string | the profile's region | The AWS region. |
| `account_id` | string | none | When set, the account the credentials resolve to is checked against it before any S3 or DynamoDB call, and a mismatch stops the run. Quote it, so that a leading zero survives. |
| `kube_context` | string | none | The kubectl context every Kubernetes call is pinned to. Without one (and without `--context`) anything that needs Kubernetes fails and kubectl is not started; kubectl's own current context is never used. |

### `diff`

These apply to the comparisons the read operations make. They do not apply
to the preview of a write, which shows every change.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `ignore` | list of globs | empty | Paths left out of every comparison, such as `**.updatedAt`. An operation's own `--ignore` adds to this list. |
| `array_match` | map of array path to field | empty | Arrays whose elements are matched by the named field instead of by position, so that an insertion at the head is one added element. |

### `trace.domains.<name>`

One entry per kind of record. It says which table holds the record and how
to follow the record to the S3 object it points at.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `table` | string | none | The table name; `{env}` is replaced with the environment's name. |
| `tables` | map of environment to name | empty | Table names for environments where the pattern does not hold; an entry here wins over `table`. |
| `bucket` | string | none | The bucket name, with `{env}` as in `table`. |
| `buckets` | map of environment to name | empty | Bucket names that win over `bucket`. |
| `hash_attr` | string | `hash` | The table's partition key attribute. |
| `sort_attr` | string | none | The table's sort key attribute; leave it out for a table without one. |
| `sort_format` | string | the value of `version_attr` in braces | How the sort key is built from `--vars`, such as `{locale}#{channel}`. Only used when `sort_attr` is set. |
| `version_attr` | string | `version` | The attribute that holds the record's version. |
| `s3_key_attr` | string | `key` | The attribute that holds the object's key. |
| `s3_version_attr` | string | `versionId` | The attribute that holds the object's version id. |
| `labels` | list of attribute names | empty | Attributes shown in summaries. |
| `namespace` | string | none | The Kubernetes namespace `config.drift` looks in when none is given. |

### A complete file

```yaml config
default_env: dev

environments:
  dev:
    profile: acme-dev
    region: us-west-2
    account_id: "111122223333"
    kube_context: dev-cluster
  staging:
    profile: acme-staging
    region: us-west-2
    account_id: "444455556666"
    kube_context: staging-cluster

diff:
  ignore:
    - "**.updatedAt"
    - "**.etag"
  array_match:
    "$.rules[*]": id
    "$.items[*]": id

trace:
  domains:
    menu:
      table: "ss-menu-{env}"
      tables:
        staging: legacy-menus-stg
      bucket: "ss-config-{env}"
      buckets:
        staging: legacy-config-stg
      hash_attr: storeNumber
      sort_attr: localeChannel
      sort_format: "{locale}#{channel}"
      version_attr: version
      s3_key_attr: key
      s3_version_attr: versionId
      labels: [storeNumber, version]
      namespace: menu
```

## `presets.yaml`

A preset is a named set of parameters for one operation. `ss-support run
<operation> --save-preset NAME` saves the parameters given on that command
line after a successful run, `--preset NAME` loads them again (a flag given
beside it wins), and `ctrl+s` saves from the TUI. `ss-support presets list`,
`show`, and `delete` manage them.

The file can also be edited by hand; it is read on every use. Under the one
top-level key `presets`, each operation id holds its presets by name, and
each preset holds parameter names and values. Every value is a string, so
quote numbers and anything YAML would read as something else.

```yaml presets
presets:
  s3.version-diff:
    acme-tenant:
      bucket: cfg-nonprod
      key: tenants/acme.json
      a: "3HL4kqtJlcpXroDTDmJ.rioz_Rb"
  ddb.item-history:
    acme:
      table: tenants
      pk: acme
      limit: "20"
```

A parameter saved with an empty value is dropped, since it would carry
nothing and would hide the operation's default.

## The audit log

Every run of an operation that is not a read appends one line of JSON to the
audit log when it ends: who ran what, in which environment, with which
parameters, how long it took, and how it ended (`ok`, `declined`, `denied`,
`dry-run`, or `error`). Reads are not logged. The directories are created
private to the user and the file is readable by the user only. Parameters
are recorded as given, including the values passed to `ddb.item-edit --set`.

## The cache

Fetched object versions are kept under the cache directory, because a
version id always names the same content. The current version of an object
is always fetched again. Deleting the directory is safe; `--no-cache` skips
it for one run.
