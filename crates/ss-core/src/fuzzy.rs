//! Subsequence matcher for pickers and suggestion lists: "vdiff" finds
//! "s3.version-diff" without typing it. Scoring rewards matches at word
//! boundaries, consecutive runs, and early positions, and lightly penalises
//! unmatched length so tighter targets rank first.

/// Scores how well `pattern` fits `s`: higher is better, `None` means the
/// pattern's characters do not appear in order in `s`.
///
/// Matching is greedy left to right. Each matched character earns a point,
/// plus bonuses for continuing a consecutive run, starting a word (after
/// `.`, `-`, `_`, `/`, space, `#`, or a lower-to-upper case change), and
/// sitting first in the target. Every four unmatched characters cost a point,
/// and the result never drops below zero.
pub fn score(pattern: &str, s: &str) -> Option<i32> {
    if pattern.is_empty() {
        return Some(0);
    }
    let p: Vec<char> = pattern.chars().map(fold).collect();
    let t: Vec<char> = s.chars().collect();
    let tl: Vec<char> = t.iter().copied().map(fold).collect();

    let mut score: i32 = 0;
    let mut pi = 0;
    let mut last: Option<usize> = None;
    for (i, &c) in tl.iter().enumerate() {
        if pi == p.len() {
            break;
        }
        if c != p[pi] {
            continue;
        }
        score += 1;
        if last.is_some_and(|l| l + 1 == i) {
            score += 3;
        }
        if word_start(&t, i) {
            score += 5;
        }
        if i == 0 {
            score += 3;
        }
        last = Some(i);
        pi += 1;
    }
    if pi < p.len() {
        return None;
    }
    let unmatched = i32::try_from(tl.len() - p.len()).unwrap_or(i32::MAX);
    Some((score - unmatched / 4).max(0))
}

/// Reports whether `pattern`'s characters appear in order in `s`,
/// case-insensitively. An empty pattern matches everything.
pub fn matches(pattern: &str, s: &str) -> bool {
    score(pattern, s).is_some()
}

/// Returns `(index, score)` for every item matching `pattern`, best score
/// first and stable on ties so equal scores keep the caller's order.
pub fn rank(pattern: &str, items: &[&str]) -> Vec<(usize, i32)> {
    let mut out: Vec<(usize, i32)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| score(pattern, it).map(|sc| (i, sc)))
        .collect();
    out.sort_by_key(|&(_, sc)| std::cmp::Reverse(sc));
    out
}

/// Lowercases one character to a single character, so folded text keeps the
/// same length and indices as the original.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn word_start(t: &[char], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = t[i - 1];
    matches!(prev, '.' | '-' | '_' | '/' | ' ' | '#')
        || (prev.is_lowercase() && t[i].is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_subsequences_case_insensitively() {
        let yes = [
            ("", "anything"),
            ("vdiff", "s3.version-diff"),
            ("svd", "s3.version-diff"),
            ("ih", "ddb.item-history"),
            ("VDIFF", "s3.version-diff"),
            ("restore", "Restore object version"),
        ];
        for (p, s) in yes {
            assert!(matches(p, s), "{p:?} should match {s:?}");
        }
        for (p, s) in [("diffv", "s3.version-diff"), ("zzz", "s3.version-diff")] {
            assert!(!matches(p, s), "{p:?} should not match {s:?}");
        }
    }

    #[test]
    fn exact_score_is_pinned() {
        assert_eq!(score("vdiff", "s3.version-diff"), Some(22));
        assert_eq!(score("", "x"), Some(0));
    }

    #[test]
    fn score_ordering() {
        let s = |p, t| score(p, t).expect("match");
        assert!(s("vd", "s3.version-diff") > s("vd", "avoided"));
        assert!(s("hist", "ddb.item-history") > s("hist", "hxixsxtxxxxxxx"));
        assert!(s("hello", "demo.hello") > s("hello", "hxexlxlxo-something-very-long-here"));
    }

    #[test]
    fn rank_orders_and_filters() {
        let items = [
            "ddb.item-history",
            "s3.version-diff",
            "demo.hello",
            "s3.object-inspect",
        ];
        let ranked = rank("s3", &items);
        assert!(ranked.len() >= 2, "ranked = {ranked:?}");
        assert!(matches!(
            items[ranked[0].0],
            "s3.version-diff" | "s3.object-inspect"
        ));
        assert!(rank("zzz", &items).is_empty());
        assert_eq!(rank("", &items), vec![(0, 0), (1, 0), (2, 0), (3, 0)]);
    }

    #[test]
    fn rank_is_stable_on_ties() {
        let ranked = rank("a", &["a", "a", "a"]);
        let order: Vec<usize> = ranked.iter().map(|r| r.0).collect();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn case_folding_never_shifts_indices() {
        assert!(score("i", "İx").is_some());
    }
}
