//! Line-based fallback diff for non-JSON bodies.
//!
//! Used when either side fails to parse as JSON so an operation degrades
//! instead of erroring. Unified-style `-`/`+`/` ` prefixes, no hunk headers.

use crate::diff::lcs;

/// Line diff of `a` against `b`; every output line ends in `\n`.
pub fn text(a: &str, b: &str) -> String {
    let al: Vec<String> = a.split('\n').map(str::to_string).collect();
    let bl: Vec<String> = b.split('\n').map(str::to_string).collect();
    let matches = lcs::pairs(&al, &bl);

    let mut out = String::new();
    let emit = |out: &mut String, prefix: &str, lines: &[String]| {
        for l in lines {
            out.push_str(prefix);
            out.push_str(l);
            out.push('\n');
        }
    };
    let (mut i, mut j) = (0, 0);
    for (ai, bj) in matches {
        emit(&mut out, "-", &al[i..ai]);
        emit(&mut out, "+", &bl[j..bj]);
        emit(&mut out, " ", &al[ai..=ai]);
        i = ai + 1;
        j = bj + 1;
    }
    emit(&mut out, "-", &al[i..]);
    emit(&mut out, "+", &bl[j..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_unified_style_prefixes_without_hunk_headers() {
        // Captured from the Go implementation.
        assert_eq!(text("a\nb\nc", "a\nc\nd"), " a\n-b\n c\n+d\n");
    }

    #[test]
    fn empty_side_becomes_one_removed_blank_line() {
        assert_eq!(text("", "x"), "-\n+x\n");
    }

    #[test]
    fn identical_input_is_all_context() {
        assert_eq!(text("same", "same"), " same\n");
    }
}
