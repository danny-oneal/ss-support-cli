//! `demo.json-diff`: a semantic diff of two local JSON files.

use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;

use crate::diff::{self, Opts};
use crate::guard::Class;
use crate::jsonutil;
use crate::ops::{
    Choice, Input, Meta, OpResult, Operation, Param, ParamType, Runner, SuggestFn, View,
};

/// Diffs two local JSON files through the same engine the cloud diff
/// operations use. Input that is not valid JSON degrades to a line diff.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonDiff;

/// How the two documents were compared.
enum Comparison {
    /// Both sides parsed; compared structurally.
    Json(diff::Delta),
    /// At least one side did not parse; compared line by line.
    Lines(String),
}

#[async_trait]
impl Operation for JsonDiff {
    fn meta(&self) -> Meta {
        Meta {
            id: "demo.json-diff",
            name: "Diff two JSON files",
            summary: "Semantic diff of two local JSON documents",
            category: "Demo",
            class: Class::Read,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::new("left")
                .label("Left file")
                .required(true)
                .help("Path to JSON file A")
                .suggest(suggest_json_files()),
            Param::new("right")
                .label("Right file")
                .required(true)
                .help("Path to JSON file B")
                .suggest(suggest_json_files()),
            Param::new("ignore")
                .label("Ignore paths")
                .ty(ParamType::Json)
                .help(r#"Glob list, e.g. "**.updatedAt,**.etag" or ["**.updatedAt"]"#),
            Param::new("tolerance")
                .label("Float tolerance")
                .default("0")
                .help("Epsilon for numeric comparison"),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let (left_path, right_path) = (input.str("left"), input.str("right"));

        let tolerance = match input.str("tolerance").trim() {
            "" => 0.0,
            v => v
                .parse::<f64>()
                .with_context(|| format!("invalid tolerance {v:?}"))?,
        };

        let (left_raw, right_raw) = r
            .step("read both files", |_| async {
                tokio::try_join!(read(left_path), read(right_path))
            })
            .await?;

        let opts = Opts {
            ignore: r
                .deps
                .diff
                .ignore
                .iter()
                .cloned()
                .chain(input.globs("ignore"))
                .collect(),
            array_match: r.deps.diff.array_match.clone(),
            tolerance,
            ..Opts::default()
        };
        let comparison = r
            .step("parse and diff", |_| async {
                Ok(
                    match (jsonutil::parse(&left_raw), jsonutil::parse(&right_raw)) {
                        (Ok(left), Ok(right)) => Comparison::Json(diff::json(&left, &right, &opts)),
                        _ => Comparison::Lines(diff::text(
                            &String::from_utf8_lossy(&left_raw),
                            &String::from_utf8_lossy(&right_raw),
                        )),
                    },
                )
            })
            .await?;

        let views = match comparison {
            Comparison::Lines(body) => vec![
                View::summary([
                    ("Left", left_path),
                    ("Right", right_path),
                    ("Mode", "line diff (at least one side is not valid JSON)"),
                ]),
                View::Doc {
                    title: "Line diff".into(),
                    body,
                    lang: "diff".into(),
                },
            ],
            Comparison::Json(delta) => vec![
                View::summary([
                    ("Left", left_path),
                    ("Right", right_path),
                    ("Changes", delta.stats.to_string().as_str()),
                ]),
                View::Diff {
                    left: base_name(left_path),
                    right: base_name(right_path),
                    delta,
                },
            ],
        };
        Ok(OpResult { views })
    }
}

async fn read(path: &str) -> anyhow::Result<Vec<u8>> {
    tokio::fs::read(path)
        .await
        .with_context(|| format!("open {path}"))
}

fn base_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// The `*.json` files directly inside `dir`, sorted by name, as suggestions.
///
/// # Errors
/// Fails when the directory cannot be read.
pub fn json_files_in(dir: &Path) -> io::Result<Vec<Choice>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(name) = entry.file_name().into_string()
            && name.ends_with(".json")
        {
            names.push(name);
        }
    }
    names.sort();
    Ok(names
        .into_iter()
        .map(|value| Choice { value, label: None })
        .collect())
}

