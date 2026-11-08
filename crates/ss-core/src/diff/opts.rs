//! Diff options: what the engine ignores and how it aligns arrays.

use std::collections::BTreeMap;

/// Tuning knobs for a diff run. Two rules carry almost all the weight:
/// ignore globs (timestamps, revision counters, checksums are pure noise) and
/// array matching by identity key (so one insertion at position 0 is a single
/// `Added`, not N `Changed`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Opts {
    /// Path globs to ignore, e.g. `**.updatedAt`, `$.metadata.revision`.
    /// Invalid patterns simply never match.
    pub ignore: Vec<String>,
    /// Array-element glob → identity field, e.g. `{"$.rules[*]": "id"}`.
    pub array_match: BTreeMap<String, String>,
    /// Float comparison epsilon; `0.0` means exact.
    pub tolerance: f64,
    /// Treat an explicit JSON `null` as if the key were absent, so
    /// `{"a": null}` equals `{}`.
    pub treat_null_as_absent: bool,
}
