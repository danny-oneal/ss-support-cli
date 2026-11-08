//! Renders a run as a markdown report for pasting into tickets.

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::diff::{Change, Kind};
use crate::jsonutil;
use crate::ops::{Input, Meta, Node, View};

/// Renders the report: header, resolved params, then every view in order.
pub fn markdown(meta: &Meta, input: &Input, views: &[View], now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {}\n", meta.name);
    let _ = writeln!(out, "- **Operation:** `{}`", meta.id);
    let _ = writeln!(
        out,
        "- **Generated:** {}",
        now.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let mut params: Vec<String> = input
        .as_map()
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("`{k}={v}`"))
        .collect();
    params.sort();
    if !params.is_empty() {
        let _ = writeln!(out, "- **Params:** {}", params.join(" "));
    }
    out.push('\n');
    for view in views {
        write_view(&mut out, view);
    }
    out
}

fn write_view(out: &mut String, view: &View) {
    match view {
        View::Summary { fields } => {
            out.push_str("## Summary\n\n");
            for (key, value) in fields {
                let _ = writeln!(out, "- **{key}:** {value}");
            }
            out.push('\n');
        }
        View::Diff { left, right, delta } => {
            let _ = write!(out, "## Diff: {left} → {right}\n\n**{}**\n\n", delta.stats);
            if !delta.changes.is_empty() {
                out.push_str("```diff\n");
                for change in &delta.changes {
                    write_change(out, change);
                }
                out.push_str("```\n\n");
            }
        }
        View::Table { title, cols, rows } => {
            write_title(out, title);
            write_table(out, cols, rows);
        }
        View::Doc { title, body, lang } => {
            write_title(out, title);
            let _ = write!(out, "```{lang}\n{}\n```\n\n", body.trim_end_matches('\n'));
        }
        View::Tree { title, root } => {
            write_title(out, title);
            write_node(out, root, 0);
            out.push('\n');
        }
    }
}

fn write_title(out: &mut String, title: &str) {
    if !title.is_empty() {
        let _ = write!(out, "## {title}\n\n");
    }
}

fn write_change(out: &mut String, change: &Change) {
    let path = change.path.dotted();
    let old = change
        .old
        .as_ref()
        .map(jsonutil::compact)
        .unwrap_or_default();
    let new = change
        .new
        .as_ref()
        .map(jsonutil::compact)
        .unwrap_or_default();
    let _ = match change.kind {
        Kind::Added => writeln!(out, "+ {path} = {new}"),
        Kind::Removed => writeln!(out, "- {path} = {old}"),
        Kind::Changed => write!(out, "- {path} = {old}\n+ {path} = {new}\n"),
        Kind::TypeChanged => write!(out, "- {path} = {old}  (type)\n+ {path} = {new}  (type)\n"),
    };
}

fn write_table(out: &mut String, cols: &[String], rows: &[Vec<String>]) {
    out.push('|');
    for col in cols {
        let _ = write!(out, " {} |", escape_cell(col));
    }
    out.push_str("\n|");
    for _ in cols {
        out.push_str("---|");
    }
    out.push('\n');
    for row in rows {
        out.push('|');
        for i in 0..cols.len() {
            let cell = row.get(i).map_or("", String::as_str);
            let _ = write!(out, " {} |", escape_cell(cell));
        }
        out.push('\n');
    }
    out.push('\n');
}

fn escape_cell(cell: &str) -> String {
    cell.replace('|', "\\|")
}

fn write_node(out: &mut String, node: &Node, depth: usize) {
    let _ = writeln!(out, "{}- {}", "  ".repeat(depth), node.label);
    for child in &node.children {
        write_node(out, child, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{self, Delta, Opts, Stats};
    use crate::guard::Class;
    use crate::jsonutil;
    use crate::ops::Node;
    use chrono::TimeZone;

    fn meta() -> Meta {
        Meta {
            id: "s3.version-diff",
            name: "Compare object versions",
            summary: "",
            category: "S3",
            class: Class::Read,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 8, 12, 0, 0).unwrap()
    }

    fn views() -> Vec<View> {
        let left = jsonutil::parse(br#"{"tier":"gold","port":"8080"}"#).unwrap();
        let right = jsonutil::parse(br#"{"tier":"platinum","port":8080}"#).unwrap();
        vec![
            View::summary([("Object", "cfg/a.json")]),
            View::Diff {
                left: "v1".into(),
                right: "current".into(),
                delta: diff::json(&left, &right, &Opts::default()),
            },
            View::Table {
                title: "Versions".into(),
                cols: vec!["ID".into(), "Modified".into()],
                rows: vec![vec!["3HL4".into(), "2026-07-01".into()]],
            },
            View::Doc {
                title: "Raw".into(),
                body: "{\"a\":1}\n\n".into(),
                lang: "json".into(),
            },
            View::Tree {
                title: "Layout".into(),
                root: Node {
                    label: "root".into(),
                    children: vec![Node {
                        label: "leaf".into(),
                        children: vec![],
                    }],
                },
            },
        ]
    }

    #[test]
    fn markdown_renders_every_view_kind() {
        let input = Input::from_iter([("bucket", "cfg-nonprod"), ("empty", "")]);
        let md = markdown(&meta(), &input, &views(), now());
        assert_eq!(
            md,
            "\
# Compare object versions

- **Operation:** `s3.version-diff`
- **Generated:** 2026-11-08T12:00:00Z
- **Params:** `bucket=cfg-nonprod`

## Summary

- **Object:** cfg/a.json

## Diff: v1 → current

**~1 !1**

```diff
- port = \"8080\"  (type)
+ port = 8080  (type)
- tier = \"gold\"
+ tier = \"platinum\"
```

## Versions

| ID | Modified |
|---|---|
| 3HL4 | 2026-07-01 |

## Raw

```json
{\"a\":1}
```

## Layout

- root
  - leaf

"
        );
    }

    #[test]
    fn params_line_is_sorted_and_omitted_when_empty() {
        let md = markdown(
            &meta(),
            &Input::from_iter([("b", "2"), ("a", "1")]),
            &[],
            now(),
        );
        assert!(md.contains("- **Params:** `a=1` `b=2`\n"), "{md}");

        for input in [Input::default(), Input::from_iter([("x", "")])] {
            let md = markdown(&meta(), &input, &[], now());
            assert!(!md.contains("Params"), "{md}");
        }
    }

    #[test]
    fn table_cells_escape_pipes() {
        let view = View::Table {
            title: String::new(),
            cols: vec!["C".into()],
            rows: vec![vec!["a|b".into()]],
        };
        let md = markdown(&meta(), &Input::default(), &[view], now());
        assert!(md.contains("| a\\|b |\n"), "{md}");
    }

    #[test]
    fn empty_diff_has_no_fence() {
        let view = View::Diff {
            left: "a".into(),
            right: "b".into(),
            delta: Delta {
                changes: vec![],
                stats: Stats::default(),
            },
        };
        let md = markdown(&meta(), &Input::default(), &[view], now());
        assert!(md.ends_with("## Diff: a → b\n\n**no changes**\n\n"), "{md}");
        assert!(!md.contains("```diff"), "{md}");
    }
}
