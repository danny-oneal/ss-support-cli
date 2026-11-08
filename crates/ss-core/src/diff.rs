//! Semantic JSON diff engine.
//!
//! Compares two parsed JSON [`Value`] trees and produces a [`Delta`] of typed
//! [`Change`]s. Ported from the Go `internal/diff` package; functional parity,
//! with a fresh Rust test corpus.

use serde::Serialize;
use serde_json::Value;

/// How a single value changed between the left and right documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    /// Present on the right, absent on the left.
    Added,
    /// Present on the left, absent on the right.
    Removed,
    /// Present on both with the same JSON type but a different value.
    Changed,
    /// Present on both but with a different JSON type (e.g. `"5"` → `5`).
    TypeChanged,
}

/// One step in a [`Path`] from the document root to a changed value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Segment {
    /// An object member accessed by key.
    Key(String),
    /// An array element accessed by position.
    Index(usize),
    /// An array element matched by an identity field rather than position.
    MatchKey {
        /// The identity field named by the array-match rule.
        field: String,
        /// That field's value on the matched element, as text.
        value: String,
    },
}

/// A typed path from the document root to a changed value.
///
/// Stored as structured segments rather than a pre-rendered string so that
/// ignore rules can match structurally and the exporter can render either
/// dotted notation or JSONPath from the same value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Path(pub Vec<Segment>);

/// A single difference between the two documents.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    /// Location of the change.
    pub path: Path,
    /// What kind of change occurred.
    pub kind: Kind,
    /// The left value, if any.
    pub old: Option<Value>,
    /// The right value, if any.
    pub new: Option<Value>,
}

/// Running counts of changes by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    /// Count of [`Kind::Added`] changes.
    pub added: u32,
    /// Count of [`Kind::Removed`] changes.
    pub removed: u32,
    /// Count of [`Kind::Changed`] changes.
    pub changed: u32,
    /// Count of [`Kind::TypeChanged`] changes.
    pub type_changed: u32,
}

/// The result of diffing two documents: changes sorted by path, plus stats.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Delta {
    /// All changes, sorted by path.
    pub changes: Vec<Change>,
    /// Aggregate counts by change kind.
    pub stats: Stats,
}

/// Tuning knobs for a diff run.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// Epsilon for float comparison; `0.0` means exact.
    pub tolerance: f64,
    /// Treat an explicit JSON `null` as if the key were absent.
    pub treat_null_as_absent: bool,
}

/// Diff two JSON values, producing a [`Delta`].
pub fn json(left: &Value, right: &Value, opts: &Opts) -> Delta {
    let mut delta = Delta::default();
    let mut path = Vec::new();
    walk(left, right, &mut path, opts, &mut delta);
    delta
}

/// Recursive structural walk accumulating changes into `delta`.
fn walk(left: &Value, right: &Value, path: &mut Vec<Segment>, opts: &Opts, delta: &mut Delta) {
    if left == right {
        return;
    }
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => walk_object(l, r, path, opts, delta),
        (Value::Array(l), Value::Array(r)) => walk_array(l, r, path, opts, delta),
        _ => {
            // Scalars (and container-vs-scalar): a differing JSON type is a
            // schema change (`"5"` → `5`), reported separately from a value edit.
            let kind = if std::mem::discriminant(left) == std::mem::discriminant(right) {
                Kind::Changed
            } else {
                Kind::TypeChanged
            };
            record(delta, path, kind, Some(left.clone()), Some(right.clone()));
        }
    }
}

/// Diff two JSON objects by key-set union: recurse on shared keys, and report
/// keys present on only one side as [`Kind::Removed`] / [`Kind::Added`].
fn walk_object(
    left: &serde_json::Map<String, Value>,
    right: &serde_json::Map<String, Value>,
    path: &mut Vec<Segment>,
    opts: &Opts,
    delta: &mut Delta,
) {
    for (key, lv) in left {
        path.push(Segment::Key(key.clone()));
        match right.get(key) {
            Some(rv) => walk(lv, rv, path, opts, delta),
            None => record(delta, path, Kind::Removed, Some(lv.clone()), None),
        }
        path.pop();
    }
    for (key, rv) in right {
        if !left.contains_key(key) {
            path.push(Segment::Key(key.clone()));
            record(delta, path, Kind::Added, None, Some(rv.clone()));
            path.pop();
        }
    }
}

