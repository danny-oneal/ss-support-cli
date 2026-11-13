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

/// `text` without terminal escape sequences: colour and cursor codes
/// (`ESC [ … letter`), titles and links (`ESC ] … BEL` or `ESC ] … ESC \`),
/// and two-character escapes. A sequence cut off by the end of the text is
/// removed with everything after it.
pub fn strip_ansi(text: &str) -> String {
    const ESC: char = '\x1b';
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != ESC {
            out.push(c);
            continue;
        }
        match chars.next() {
            // A control sequence runs to its final character.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // An operating-system command runs to BEL or to ESC \.
            Some(']') => {
                let mut after_esc = false;
                for c in chars.by_ref() {
                    if c == '\x07' || (after_esc && c == '\\') {
                        break;
                    }
                    after_esc = c == ESC;
                }
            }
            // Choosing a character set takes one more character.
            Some('(' | ')' | '*' | '+') => {
                chars.next();
            }
            // Every other escape is two characters long.
            Some(_) | None => {}
        }
    }
    out
}

/// The single line `line` broken into rows of at most `width` columns, never
/// splitting a character. A width of zero leaves the line whole.
pub fn wrap_columns(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_owned()];
    }
    let mut rows = vec![String::new()];
    let mut used = 0;
    for grapheme in line.graphemes(true) {
        let cols = grapheme.width();
        if used > 0 && used + cols > width {
            rows.push(String::new());
            used = 0;
        }
        if let Some(row) = rows.last_mut() {
            row.push_str(grapheme);
        }
        used += cols;
    }
    rows
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
    fn colour_and_cursor_sequences_are_removed_and_the_text_kept() {
        assert_eq!(strip_ansi("\x1b[31mERROR\x1b[0m done"), "ERROR done");
        assert_eq!(strip_ansi("a\x1b[2K\x1b[1;1Hb"), "ab");
        assert_eq!(strip_ansi("\x1b[38;5;196mred\x1b[m"), "red");
    }

    #[test]
    fn titles_and_links_are_removed_and_their_text_kept() {
        assert_eq!(strip_ansi("\x1b]0;window title\x07text"), "text");
        assert_eq!(
            strip_ansi("\x1b]8;;http://example.test\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
    }

    #[test]
    fn two_character_escapes_are_removed() {
        assert_eq!(strip_ansi("a\x1bcb\x1b(Bc"), "abc");
    }

    #[test]
    fn a_sequence_cut_off_by_the_end_of_the_text_leaves_nothing_behind() {
        assert_eq!(strip_ansi("abc\x1b[31"), "abc");
        assert_eq!(strip_ansi("abc\x1b"), "abc");
        assert_eq!(strip_ansi("abc\x1b]0;never closed"), "abc");
    }

    #[test]
    fn text_without_escapes_is_unchanged() {
        assert_eq!(
            strip_ansi("2026-01-02 INFO [worker] 日本 ok"),
            "2026-01-02 INFO [worker] 日本 ok"
        );
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn a_line_is_wrapped_into_rows_of_the_given_width() {
        assert_eq!(wrap_columns("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap_columns("abc", 3), ["abc"]);
        assert_eq!(wrap_columns("abc", 10), ["abc"]);
    }

    #[test]
    fn an_empty_line_wraps_to_one_empty_row() {
        assert_eq!(wrap_columns("", 5), [""]);
    }

    #[test]
    fn a_wide_character_moves_to_the_next_row_rather_than_being_split() {
        assert_eq!(wrap_columns("a日本語", 3), ["a日", "本", "語"]);
    }

    #[test]
    fn a_width_too_small_for_a_character_still_makes_progress() {
        assert_eq!(wrap_columns("日本", 1), ["日", "本"]);
        assert_eq!(wrap_columns("ab", 0), ["ab"]);
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
