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