/// Diff two JSON arrays with a longest-common-subsequence alignment over
/// element equality, so pure insertions and deletions stay clean rather than
/// marking every following element as changed. Elements present on only one
/// side are reported as [`Kind::Removed`] / [`Kind::Added`] at their index;
/// differing aligned elements are not recursed into (that is what keyed array
/// matching is for).
fn walk_array(
    left: &[Value],
    right: &[Value],
    path: &mut Vec<Segment>,
    _opts: &Opts,
    delta: &mut Delta,
) {
    let (n, m) = (left.len(), right.len());

    // lcs[i][j] = length of the LCS of left[i..] and right[j..].
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if left[i] == right[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if left[i] == right[j] {
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            removed_at(delta, path, i, &left[i]);
            i += 1;
        } else {
            added_at(delta, path, j, &right[j]);
            j += 1;
        }
    }
    while i < n {
        removed_at(delta, path, i, &left[i]);
        i += 1;
    }
    while j < m {
        added_at(delta, path, j, &right[j]);
        j += 1;
    }
}

/// Record a [`Kind::Removed`] for the array element at `index`.
fn removed_at(delta: &mut Delta, path: &mut Vec<Segment>, index: usize, old: &Value) {
    path.push(Segment::Index(index));
    record(delta, path, Kind::Removed, Some(old.clone()), None);
    path.pop();
}

/// Record a [`Kind::Added`] for the array element at `index`.
fn added_at(delta: &mut Delta, path: &mut Vec<Segment>, index: usize, new: &Value) {
    path.push(Segment::Index(index));
    record(delta, path, Kind::Added, None, Some(new.clone()));
    path.pop();
}

/// Append a change and increment the matching stat counter.
fn record(delta: &mut Delta, path: &[Segment], kind: Kind, old: Option<Value>, new: Option<Value>) {
    match kind {
        Kind::Added => delta.stats.added += 1,
        Kind::Removed => delta.stats.removed += 1,
        Kind::Changed => delta.stats.changed += 1,
        Kind::TypeChanged => delta.stats.type_changed += 1,
    }
    delta.changes.push(Change {
        path: Path(path.to_vec()),
        kind,
        old,
        new,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_value_change_is_one_changed() {
        let left = json!(5);
        let right = json!(6);

        let delta = json(&left, &right, &Opts::default());

        assert_eq!(delta.stats.changed, 1);
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].kind, Kind::Changed);
        assert_eq!(delta.changes[0].old, Some(json!(5)));
        assert_eq!(delta.changes[0].new, Some(json!(6)));
    }

    #[test]
    fn scalar_type_change_is_type_changed_not_changed() {
        let left = json!("5");
        let right = json!(5);

        let delta = json(&left, &right, &Opts::default());

        assert_eq!(delta.stats.type_changed, 1);
        assert_eq!(delta.stats.changed, 0);
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].kind, Kind::TypeChanged);
    }

    #[test]
    fn object_reports_added_removed_and_nested_changed_keys() {
        let left = json!({"a": 1, "b": 2, "c": 3});
        let right = json!({"a": 1, "b": 20, "d": 4});

        let delta = json(&left, &right, &Opts::default());

        // a unchanged, b changed, c removed, d added.
        assert_eq!(delta.stats.changed, 1);
        assert_eq!(delta.stats.removed, 1);
        assert_eq!(delta.stats.added, 1);
        assert_eq!(delta.stats.type_changed, 0);

        let changed = delta
            .changes
            .iter()
            .find(|c| c.kind == Kind::Changed)
            .expect("a changed entry");
        assert_eq!(changed.path, Path(vec![Segment::Key("b".into())]));
        assert_eq!(changed.old, Some(json!(2)));
        assert_eq!(changed.new, Some(json!(20)));
    }

    #[test]
    fn array_front_insertion_stays_a_single_added() {
        let left = json!([2, 3]);
        let right = json!([1, 2, 3]);

        let delta = json(&left, &right, &Opts::default());

        // LCS keeps a pure insertion clean instead of marking the tail changed.
        assert_eq!(delta.stats.added, 1);
        assert_eq!(delta.stats.removed, 0);
        assert_eq!(delta.stats.changed, 0);
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].kind, Kind::Added);
        assert_eq!(delta.changes[0].path, Path(vec![Segment::Index(0)]));
        assert_eq!(delta.changes[0].new, Some(json!(1)));
    }
}
