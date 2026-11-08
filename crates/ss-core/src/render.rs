//! Renders [`View`] lists as terminal text and as the `-o json` value.

use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::diff::{Change, Delta, Kind};
use crate::jsonutil;
use crate::ops::{Node, View};

/// Renders views as plain terminal text, separated by one blank line.
pub fn text(views: &[View]) -> String {
    let mut out = String::new();
    for (i, view) in views.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        write_view(&mut out, view);
    }
    out
}

/// Renders views as `{"views":[…]}`, one typed object per view.
pub fn json(views: &[View]) -> Value {
    json!({ "views": views.iter().map(view_json).collect::<Vec<_>>() })
}

fn write_view(out: &mut String, view: &View) {
    match view {
        View::Summary { fields } => write_summary(out, fields),
        View::Diff { left, right, delta } => write_diff(out, left, right, delta),
        View::Table { title, cols, rows } => {
            write_title(out, title);
            write_table(out, cols, rows);
        }
        View::Doc { title, body, .. } => {
            write_title(out, title);
            out.push_str(body.trim_end_matches('\n'));
            out.push('\n');
        }
        View::Tree { title, root } => {
            write_title(out, title);
            write_node(out, root, 0);
        }
    }
}

fn write_title(out: &mut String, title: &str) {
    if !title.is_empty() {
        out.push_str(title);
        out.push('\n');
    }
}

fn write_summary(out: &mut String, fields: &[(String, String)]) {
    let width = fields
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    for (key, value) in fields {
        let _ = writeln!(out, "{key:<width$} : {value}");
    }
}

fn write_diff(out: &mut String, left: &str, right: &str, delta: &Delta) {
    let _ = writeln!(out, "diff {left} → {right}   ({})", delta.stats);
    for change in &delta.changes {
        write_change(out, change);
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
        Kind::Added => writeln!(out, "  + {path} = {new}"),
        Kind::Removed => writeln!(out, "  - {path} = {old}"),
        Kind::Changed => writeln!(out, "  ~ {path}: {old} -> {new}"),
        Kind::TypeChanged => writeln!(out, "  ! {path}: {old} -> {new} (type changed)"),
    };
}

fn write_table(out: &mut String, cols: &[String], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    write_row(out, &widths, cols);
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    write_row(out, &widths, &rule);
    for row in rows {
        write_row(out, &widths, row);
    }
}

fn write_row(out: &mut String, widths: &[usize], cells: &[String]) {
    let mut line = String::new();
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        let cell = cells.get(i).map_or("", String::as_str);
        let _ = write!(line, "{cell:<width$}");
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

fn write_node(out: &mut String, node: &Node, depth: usize) {
    let _ = writeln!(out, "{}{}", "  ".repeat(depth), node.label);
    for child in &node.children {
        write_node(out, child, depth + 1);
    }
}

fn view_json(view: &View) -> Value {
    match view {
        View::Summary { fields } => {
            let map: Map<String, Value> = fields
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            json!({ "type": "summary", "fields": map })
        }
        View::Diff { left, right, delta } => json!({
            "type": "diff",
            "left": left,
            "right": right,
            "delta": serde_json::to_value(delta).unwrap_or(Value::Null),
        }),
        View::Table { title, cols, rows } => {
            json!({ "type": "table", "title": title, "cols": cols, "rows": rows })
        }
        View::Doc { title, body, lang } => {
            json!({ "type": "doc", "title": title, "lang": lang, "body": body })
        }
        View::Tree { title, root } => {
            json!({ "type": "tree", "title": title, "root": node_json(root) })
        }
    }
}

fn node_json(node: &Node) -> Value {
    json!({
        "label": node.label,
        "children": node.children.iter().map(node_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{self, Opts};
    use crate::jsonutil;
    use crate::ops::Node;
    use serde_json::json;

    fn all_views() -> Vec<View> {
        let left = jsonutil::parse(br#"{"tier":"gold","port":"8080"}"#).unwrap();
        let right = jsonutil::parse(br#"{"tier":"platinum","port":8080}"#).unwrap();
        vec![
            View::summary([("Object", "cfg/a.json"), ("Version", "v1")]),
            View::Diff {
                left: "v1".into(),
                right: "current".into(),
                delta: diff::json(&left, &right, &Opts::default()),
            },
            View::Table {
                title: "Versions".into(),
                cols: vec!["ID".into(), "Modified".into()],
                rows: vec![vec!["3HL4".into(), "2026-07-01".into()], vec!["a".into()]],
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
    fn text_renders_every_view_kind() {
        assert_eq!(
            text(&all_views()),
            "\
Object  : cfg/a.json
Version : v1

diff v1 → current   (~1 !1)
  ! port: \"8080\" -> 8080 (type changed)
  ~ tier: \"gold\" -> \"platinum\"

Versions
ID    Modified
----  ----------
3HL4  2026-07-01
a

Raw
{\"a\":1}

Layout
root
  leaf
"
        );
    }

    #[test]
    fn untitled_table_has_no_title_line() {
        let v = View::Table {
            title: String::new(),
            cols: vec!["A".into()],
            rows: vec![],
        };
        assert_eq!(text(&[v]), "A\n-\n");
    }

    #[test]
    fn json_has_the_typed_view_shape() {
        let v = json(&all_views());
        assert_eq!(
            v["views"][0],
            json!({"type":"summary","fields":{"Object":"cfg/a.json","Version":"v1"}})
        );
        assert_eq!(
            v["views"][2],
            json!({"type":"table","title":"Versions","cols":["ID","Modified"],"rows":[["3HL4","2026-07-01"],["a"]]})
        );
        assert_eq!(
            v["views"][3],
            json!({"type":"doc","title":"Raw","lang":"json","body":"{\"a\":1}\n\n"})
        );
        assert_eq!(
            v["views"][4],
            json!({"type":"tree","title":"Layout","root":{"label":"root","children":[{"label":"leaf","children":[]}]}})
        );
        assert_eq!(v["views"][1]["type"], "diff");
        assert_eq!(v["views"][1]["left"], "v1");
        assert_eq!(v["views"][1]["right"], "current");
        assert_eq!(
            v["views"][1]["delta"]["stats"],
            json!({"added":0,"removed":0,"changed":1,"type_changed":1})
        );
    }

    #[test]
    fn summary_later_duplicate_keys_win() {
        let v = json(&[View::summary([("K", "first"), ("K", "second")])]);
        assert_eq!(v["views"][0]["fields"], json!({"K":"second"}));
    }

    #[test]
    fn json_numbers_print_without_fraction() {
        let out = jsonutil::pretty(&json(&all_views()));
        assert!(out.contains("\"new\": 8080,"), "{out}");
        assert!(!out.contains("8080.0"), "{out}");
    }

    #[test]
    fn empty_table_rows_are_an_empty_array() {
        let out = jsonutil::pretty(&json(&[View::Table {
            title: String::new(),
            cols: vec![],
            rows: vec![],
        }]));
        assert!(out.contains("\"rows\": []"), "{out}");
    }

    #[test]
    fn widths_count_chars_not_bytes() {
        let v = View::Table {
            title: String::new(),
            cols: vec!["Name".into(), "N".into()],
            rows: vec![vec!["héllo".into(), "1".into()]],
        };
        assert_eq!(text(&[v]), "Name   N\n-----  -\nhéllo  1\n");
    }
}
