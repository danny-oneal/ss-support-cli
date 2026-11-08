//! Change counts and their one-line summary.

use std::fmt;

use serde::Serialize;

/// Running counts of changes by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    /// Count of added values.
    pub added: usize,
    /// Count of removed values.
    pub removed: usize,
    /// Count of changed values.
    pub changed: usize,
    /// Count of type-changed values.
    pub type_changed: usize,
}

impl Stats {
    /// Every change, regardless of kind.
    pub fn total(&self) -> usize {
        self.added + self.removed + self.changed + self.type_changed
    }

    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Renders `+2 −1 ~3 !1`; zero counts are omitted, `no changes` when
/// everything is zero.
impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("no changes");
        }
        let parts = [
            ("+", self.added),
            ("−", self.removed),
            ("~", self.changed),
            ("!", self.type_changed),
        ];
        let mut first = true;
        for (glyph, n) in parts {
            if n == 0 {
                continue;
            }
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "{glyph}{n}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_omits_zero_counts() {
        let s = Stats {
            added: 2,
            removed: 1,
            changed: 3,
            type_changed: 1,
        };
        assert_eq!(s.to_string(), "+2 −1 ~3 !1");
        assert_eq!(
            Stats {
                added: 1,
                ..Stats::default()
            }
            .to_string(),
            "+1"
        );
        assert_eq!(
            Stats {
                changed: 1,
                type_changed: 2,
                ..Stats::default()
            }
            .to_string(),
            "~1 !2"
        );
    }

    #[test]
    fn empty_stats_say_no_changes() {
        assert_eq!(Stats::default().to_string(), "no changes");
        assert!(Stats::default().is_empty());
        assert_eq!(
            Stats {
                added: 2,
                removed: 1,
                changed: 3,
                type_changed: 1
            }
            .total(),
            7
        );
    }
}
