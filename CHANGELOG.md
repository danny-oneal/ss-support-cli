# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0-alpha.7] - 2026-11-13

### Added
- `s3.restore-version`: makes an earlier version of an object its current
  one again by copying it onto its key, so the version that was current
  stays in the history. Confirmed with `y`. It refuses a delete marker and
  an object whose bucket is not keeping versions.
- `ddb.item-edit`: sets (`--set`, a JSON object) and removes (`--remove`)
  attributes of one item and leaves the others alone. Confirmed by typing
  the table name; `--yes` does not cover it.
- Both show the diff from what is stored to what will be stored before they
  ask, and neither writes when what it was shown has changed since.
- `R` on a version in the TUI's S3 browser opens the restore form filled in.

### Changed
- `--dry-run` shows what the operation was about to change, after the note
  that it stopped.
- In the TUI, a warning a run gave (an audit entry that could not be
  written) stays above the result, and with a failure, instead of
  disappearing from the status line at the next key.

## [0.1.0-alpha.6] - 2026-11-13

### Added
- A pod list in the TUI (`p`): the pods of the context's default namespace,
  or of another one (`n`), with ready containers, status as `kubectl get
  pods` words it, restarts, and age. A named namespace that lists no pods is
  looked up first, so a mistyped namespace is reported instead of shown as
  empty.
- A log view in the TUI (`enter` on a pod): the pod's log from its last 200
  lines, followed as it grows, with scrolling, a line filter, and sideways
  scrolling for long lines. The newest 5,000 lines are kept, up to 8 MiB; a
  line longer than 16 KiB is shown as several; colour codes are removed.
  Leaving the view stops kubectl.

### Changed
- Every kubectl process still running is stopped before `ss-support` exits,
  however it exits: a command that finished, a signal, a second signal, or a
  panic in the TUI.
- Leaving a TUI screen stops what it was still waiting for, so a kubectl
  that has not answered does not outlive the screen that asked.
- A screen that is only receiving log lines is redrawn at most ten times a
  second; everything else is drawn at once.

## [0.1.0-alpha.5] - 2026-11-13

### Added
- An interactive TUI: `ss-support` with no subcommand, on a terminal, opens a
  list of every operation with a fuzzy filter. Choosing one shows a form of
  its params (with suggestions, saved presets, and defaults shown but not
  filled in), runs it with a live step list, and shows the result one view at
  a time. Runs go through the same environment allowlist and audit log as
  `ss-support run`.
- Confirmation in the TUI is always a typed answer ended with enter: `y` for
  a mutating operation, the resource name for a destructive one. `--yes` has
  no effect there. The preview of what will change scrolls when it is longer
  than the screen. Keys typed or pasted before a confirmation could be read
  do not answer it.
- A diff view in the TUI: step through changes, switch between unified and
  side-by-side, collapse to one line per change, and filter by path.
- An S3 browser in the TUI (`b`): buckets, prefixes, objects, and an object's
  versions; mark two versions and `d` opens `s3.version-diff` for them.
- `e` on a result exports the markdown report to the working directory;
  `ctrl+s` on a form or a result saves the run as a preset.

### Changed
- A bare `ss-support` prints the help only when stdin or stdout is not a
  terminal; on a terminal it opens the TUI.

### Fixed
- Text tables and summaries line up when a cell or key holds wide characters
  (CJK, for example): columns are padded by display width, not by character
  count. This applies to `ss-support run` output as well.

## [0.1.0-alpha.4] - 2026-11-10

### Added
- Read-only cross-service operations `config.drift` (an S3 config against a
  live Kubernetes ConfigMap) and `record.trace` (a DynamoDB record followed to
  its S3 config, its previous version, and optionally a live ConfigMap).
- Kubernetes access through kubectl, with every call pinned to the
  environment's `kube_context` (or `--context`); without one, any step that
  needs Kubernetes fails (exit 1) and kubectl is not started. Only
  `kubectl get` and `kubectl logs` are ever executed. kubectl errors are
  reported as one line, and resource, ConfigMap, and pod names that are empty
  or start with `-` are refused.

## [0.1.0-alpha.3] - 2026-11-10

