# Changelog

All notable changes are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
