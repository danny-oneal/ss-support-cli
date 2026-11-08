//! Typed paths from the document root to a changed value.
//!
//! Stored as structured segments rather than a pre-rendered string so that
//! ignore rules match structurally, the TUI can collapse by subtree, and the
//! exporters render dotted notation or JSONPath from the same value.

use std::fmt;

use serde::Serialize;
use serde_json::Value;

use crate::jsonutil;

/// One step in a [`Path`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Segment {
    /// An object member accessed by key.
    Key(String),
    /// An array element accessed by position.
    Index(usize),
    /// An array element matched by an identity field rather than position.
    MatchKey {
        /// The identity field named by the array-match rule.
        field: String,
        /// That field's value on the matched element.
        value: Value,
    },
}

impl Segment {
    /// The dotted-notation text of this segment alone (no leading dot).
    fn write_dotted(&self, out: &mut String) {
        match self {
            Segment::Key(k) => out.push_str(k),
            Segment::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
            Segment::MatchKey { field, value } => {
                out.push('[');
                out.push_str(field);
                out.push('=');
                out.push_str(&plain(value));
                out.push(']');
            }
        }
    }
}

/// An identity value as it appears inside a path segment: strings bare,
/// everything else as compact JSON.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => jsonutil::compact(other),
    }
}

/// A typed path from the document root to a changed value.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Path(pub Vec<Segment>);

impl Path {
    /// The segments, root first.
    pub fn segments(&self) -> &[Segment] {
        &self.0
    }

    /// A new path with `s` appended; the parent is left untouched.
    #[must_use]
    pub fn child(&self, s: Segment) -> Path {
        let mut out = Vec::with_capacity(self.0.len() + 1);
        out.extend(self.0.iter().cloned());
        out.push(s);
        Path(out)
    }

    /// Dotted notation: `a.b[0].c`, `rules[id=r1].limit`; the root is empty.
    pub fn dotted(&self) -> String {
        let mut out = String::new();
        for s in &self.0 {
            if matches!(s, Segment::Key(_)) && !out.is_empty() {
                out.push('.');
            }
            s.write_dotted(&mut out);
        }
        out
    }

    /// JSONPath: `$.a.b[0].c`; match-key segments become filter expressions
    /// (`$.rules[?(@.id=="r1")]`).
    pub fn jsonpath(&self) -> String {
        let mut out = String::from("$");
        for s in &self.0 {
            match s {
                Segment::Key(k) => {
                    out.push('.');
                    out.push_str(k);
                }
                Segment::Index(i) => {
                    out.push('[');
                    out.push_str(&i.to_string());
                    out.push(']');
                }
                Segment::MatchKey { field, value } => {
                    out.push_str("[?(@.");
                    out.push_str(field);
                    out.push_str("==");
                    out.push_str(&serde_json::to_string(&plain(value)).unwrap_or_default());
                    out.push_str(")]");
                }
            }
        }
        out
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dotted())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(segs: Vec<Segment>) -> Path {
        Path(segs)
    }

    #[test]
    fn dotted_joins_keys_with_dots_and_appends_indexes() {
        let path = p(vec![
            Segment::Key("a".into()),
            Segment::Key("b".into()),
            Segment::Index(0),
            Segment::Key("c".into()),
        ]);
        assert_eq!(path.dotted(), "a.b[0].c");
        assert_eq!(path.to_string(), "a.b[0].c");
    }

    #[test]
    fn dotted_renders_match_keys_unquoted() {
        let path = p(vec![
            Segment::Key("rules".into()),
            Segment::MatchKey {
                field: "id".into(),
                value: json!("r1"),
            },
            Segment::Key("limit".into()),
        ]);
        assert_eq!(path.dotted(), "rules[id=r1].limit");
        let numeric = p(vec![Segment::MatchKey {
            field: "id".into(),
            value: json!(5),
        }]);
        assert_eq!(numeric.dotted(), "[id=5]");
    }

    #[test]
    fn jsonpath_uses_dollar_root_and_filter_expressions() {
        let path = p(vec![
            Segment::Key("a".into()),
            Segment::Key("b".into()),
            Segment::Index(0),
            Segment::Key("c".into()),
        ]);
        assert_eq!(path.jsonpath(), "$.a.b[0].c");
        let keyed = p(vec![
            Segment::Key("rules".into()),
            Segment::MatchKey {
                field: "id".into(),
                value: json!("r1"),
            },
        ]);
        assert_eq!(keyed.jsonpath(), r#"$.rules[?(@.id=="r1")]"#);
        let numeric = p(vec![Segment::MatchKey {
            field: "id".into(),
            value: json!(5),
        }]);
        assert_eq!(numeric.jsonpath(), r#"$[?(@.id=="5")]"#);
    }

    #[test]
    fn root_path_renders_empty_dotted_and_bare_dollar() {
        let root = Path::default();
        assert_eq!(root.dotted(), "");
        assert_eq!(root.jsonpath(), "$");
    }

    #[test]
    fn child_appends_without_aliasing_the_parent() {
        let parent = p(vec![Segment::Key("a".into())]);
        let child = parent.child(Segment::Index(2));
        assert_eq!(child.dotted(), "a[2]");
        assert_eq!(parent.dotted(), "a");
        assert_eq!(child.segments().len(), 2);
    }
}
