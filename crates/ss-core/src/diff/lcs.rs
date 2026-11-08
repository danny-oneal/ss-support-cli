//! Longest-common-subsequence alignment over element hashes.

/// Matched index pairs `(ai, bi)` of a classic LCS dynamic program, in
/// increasing order. Ties break toward advancing the left side
/// (`dp[i+1][j] >= dp[i][j+1]`), exactly as the Go implementation does, so
/// the two engines align the same gaps.
pub fn pairs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return Vec::new();
    }
    // dp[i][j] = LCS length of a[i..] and b[j..].
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn pairs_returns_matched_index_pairs_in_order() {
        assert_eq!(
            pairs(&s(&["a", "b", "c"]), &s(&["a", "c"])),
            vec![(0, 0), (2, 1)]
        );
        assert_eq!(
            pairs(&s(&["b", "c"]), &s(&["a", "b", "c"])),
            vec![(0, 1), (1, 2)]
        );
    }

    #[test]
    fn pairs_is_empty_when_either_side_is_empty_or_nothing_matches() {
        assert!(pairs(&s(&[]), &s(&["a"])).is_empty());
        assert!(pairs(&s(&["a"]), &s(&[])).is_empty());
        assert!(pairs(&s(&["a", "b"]), &s(&["c", "d"])).is_empty());
    }

    #[test]
    fn pairs_prefers_advancing_the_left_side_on_ties_like_go() {
        // a=[x,y], b=[y,x]: LCS length 1 either way; Go's dp[i+1][j] >= dp[i][j+1]
        // tie-break skips a[0] first and matches "y".
        assert_eq!(pairs(&s(&["x", "y"]), &s(&["y", "x"])), vec![(1, 0)]);
    }
}
