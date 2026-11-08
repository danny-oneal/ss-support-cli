//! The structural walk: objects by key-set union, arrays by identity key or
//! LCS, scalars by typed compare.

use serde::Serialize;
use serde_json::Value;

use std::collections::BTreeMap;

use crate::diff::glob::{PathGlob, compile_globs};
use crate::diff::lcs;
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
    let array_rules = opts
        .array_match
        .iter()
        .filter_map(|(pattern, field)| {
            PathGlob::compile(pattern).ok().map(|g| ArrayRule {
                glob: g,
                field: field.clone(),
            })
        })
        .collect();
    let mut w = Walker {
        opts,
        ignore,
        array_rules,
        changes: Vec::new(),
    };
    w.walk(&Path::default(), left, right);
    // Go sorts by the dotted path string (stable), so output order is
    // independent of map iteration and matching strategy.
    let mut changes = w.changes;
    changes.sort_by_cached_key(|c| c.path.dotted());
    let mut delta = Delta::default();
    for c in changes {
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

/// One `array_match` rule: elements of arrays at `glob` are identified by `field`.
struct ArrayRule {
    glob: PathGlob,
    field: String,
}

struct Walker<'a> {
    opts: &'a Opts,
    ignore: Vec<PathGlob>,
    array_rules: Vec<ArrayRule>,
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

    fn array(&mut self, p: &Path, a: &[Value], b: &[Value]) {
        match self.array_key_for(p) {
            Some(field) => self.keyed_array(p, a, b, &field),
            None => self.lcs_array(p, a, b),
        }
    }

    /// Whether an `array_match` rule addresses elements of the array at `p`.
    /// Rules are written against elements (`$.rules[*]`), so probe with a
    /// positional child.
    fn array_key_for(&self, p: &Path) -> Option<String> {
        let probe = p.child(Segment::Index(0));
        self.array_rules
            .iter()
            .find(|r| r.glob.matches(&probe))
            .map(|r| r.field.clone())
    }

    /// Match elements by identity field so insertion and reordering are not
    /// reported as churn. Elements missing the field (or carrying a duplicate
    /// identity) fall back to positional semantics.
    fn keyed_array(&mut self, p: &Path, a: &[Value], b: &[Value], field: &str) {
        let (am, a_order, a_unkeyed) = index_by_key(a, field);
        let (bm, b_order, b_unkeyed) = index_by_key(b, field);

        for k in &a_order {
            let (av, _) = am[k];
            let cp = p.child(Segment::MatchKey {
                field: field.to_string(),
                value: id_value(av, field),
            });
            match bm.get(k) {
                Some((bv, _)) => self.walk(&cp, av, bv),
                None => self.add(cp, Kind::Removed, Some(av.clone()), None),
            }
        }
        for k in &b_order {
            if !am.contains_key(k) {
                let (bv, _) = bm[k];
                let cp = p.child(Segment::MatchKey {
                    field: field.to_string(),
                    value: id_value(bv, field),
                });
                self.add(cp, Kind::Added, None, Some(bv.clone()));
            }
        }
        // Unkeyed leftovers keep positional semantics.
        let n = a_unkeyed.len().min(b_unkeyed.len());
        for ((av, ai), (bv, _)) in a_unkeyed.iter().zip(&b_unkeyed) {
            self.walk(&p.child(Segment::Index(*ai)), av, bv);
        }
        for (av, ai) in &a_unkeyed[n..] {
            self.add(
                p.child(Segment::Index(*ai)),
                Kind::Removed,
                Some((*av).clone()),
                None,
            );
        }
        for (bv, bi) in &b_unkeyed[n..] {
            self.add(
                p.child(Segment::Index(*bi)),
                Kind::Added,
                None,
                Some((*bv).clone()),
            );
        }
    }

    /// Align by LCS over canonical element strings so pure insertions and
    /// deletions stay clean. Between matches, remaining elements pair
    /// positionally and recurse, so a single edited element shows nested
    /// changes rather than remove+add.
    fn lcs_array(&mut self, p: &Path, a: &[Value], b: &[Value]) {
        let ha: Vec<String> = a.iter().map(jsonutil::canonical).collect();
        let hb: Vec<String> = b.iter().map(jsonutil::canonical).collect();
        let matches = lcs::pairs(&ha, &hb);
        let (mut i, mut j) = (0, 0);
        for (ai, bj) in matches
            .into_iter()
            .chain(std::iter::once((a.len(), b.len())))
        {
            let (ga, gb) = (&a[i..ai], &b[j..bj]);
            let n = ga.len().min(gb.len());
            for (k, (av, bv)) in ga.iter().zip(gb).enumerate() {
                self.walk(&p.child(Segment::Index(i + k)), av, bv);
            }
            for (k, av) in ga.iter().enumerate().skip(n) {
                self.add(
                    p.child(Segment::Index(i + k)),
                    Kind::Removed,
                    Some(av.clone()),
                    None,
                );
            }
            for (k, bv) in gb.iter().enumerate().skip(n) {
                self.add(
                    p.child(Segment::Index(j + k)),
                    Kind::Added,
                    None,
                    Some(bv.clone()),
                );
            }
            i = ai + 1;
            j = bj + 1;
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

/// Splits an array into keyed elements (canonical identity → (value, position),
/// plus first-seen order) and unkeyed leftovers (non-objects, missing field,
/// duplicate identities) with their positions.
#[allow(clippy::type_complexity)]
fn index_by_key<'v>(
    items: &'v [Value],
    field: &str,
) -> (
    BTreeMap<String, (&'v Value, usize)>,
    Vec<String>,
    Vec<(&'v Value, usize)>,
) {
    let mut map = BTreeMap::new();
    let mut order = Vec::new();
    let mut unkeyed = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let Some(idv) = it.as_object().and_then(|o| o.get(field)) else {
            unkeyed.push((it, i));
            continue;
        };
        let k = jsonutil::canonical(idv);
        if map.contains_key(&k) {
            unkeyed.push((it, i)); // duplicate IDs: keep first, rest unkeyed
            continue;
        }
        map.insert(k.clone(), (it, i));
        order.push(k);
    }
    (map, order, unkeyed)
}

/// The raw identity value of a keyed element (callers only pass objects that
/// have the field).
fn id_value(elem: &Value, field: &str) -> Value {
    elem.get(field).cloned().unwrap_or(Value::Null)
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

    #[test]
    fn array_deletion_is_a_single_removed() {
        let delta = json(
            &json!(["a", "b", "c"]),
            &json!(["a", "c"]),
            &Opts::default(),
        );
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].kind, Kind::Removed);
        assert_eq!(delta.changes[0].path.dotted(), "[1]");
    }

    #[test]
    fn edited_element_between_matches_diffs_nested_not_remove_add() {
        let a = json!({"items": [{"n": 1}, {"n": 2}, {"n": 9}]});
        let b = json!({"items": [{"n": 1}, {"n": 3}, {"n": 9}]});
        let delta = json(&a, &b, &Opts::default());
        assert_eq!(delta.changes.len(), 1, "{:?}", delta.changes);
        assert_eq!(delta.changes[0].path.dotted(), "items[1].n");
        assert_eq!(delta.changes[0].kind, Kind::Changed);
    }

    #[test]
    fn lcs_matches_integral_floats_against_integers() {
        let delta = json(&json!([1, 2, 3]), &json!([1.0, 2.0, 3.0]), &Opts::default());
        assert!(delta.changes.is_empty(), "{:?}", delta.changes);
    }

    #[test]
    fn keyed_arrays_match_by_identity_field() {
        let a = json!({"rules": [{"id": "r1", "limit": 10}, {"id": "r2", "limit": 20}]});
        let b = json!({"rules": [{"id": "r0", "limit": 5}, {"id": "r1", "limit": 10}, {"id": "r2", "limit": 25}]});
        let opts = Opts {
            array_match: [("$.rules[*]".to_string(), "id".to_string())].into(),
            ..Opts::default()
        };
        let delta = json(&a, &b, &opts);
        let paths: Vec<String> = delta.changes.iter().map(|c| c.path.dotted()).collect();
        assert_eq!(paths, vec!["rules[id=r0]", "rules[id=r2].limit"]);
        assert_eq!(delta.changes[0].kind, Kind::Added);
        assert_eq!(delta.changes[1].kind, Kind::Changed);
        assert_eq!(
            delta.changes[0].path.jsonpath(),
            r#"$.rules[?(@.id=="r0")]"#
        );
    }

    #[test]
    fn keyed_arrays_report_removed_elements_by_key() {
        let a = json!({"rules": [{"id": "r1"}, {"id": "r2"}]});
        let b = json!({"rules": [{"id": "r2"}]});
        let opts = Opts {
            array_match: [("$.rules[*]".to_string(), "id".to_string())].into(),
            ..Opts::default()
        };
        let delta = json(&a, &b, &opts);
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].kind, Kind::Removed);
        assert_eq!(delta.changes[0].path.dotted(), "rules[id=r1]");
    }

    #[test]
    fn keyed_arrays_fall_back_to_positions_for_unkeyed_and_duplicate_elements() {
        // Element without the field, a scalar, and a duplicate id all keep
        // positional semantics; duplicates keep the first occurrence keyed.
        let a = json!({"rules": [{"id": "r1", "v": 1}, {"name": "x"}, "s", {"id": "r1", "v": 2}]});
        let b = json!({"rules": [{"id": "r1", "v": 1}, {"name": "y"}, "s", {"id": "r1", "v": 3}]});
        let opts = Opts {
            array_match: [("$.rules[*]".to_string(), "id".to_string())].into(),
            ..Opts::default()
        };
        let delta = json(&a, &b, &opts);
        let paths: Vec<String> = delta.changes.iter().map(|c| c.path.dotted()).collect();
        assert_eq!(paths, vec!["rules[1].name", "rules[3].v"]);
    }

    #[test]
    fn keyed_identity_values_match_numerically() {
        let a = json!({"rules": [{"id": 1, "v": "a"}]});
        let b = json!({"rules": [{"id": 1.0, "v": "b"}]});
        let opts = Opts {
            array_match: [("$.rules[*]".to_string(), "id".to_string())].into(),
            ..Opts::default()
        };
        let delta = json(&a, &b, &opts);
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].path.dotted(), "rules[id=1].v");
    }

    #[test]
    fn array_match_rule_applies_only_to_the_addressed_array() {
        let a = json!({"rules": [{"id": "r1"}], "other": [{"id": "o1"}]});
        let b = json!({"rules": [{"id": "r1"}], "other": [{"id": "o2"}]});
        let opts = Opts {
            array_match: [("$.rules[*]".to_string(), "id".to_string())].into(),
            ..Opts::default()
        };
        let delta = json(&a, &b, &opts);
        // `other` is unkeyed: the single element pairs positionally and diffs nested.
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].path.dotted(), "other[0].id");
    }
}
