//! The structural walk: objects by key-set union, arrays by identity key or
//! LCS, scalars by typed compare.

use serde::Serialize;
use serde_json::Value;

use crate::diff::glob::{PathGlob, compile_globs};
use crate::diff::opts::Opts;
use crate::diff::path::{Path, Segment};
use crate::jsonutil;

/// How a single value changed between the left and right documents.
///
/// `TypeChanged` is reported separately from `Changed` on purpose: `"5"` → `5`
/// is a schema bug, not a value edit, and support engineers need to see the
/// difference.
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
    pub added: usize,
    /// Count of [`Kind::Removed`] changes.
    pub removed: usize,
    /// Count of [`Kind::Changed`] changes.
    pub changed: usize,
    /// Count of [`Kind::TypeChanged`] changes.
    pub type_changed: usize,
}

/// The result of diffing two documents: changes sorted by path, plus stats.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Delta {
    /// All changes, sorted by dotted path.
    pub changes: Vec<Change>,
    /// Aggregate counts by change kind.
    pub stats: Stats,
}

/// Diff two JSON values, producing a [`Delta`].
pub fn json(left: &Value, right: &Value, opts: &Opts) -> Delta {
    let (ignore, _invalid) = compile_globs(&opts.ignore); // invalid globs simply don't match
    let mut w = Walker {
        opts,
        ignore,
        changes: Vec::new(),
    };
    w.walk(&Path::default(), left, right);
    let mut delta = Delta::default();
    for c in w.changes {
        match c.kind {
            Kind::Added => delta.stats.added += 1,
            Kind::Removed => delta.stats.removed += 1,
            Kind::Changed => delta.stats.changed += 1,
            Kind::TypeChanged => delta.stats.type_changed += 1,
        }
        delta.changes.push(c);
    }
    delta
}

/// `None` for a missing key, and for an explicit null when `null_absent`.
fn present(v: Option<&Value>, null_absent: bool) -> Option<&Value> {
    match v {
        Some(Value::Null) if null_absent => None,
        other => other,
    }
}

struct Walker<'a> {
    opts: &'a Opts,
    ignore: Vec<PathGlob>,
    changes: Vec<Change>,
}