fn suggest_json_files() -> SuggestFn {
    Arc::new(|_deps, _input| {
        Box::pin(async {
            json_files_in(Path::new(".")).context("list json files in the current directory")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Deps, FailureKind, Input, Runner, View};

    fn write(dir: &std::path::Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, body).expect("write fixture");
        p.to_str().expect("utf-8 path").to_owned()
    }

    #[tokio::test]
    async fn diffs_json_files_with_config_and_param_ignores() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write(dir.path(), "a.json", r#"{"a":1,"t":"x","u":1}"#);
        let b = write(dir.path(), "b.json", r#"{"a":2,"t":"y","u":2}"#);
        let mut deps = Deps::default();
        deps.diff.ignore = vec!["**.t".into()];
        let input: Input = [
            ("left", a.as_str()),
            ("right", b.as_str()),
            ("ignore", "**.u"),
        ]
        .into_iter()
        .collect();
        let views = Runner::new(deps)
            .execute(&JsonDiff, input)
            .await
            .expect("diff succeeds")
            .views;
        assert_eq!(
            views[0],
            View::summary([
                ("Left", a.as_str()),
                ("Right", b.as_str()),
                ("Changes", "~1")
            ])
        );
        let View::Diff { left, right, delta } = &views[1] else {
            panic!("second view is a diff");
        };
        assert_eq!((left.as_str(), right.as_str()), ("a.json", "b.json"));
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].path.dotted(), "a");
    }

    #[tokio::test]
    async fn falls_back_to_line_diff_for_invalid_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write(dir.path(), "a.json", "not json\n");
        let b = write(dir.path(), "b.json", "{}");
        let input: Input = [("left", a.as_str()), ("right", b.as_str())]
            .into_iter()
            .collect();
        let views = Runner::new(Deps::default())
            .execute(&JsonDiff, input)
            .await
            .expect("diff succeeds")
            .views;
        assert_eq!(
            views[0],
            View::summary([
                ("Left", a.as_str()),
                ("Right", b.as_str()),
                ("Mode", "line diff (at least one side is not valid JSON)"),
            ])
        );
        let View::Doc { title, body, lang } = &views[1] else {
            panic!("second view is a doc");
        };
        assert_eq!(title, "Line diff");
        assert_eq!(lang, "diff");
        assert_eq!(body, &crate::diff::text("not json\n", "{}"));
    }

    #[tokio::test]
    async fn missing_file_names_the_step_and_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let left = dir.path().join("missing.json").to_str().unwrap().to_owned();
        let right = write(dir.path(), "b.json", "{}");
        let input: Input = [("left", left.as_str()), ("right", right.as_str())]
            .into_iter()
            .collect();
        let failure = Runner::new(Deps::default())
            .execute(&JsonDiff, input)
            .await
            .expect_err("missing file fails");
        assert_eq!(failure.kind, FailureKind::Failed);
        let msg = format!("{:#}", failure.error);
        assert!(
            msg.starts_with(&format!("read both files: open {left}: ")),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn bad_tolerance_fails_before_reading() {
        let input: Input = [
            ("left", "/nonexistent/a.json"),
            ("right", "/nonexistent/b.json"),
            ("tolerance", "x"),
        ]
        .into_iter()
        .collect();
        let failure = Runner::new(Deps::default())
            .execute(&JsonDiff, input)
            .await
            .expect_err("bad tolerance fails");
        let msg = format!("{:#}", failure.error);
        assert!(msg.starts_with(r#"invalid tolerance "x": "#), "{msg}");
    }

    #[test]
    fn json_files_in_lists_only_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.json", "{}");
        write(dir.path(), "b.txt", "x");
        let choices = json_files_in(dir.path()).expect("list");
        assert_eq!(
            choices,
            vec![crate::ops::Choice {
                value: "a.json".into(),
                label: None
            }]
        );
    }
}
