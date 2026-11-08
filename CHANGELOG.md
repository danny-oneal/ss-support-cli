# Changelog

All notable changes to the Rust port of `ss` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Cargo workspace (`ss-core` library, `ss` binary) with the exit-code scheme
  `0` ok · `1` operation failed · `2` usage/config error · `130` interrupted.
- Partial semantic JSON diff engine carried over from the scaffold.