impl Walker<'_> {
    fn ignored(&self, p: &Path) -> bool {
        self.ignore.iter().any(|g| g.matches(p))
    }

    fn add(&mut self, path: Path, kind: Kind, old: Option<Value>, new: Option<Value>) {
        if self.ignored(&path) {
            return;
        }
        self.changes.push(Change {
            path,
            kind,
            old,
            new,
        });
    }

    fn walk(&mut self, p: &Path, a: &Value, b: &Value) {
        if self.ignored(p) {
            return;
        }
        match (a, b) {
            (Value::Object(l), Value::Object(r)) => self.object(p, l, r),
            (Value::Array(l), Value::Array(r)) => self.array(p, l, r),
            _ => self.scalar(p, a, b),
        }
    }

    /// Key-set union: recurse on shared keys, report one-sided keys as
    /// Removed / Added. With `treat_null_as_absent`, an explicit null counts
    /// as a missing key.
    fn object(
        &mut self,
        p: &Path,
        a: &serde_json::Map<String, Value>,
        b: &serde_json::Map<String, Value>,
    ) {
        let null_absent = self.opts.treat_null_as_absent;
        let keys = a.keys().chain(b.keys().filter(|k| !a.contains_key(*k)));
        for k in keys {
            let cp = p.child(Segment::Key(k.clone()));
            match (
                present(a.get(k), null_absent),
                present(b.get(k), null_absent),
            ) {
                (Some(av), None) => self.add(cp, Kind::Removed, Some(av.clone()), None),
                (None, Some(bv)) => self.add(cp, Kind::Added, None, Some(bv.clone())),
                (Some(av), Some(bv)) => self.walk(&cp, av, bv),
                (None, None) => {}
            }
        }
    }

    /// Longest-common-subsequence alignment over element equality so pure
    /// insertions and deletions stay clean. Keyed matching and nested pairing
    /// arrive with the array task.
    fn array(&mut self, p: &Path, a: &[Value], b: &[Value]) {
        let (n, m) = (a.len(), b.len());
        let mut lcs = vec![vec![0usize; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i][j] = if a[i] == b[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if a[i] == b[j] {
                i += 1;
                j += 1;
            } else if lcs[i + 1][j] >= lcs[i][j + 1] {
                self.add(
                    p.child(Segment::Index(i)),
                    Kind::Removed,
                    Some(a[i].clone()),
                    None,
                );
                i += 1;
            } else {
                self.add(
                    p.child(Segment::Index(j)),
                    Kind::Added,
                    None,
                    Some(b[j].clone()),
                );
                j += 1;
            }
        }
        for (k, v) in a.iter().enumerate().skip(i) {
            self.add(
                p.child(Segment::Index(k)),
                Kind::Removed,
                Some(v.clone()),
                None,
            );
        }
        for (k, v) in b.iter().enumerate().skip(j) {
            self.add(
                p.child(Segment::Index(k)),
                Kind::Added,
                None,
                Some(v.clone()),
            );
        }
    }

    /// Typed compare. Null transitions are value edits (a field being set or
    /// cleared), not schema bugs; everything else crossing types is
    /// `TypeChanged`.
    fn scalar(&mut self, p: &Path, a: &Value, b: &Value) {
        let (ta, tb) = (jsonutil::json_type(a), jsonutil::json_type(b));
        if ta == tb {
            if !self.scalar_equal(a, b) {
                self.add(p.clone(), Kind::Changed, Some(a.clone()), Some(b.clone()));
            }
            return;
        }
        let kind = if a.is_null() || b.is_null() {
            Kind::Changed
        } else {
            Kind::TypeChanged
        };
        self.add(p.clone(), kind, Some(a.clone()), Some(b.clone()));
    }

    fn scalar_equal(&self, a: &Value, b: &Value) -> bool {
        match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => {
                if self.opts.tolerance > 0.0 {
                    (x - y).abs() <= self.opts.tolerance
                } else {
                    x == y
                }
            }
            _ => a == b,
        }
    }
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

    #[test]
    fn integer_and_integral_float_are_equal() {
        // Go parses every number as float64; `1` and `1.0` are one value.
        let delta = json(&json!({"a": 1}), &json!({"a": 1.0}), &Opts::default());
        assert!(delta.changes.is_empty(), "{:?}", delta.changes);
    }

    #[test]
    fn null_transitions_are_changed_not_type_changed() {
        let delta = json(
            &json!({"flag": null, "count": 5}),
            &json!({"flag": true, "count": null}),
            &Opts::default(),
        );
        assert_eq!(delta.stats.changed, 2);
        assert_eq!(delta.stats.type_changed, 0);
    }

    #[test]
    fn container_versus_scalar_or_other_container_is_type_changed() {
        let delta = json(
            &json!({"data": {"x": 1}}),
            &json!({"data": [1]}),
            &Opts::default(),
        );
        assert_eq!(delta.stats.type_changed, 1);
        assert_eq!(delta.changes[0].path.dotted(), "data");
    }

    #[test]
    #[allow(clippy::approx_constant)] // values copied from Go's tolerance fixture
    fn tolerance_absorbs_small_float_differences_only() {
        let opts = Opts {
            tolerance: 0.001,
            ..Opts::default()
        };
        let delta = json(
            &json!({"pi": 3.14159, "e": 2.71828}),
            &json!({"pi": 3.14160, "e": 2.8}),
            &opts,
        );
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].path.dotted(), "e");
        assert_eq!(delta.changes[0].kind, Kind::Changed);
    }

    #[test]
    fn treat_null_as_absent_equates_null_and_missing_keys() {
        let opts = Opts {
            treat_null_as_absent: true,
            ..Opts::default()
        };
        let delta = json(
            &json!({"a": null, "b": 1}),
            &json!({"b": 1, "c": null}),
            &opts,
        );
        assert!(delta.changes.is_empty(), "{:?}", delta.changes);
        // Without the option they are a Removed and an Added.
        let plain = json(
            &json!({"a": null, "b": 1}),
            &json!({"b": 1, "c": null}),
            &Opts::default(),
        );
        assert_eq!((plain.stats.removed, plain.stats.added), (1, 1));
    }

    #[test]
    fn ignore_globs_suppress_leaves_and_whole_subtrees() {
        let a = json!({"metadata": {"revision": 1, "name": "x"}, "updatedAt": "t1", "nested": {"updatedAt": "t1", "value": 1}});
        let b = json!({"metadata": {"revision": 2, "name": "x"}, "updatedAt": "t2", "nested": {"updatedAt": "t2", "value": 2}});
        let opts = Opts {
            ignore: vec!["**.updatedAt".into(), "$.metadata.revision".into()],
            ..Opts::default()
        };
        let delta = json(&a, &b, &opts);
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].path.dotted(), "nested.value");

        let subtree = Opts {
            ignore: vec![
                "nested".into(),
                "**.updatedAt".into(),
                "$.metadata.revision".into(),
            ],
            ..Opts::default()
        };
        assert!(json(&a, &b, &subtree).changes.is_empty());
    }

    #[test]
    fn invalid_ignore_globs_are_skipped_not_fatal() {
        let opts = Opts {
            ignore: vec!["a[".into()],
            ..Opts::default()
        };
        let delta = json(&json!({"a": 1}), &json!({"a": 2}), &opts);
        assert_eq!(delta.stats.changed, 1);
    }

    #[test]
    fn identical_documents_produce_no_changes() {
        let doc = json!({"a": 1.0, "b": ["x", "y"], "c": {"d": true}});
        assert!(json(&doc, &doc, &Opts::default()).changes.is_empty());
    }
}
