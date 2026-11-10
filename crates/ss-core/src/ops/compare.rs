//! Comparison of two stored bodies: structurally when both are JSON, line by
//! line otherwise.

use crate::diff::{self, Delta, Opts};
use crate::jsonutil;

/// How two bodies were compared.
#[derive(Debug)]
pub(crate) enum Comparison {
    /// Both bodies parsed; compared structurally.
    Json(Delta),
    /// At least one body did not parse; compared line by line.
    Lines(String),
}

/// Compares `a` with `b`: structurally under `opts` when both are valid JSON,
/// otherwise as a line diff of their lossy UTF-8 text.
pub(crate) fn compare_bodies(a: &[u8], b: &[u8], opts: &Opts) -> Comparison {
    match (jsonutil::parse(a), jsonutil::parse(b)) {
        (Ok(left), Ok(right)) => Comparison::Json(diff::json(&left, &right, opts)),
        _ => Comparison::Lines(diff::text(
            &String::from_utf8_lossy(a),
            &String::from_utf8_lossy(b),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_bodies_compare_structurally() {
        let comparison = compare_bodies(br#"{"a":1}"#, br#"{"a":2}"#, &Opts::default());
        let Comparison::Json(delta) = comparison else {
            panic!("both bodies are JSON");
        };
        let paths: Vec<_> = delta.changes.iter().map(|c| c.path.dotted()).collect();
        assert_eq!(paths, ["a"]);
    }

    #[test]
    fn a_non_json_side_falls_back_to_lines() {
        let comparison = compare_bodies(b"x\ny", br#"{"a":1}"#, &Opts::default());
        let Comparison::Lines(body) = comparison else {
            panic!("one body is not JSON");
        };
        assert!(body.contains("-x"), "{body}");
    }
}
