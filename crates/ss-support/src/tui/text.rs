//! Making text from outside safe and stable to draw: tabs become spaces,
//! control characters disappear, and lines can be cut by display column.

use std::iter::repeat_n;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The distance between tab stops, in columns.
const TAB_STOP: usize = 8;

/// `text` with tabs expanded to the next stop and every other control
/// character except the line break removed.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut col = 0;
    for grapheme in text.graphemes(true) {
        match grapheme {
            "\n" | "\r\n" => {
                out.push('\n');
                col = 0;
            }
            "\t" => {
                let pad = TAB_STOP - col % TAB_STOP;
                out.extend(repeat_n(' ', pad));
                col += pad;
            }
            other if other.contains(char::is_control) => {}
            other => {
                out.push_str(other);
                col += other.width();
            }
        }
    }
    out
}

/// The part of the single line `line` that starts `skip` columns in and is
/// at most `width` columns wide. A wide character cut by either edge is
/// shown as a space, so columns to its right keep their place.
pub fn slice_columns(line: &str, skip: usize, width: usize) -> String {
    let end = skip.saturating_add(width);
    let mut out = String::new();
    let mut col = 0;
    for grapheme in line.graphemes(true) {
        let start = col;
        col += grapheme.width();
        if col == start || col <= skip {
            continue;
        }
        if start >= end {
            break;
        }
        if start < skip || col > end {
            out.extend(repeat_n(' ', col.min(end) - start.max(skip)));
        } else {
            out.push_str(grapheme);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_expand_to_the_next_stop() {
        assert_eq!(clean("a\tb"), "a       b");
        assert_eq!(clean("12345678\tx"), "12345678        x");
        assert_eq!(clean("\tx"), "        x");
    }

    #[test]
    fn a_tab_after_a_wide_character_counts_its_two_columns() {
        assert_eq!(clean("日\tb"), "日      b");
    }

    #[test]
    fn tab_stops_restart_on_every_line() {
        assert_eq!(clean("abc\n\tx"), "abc\n        x");
    }

    #[test]
    fn control_characters_are_dropped_and_line_breaks_kept() {
        assert_eq!(clean("x\x1b[2Jy\r\nz\x07"), "x[2Jy\nz");
        assert_eq!(clean("a\rb"), "ab");
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(clean("key : value — ✓ 日本"), "key : value — ✓ 日本");
        assert_eq!(clean(""), "");
    }

    #[test]
    fn a_slice_skips_and_limits_by_display_column() {
        assert_eq!(slice_columns("abcdef", 2, 3), "cde");
        assert_eq!(slice_columns("abcdef", 0, 10), "abcdef");
        assert_eq!(slice_columns("abcdef", 6, 3), "");
        assert_eq!(slice_columns("abcdef", 2, 0), "");
    }

    #[test]
    fn a_wide_character_cut_by_an_edge_becomes_a_space() {
        assert_eq!(slice_columns("日本語", 1, 4), " 本 ");
        assert_eq!(slice_columns("日本語", 2, 4), "本語");
        assert_eq!(slice_columns("a日b", 0, 2), "a ");
    }
}
