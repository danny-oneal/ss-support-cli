//! Structural path globs for ignore rules and array-match rules.
//!
//! Globs match [`Path`]s segment by segment rather than by string comparison,
//! so rules are composable predicates. Syntax:
//!
//! ```text
//! foo.bar     exact keys
//! *           exactly one segment (key or index)
//! **          any number of segments, including none
//! foo[*]      key foo, then any array position (indexed or keyed)
//! foo[2]      key foo, then index 2
//! $.foo       a leading "$." (or bare "$") is accepted and ignored
//! ```

use crate::diff::path::{Path, Segment};

/// A compile error for one glob pattern.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GlobError {
    /// The pattern had no segments after stripping `$` and `.`.
    #[error("empty path glob {0:?}")]
    Empty(String),
    /// Two dots in a row, or a leading/trailing dot.
    #[error("empty segment in glob {0:?}")]
    EmptySegment(String),
    /// A `[` with no closing `]`.
    #[error("unterminated '[' in glob {0:?}")]
    Unterminated(String),
    /// Bracket contents that are neither `*` nor an index.
    #[error("bad index {index:?} in glob {pattern:?}")]
    BadIndex {
        /// The offending bracket contents.
        index: String,
        /// The whole pattern.
        pattern: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GlobSeg {
    Key(String),
    Star,
    DoubleStar,
    AnyIndex,
    Index(usize),
}

/// A compiled path glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathGlob {
    raw: String,
    segs: Vec<GlobSeg>,
}

impl PathGlob {
    /// Parses a pattern. Invalid bracket forms fail loudly rather than
    /// silently never matching.
    ///
    /// # Errors
    /// See [`GlobError`].
    pub fn compile(pattern: &str) -> Result<Self, GlobError> {
        let raw = pattern.to_string();
        let mut p = pattern.trim();
        p = p.strip_prefix('$').unwrap_or(p);
        p = p.strip_prefix('.').unwrap_or(p);
        if p.is_empty() {
            return Err(GlobError::Empty(raw));
        }
        let mut segs = Vec::new();
        for tok in p.split('.') {
            if tok.is_empty() {
                return Err(GlobError::EmptySegment(raw));
            }
            let mut rest = tok;
            while !rest.is_empty() {
                match rest.find('[') {
                    None => {
                        segs.push(key_or_star(rest));
                        rest = "";
                    }
                    Some(0) => {
                        let end = rest
                            .find(']')
                            .ok_or_else(|| GlobError::Unterminated(raw.clone()))?;
                        let inner = &rest[1..end];
                        if inner == "*" {
                            segs.push(GlobSeg::AnyIndex);
                        } else {
                            let n = inner.parse::<usize>().map_err(|_| GlobError::BadIndex {
                                index: inner.to_string(),
                                pattern: raw.clone(),
                            })?;
                            segs.push(GlobSeg::Index(n));
                        }
                        rest = &rest[end + 1..];
                    }
                    Some(br) => {
                        segs.push(key_or_star(&rest[..br]));
                        rest = &rest[br..];
                    }
                }
            }
        }
        Ok(PathGlob { raw, segs })
    }

    /// The pattern as written.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Whether the glob matches the full path.
    pub fn matches(&self, p: &Path) -> bool {
        match_segs(&self.segs, p.segments())
    }
}

fn key_or_star(tok: &str) -> GlobSeg {
    match tok {
        "**" => GlobSeg::DoubleStar,
        "*" => GlobSeg::Star,
        other => GlobSeg::Key(other.to_string()),
    }
}

fn match_segs(gs: &[GlobSeg], p: &[Segment]) -> bool {
    let Some((head, tail)) = gs.split_first() else {
        return p.is_empty();
    };
    if *head == GlobSeg::DoubleStar {
        // ** matches zero or more segments.
        return (0..=p.len()).any(|i| match_segs(tail, &p[i..]));
    }
    let Some((first, rest)) = p.split_first() else {
        return false;
    };
    match_one(head, first) && match_segs(tail, rest)
}

