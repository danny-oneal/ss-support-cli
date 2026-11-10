//! `s3.version-diff`: a semantic diff between two versions of one object.

use async_trait::async_trait;

use super::{port, suggest_buckets, suggest_keys, suggest_versions};
use crate::diff::Opts;
use crate::guard::Class;
use crate::ops::compare::{Comparison, compare_bodies};
use crate::ops::{Input, Meta, OpResult, Operation, Param, ParamType, Runner, View};

/// Diffs two versions of one S3 object. Bodies that are not both valid JSON
/// are compared line by line instead.
#[derive(Clone, Copy, Debug, Default)]
pub struct VersionDiff;

#[async_trait]
impl Operation for VersionDiff {
    fn meta(&self) -> Meta {
        Meta {
            id: "s3.version-diff",
            name: "Compare object versions",
            summary: "Semantic diff between two versions of one S3 object",
            category: "S3",
            class: Class::Read,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::new("bucket")
                .label("Bucket")
                .required(true)
                .suggest(suggest_buckets()),
            Param::new("key")
                .label("Key")
                .required(true)
                .suggest(suggest_keys()),
            Param::new("a")
                .label("Version A")
                .required(true)
                .help("older side; a version ID from the key's history")
                .suggest(suggest_versions()),
            Param::new("b")
                .label("Version B")
                .default("null")
                .help(r#"newer side; "null" = current"#)
                .suggest(suggest_versions()),
            Param::new("ignore")
                .label("Ignore paths")
                .ty(ParamType::Json)
                .help(r#"extra ignore globs, e.g. "**.updatedAt,**.etag""#),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let s3 = port(&r.deps)?;
        let (bucket, key) = (input.str("bucket"), input.str("key"));
        let (a, b) = (input.str("a"), input.str("b"));

        let (obj_a, obj_b) = r
            .step("fetch both versions", |_| async {
                let (obj_a, obj_b) = tokio::try_join!(
                    s3.get_version(bucket, key, a),
                    s3.get_version(bucket, key, b),
                )?;
                Ok((obj_a, obj_b))
            })
            .await?;
        let (label_a, label_b) = (obj_a.version_label(), obj_b.version_label());

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
            ..Opts::default()
        };
        let comparison = r
            .step("parse and diff", |_| async {
                Ok(compare_bodies(&obj_a.body, &obj_b.body, &opts))
            })
            .await?;

        let object = format!("{bucket}/{key}");
        let views = match comparison {
            Comparison::Lines(body) => vec![
                View::summary([
                    ("Object", object.as_str()),
                    ("A", label_a.as_str()),
                    ("B", label_b.as_str()),
                    ("Mode", "line diff (body is not valid JSON)"),
                ]),
                View::Doc {
                    title: format!("Line diff: {label_a} → {label_b}"),
                    body,
                    lang: "diff".into(),
                },
            ],
            Comparison::Json(delta) => vec![
                View::summary([
                    ("Object", object.as_str()),
                    ("A", label_a.as_str()),
                    ("B", label_b.as_str()),
                    ("Changes", delta.stats.to_string().as_str()),
                ]),
                View::Diff {
                    left: label_a,
                    right: label_b,
                    delta,
                },
            ],
        };
        Ok(OpResult { views })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::find_cause;
    use crate::ops::s3::fixture;
    use crate::ops::{Deps, Input, Runner, View};
    use crate::svc::SvcError;

    const KEY: &str = "tenants/acme.json";
    const LABEL_V1: &str = "v1 (2026-07-01 10:00)";
    const LABEL_V2: &str = "v2 (2026-07-02 10:00)";

    fn input(pairs: &[(&str, &str)]) -> Input {
        pairs.iter().copied().collect()
    }

    async fn run(deps: Deps, pairs: &[(&str, &str)]) -> Vec<View> {
        Runner::new(deps)
            .execute(&VersionDiff, input(pairs))
            .await
            .expect("diff succeeds")
            .views
    }

    #[tokio::test]
    async fn keyed_and_ignored_diff() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[
                ("bucket", fixture::BUCKET),
                ("key", KEY),
                ("a", "v1"),
                ("b", "v2"),
            ],
        )
        .await;
        assert_eq!(
            views[0],
            View::summary([
                ("Object", "cfg-nonprod/tenants/acme.json"),
                ("A", LABEL_V1),
                ("B", LABEL_V2),
                ("Changes", "+1 ~1"),
            ])
        );
        let View::Diff { left, right, delta } = &views[1] else {
            panic!("second view is a diff");
        };
        assert_eq!((left.as_str(), right.as_str()), (LABEL_V1, LABEL_V2));
        assert_eq!(
            (delta.stats.added, delta.stats.changed, delta.stats.total()),
            (1, 1, 2)
        );
        let paths: Vec<_> = delta.changes.iter().map(|c| c.path.dotted()).collect();
        assert!(paths.contains(&"rules[id=r0]".to_owned()), "{paths:?}");
        assert!(paths.contains(&"tier".to_owned()), "{paths:?}");
    }

    #[tokio::test]
    async fn b_defaults_to_current() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[("bucket", fixture::BUCKET), ("key", KEY), ("a", "v1")],
        )
        .await;
        assert_eq!(
            views[0],
            View::summary([
                ("Object", "cfg-nonprod/tenants/acme.json"),
                ("A", LABEL_V1),
                ("B", LABEL_V2),
                ("Changes", "+1 ~1"),
            ])
        );
    }

    #[tokio::test]
    async fn ignore_param_adds_to_config_globs() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[
                ("bucket", fixture::BUCKET),
                ("key", KEY),
                ("a", "v1"),
                ("b", "v2"),
                ("ignore", "**.tier"),
            ],
        )
        .await;
        assert_eq!(
            views[0],
            View::summary([
                ("Object", "cfg-nonprod/tenants/acme.json"),
                ("A", LABEL_V1),
                ("B", LABEL_V2),
                ("Changes", "+1"),
            ])
        );
    }

    #[tokio::test]
    async fn non_json_bodies_fall_back_to_a_line_diff() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[
                ("bucket", fixture::BUCKET),
                ("key", "tenants/notes.txt"),
                ("a", "t1"),
                ("b", "t2"),
            ],
        )
        .await;
        assert_eq!(
            views[0],
            View::summary([
                ("Object", "cfg-nonprod/tenants/notes.txt"),
                ("A", "t1 (2026-07-01 10:00)"),
                ("B", "t2 (2026-07-01 11:00)"),
                ("Mode", "line diff (body is not valid JSON)"),
            ])
        );
        let View::Doc { title, body, lang } = &views[1] else {
            panic!("second view is a doc");
        };
        assert_eq!(
            title,
            "Line diff: t1 (2026-07-01 10:00) → t2 (2026-07-01 11:00)"
        );
        assert_eq!(lang, "diff");
        assert!(body.contains("-world"), "{body}");
        assert!(body.contains("+there"), "{body}");
    }

    #[tokio::test]
    async fn a_missing_version_fails_as_not_found() {
        let failure = Runner::new(fixture::deps(fixture::fake()))
            .execute(
                &VersionDiff,
                input(&[("bucket", fixture::BUCKET), ("key", KEY), ("a", "nope")]),
            )
            .await
            .expect_err("unknown version fails");
        assert!(
            find_cause::<SvcError>(&failure.error).is_some_and(SvcError::is_not_found),
            "{:#}",
            failure.error
        );
        assert!(
            format!("{:#}", failure.error).starts_with("fetch both versions: "),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn without_s3_the_run_fails_clearly() {
        let failure = Runner::new(Deps::default())
            .execute(
                &VersionDiff,
                input(&[("bucket", fixture::BUCKET), ("key", KEY), ("a", "v1")]),
            )
            .await
            .expect_err("no port fails");
        assert!(
            format!("{:#}", failure.error).contains("S3 is not configured"),
            "{:#}",
            failure.error
        );
    }
}