### Added
- Read-only DynamoDB operations `ddb.item-compare`, `ddb.item-history`, and
  `ddb.gsi-consistency`, with table, index, and version suggestions.
- DynamoDB access through the selected environment's profile and region, with
  the same account guardrail and expired-session message as S3.
- `SS_SUPPORT_DDB_ENDPOINT` targets a DynamoDB-compatible endpoint such as
  dynalite (no identity check).
- `make contract-ddb` runs the DynamoDB contract suite against dynalite.

## [0.1.0-alpha.2] - 2026-11-10

### Changed
- The binary and package are renamed from `ss` to `ss-support`, so the tool no
  longer collides with the system `ss` socket-statistics command.
- Environment variables move from `SS_*` to `SS_SUPPORT_*` (`SS_SUPPORT_ENV`,
  `SS_SUPPORT_PROFILE`, `SS_SUPPORT_REGION`, `SS_SUPPORT_CONFIG`,
  `SS_SUPPORT_PRESETS`, `SS_SUPPORT_S3_ENDPOINT`, and the `*_TEST_*` ones).
- Config, presets, cache, and audit log directories move from `…/ss/` to
  `…/ss-support/` (`~/.config/ss-support/config.yaml`, `~/.cache/ss-support/`,
  `~/.local/state/ss-support/audit.log`). Existing files must be moved by hand.

## [0.1.0-alpha.1] - 2026-11-09

### Added
- Cargo workspace (`ss-core` library, `ss` binary) with the exit-code scheme
  `0` ok · `1` operation failed · `2` usage/config error · `130` interrupted.
- Semantic JSON diff engine: typed paths, structural ignore globs, keyed and
  LCS array alignment, float tolerance, null-as-absent, line-diff fallback.
- Operations layer: a runner with an environment guard, a confirmation prompt
  before the first change (typed resource name for destructive operations),
  `--dry-run`, step progress, Ctrl-C cancellation, and a JSONL audit log.
- Config file, saved parameter presets, and a local object-version cache.
- Commands `ss ops list|describe`, `ss run <op>` (with `--preset`,
  `--save-preset`, `--export`, `-o json`), and `ss presets list|show|delete`.
- Demo operations `demo.hello` and `demo.json-diff`.
- Standalone repository layout: `Makefile` (`make check`, `make msrv`), CI
  workflow, vendored diff golden fixtures, and `docs/`.
- Read-only S3 operations `s3.version-diff`, `s3.object-inspect`, and
  `s3.prefix-audit`, with bucket → key → version suggestions.
- AWS access through the named profile and region of the selected
  environment; an environment's `account_id` is checked against the caller
  identity before the first S3 call; an expired SSO session names the
  `aws sso login` command.
- `SS_S3_ENDPOINT` targets an S3-compatible endpoint such as MinIO
  (path-style, no identity check).
- Fetched object versions are cached locally (`--no-cache` bypasses); bodies
  over 32 MiB are refused.
- `make contract` runs the S3 contract suite against MinIO.

### Fixed
- A bad operation param value (e.g. `--tolerance abc`) exits 2, not 1.
- An unreadable config file exits 1; only an unparsable one exits 2.
- SIGTERM and SIGHUP cancel the run like Ctrl-C and exit 130; a second
  signal of any kind exits at once.
- A run cancelled while an operation is failing on its own is reported as
  interrupted (exit 130), not as a failure.
- A read-only operation can never run a mutating step.
- Op flags accept values starting with a hyphen (`--repeat -1`), and a
  repeated flag takes its last value instead of failing.
- `--save-preset` with nothing to save exits 2 before the operation runs.
- The confirmation prompt line is ended when input ends or the run is
  cancelled, so the error starts on its own line.
- A failed audit-log write prints `warning: audit log: …` instead of being
  silently dropped; the exit code is unchanged.
- `s3.object-inspect` on a deleted object shows the delete marker and the
  full version history instead of failing.
- Missing AWS credentials and a never-started or rejected SSO session name
  the `aws sso login --profile` command instead of a raw SDK error, also with
  `SS_S3_ENDPOINT` set; with no profile in the environment the hint names
  `AWS_PROFILE`'s.