fn match_one(g: &GlobSeg, s: &Segment) -> bool {
    match g {
        GlobSeg::Star => true,
        GlobSeg::Key(k) => matches!(s, Segment::Key(sk) if sk == k),
        // Keyed elements sit where an index would.
        GlobSeg::AnyIndex => matches!(s, Segment::Index(_) | Segment::MatchKey { .. }),
        GlobSeg::Index(i) => matches!(s, Segment::Index(si) if si == i),
        GlobSeg::DoubleStar => true,
    }
}

/// Compiles a list, skipping invalid patterns and returning them separately
/// so the caller can report (but not fail on) a bad ignore rule.
pub fn compile_globs(patterns: &[String]) -> (Vec<PathGlob>, Vec<GlobError>) {
    let mut ok = Vec::with_capacity(patterns.len());
    let mut errs = Vec::new();
    for p in patterns {
        match PathGlob::compile(p) {
            Ok(g) => ok.push(g),
            Err(e) => errs.push(e),
        }
    }
    (ok, errs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Path, Segment};
    use serde_json::json;

    fn k(s: &str) -> Segment {
        Segment::Key(s.into())
    }

    #[test]
    fn match_table_mirrors_go() {
        let cases: Vec<(&str, Vec<Segment>, bool)> = vec![
            ("**.updatedAt", vec![k("updatedAt")], true),
            ("**.updatedAt", vec![k("nested"), k("updatedAt")], true),
            (
                "**.updatedAt",
                vec![k("a"), Segment::Index(3), k("updatedAt")],
                true,
            ),
            ("**.updatedAt", vec![k("updatedAtNot")], false),
            (
                "$.metadata.revision",
                vec![k("metadata"), k("revision")],
                true,
            ),
            (
                "$.metadata.revision",
                vec![k("other"), k("revision")],
                false,
            ),
            ("a.*.c", vec![k("a"), k("b"), k("c")], true),
            ("a.*.c", vec![k("a"), Segment::Index(0), k("c")], true),
            ("a.*.c", vec![k("a"), k("b"), k("x"), k("c")], false),
            ("rules[*]", vec![k("rules"), Segment::Index(2)], true),
            (
                "rules[*]",
                vec![
                    k("rules"),
                    Segment::MatchKey {
                        field: "id".into(),
                        value: json!("r1"),
                    },
                ],
                true,
            ),
            ("rules[1]", vec![k("rules"), Segment::Index(1)], true),
            ("rules[1]", vec![k("rules"), Segment::Index(2)], false),
            (
                "$.rules[*].limit",
                vec![k("rules"), Segment::Index(0), k("limit")],
                true,
            ),
            ("**", vec![k("anything"), Segment::Index(1)], true),
            ("**.etag", vec![], false),
        ];
        for (glob, segs, want) in cases {
            let g = PathGlob::compile(glob).unwrap_or_else(|e| panic!("compile {glob:?}: {e}"));
            let path = Path(segs);
            assert_eq!(
                g.matches(&path),
                want,
                "glob {glob:?} vs path {:?}",
                path.dotted()
            );
        }
    }

    #[test]
    fn compile_rejects_malformed_patterns() {
        for bad in ["", "$", "a..b", "a[", "a[x]"] {
            assert!(
                PathGlob::compile(bad).is_err(),
                "expected error compiling {bad:?}"
            );
        }
    }

    #[test]
    fn raw_keeps_the_original_pattern() {
        assert_eq!(PathGlob::compile("$.rules[*]").unwrap().raw(), "$.rules[*]");
    }

    #[test]
    fn compile_globs_skips_invalid_patterns_and_reports_them() {
        let (ok, errs) = compile_globs(&["**.a".to_string(), "a[".to_string(), "b".to_string()]);
        assert_eq!(ok.len(), 2);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].to_string().contains("a["), "{}", errs[0]);
    }
}
