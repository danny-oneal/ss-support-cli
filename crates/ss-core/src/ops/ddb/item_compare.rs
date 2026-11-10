//! `ddb.item-compare`: a semantic diff between two DynamoDB items.

use async_trait::async_trait;
use serde_json::Value;

use super::{parse_key, port, suggest_tables};
use crate::diff::{self, Opts};
use crate::guard::Class;
use crate::ops::{Input, Meta, OpResult, Operation, Param, ParamType, Runner, View};
use crate::svc::ddb::key_string;

/// Diffs two DynamoDB items, each named by its key, within one table or
/// across two tables.
#[derive(Clone, Copy, Debug, Default)]
pub struct ItemCompare;

#[async_trait]
impl Operation for ItemCompare {
    fn meta(&self) -> Meta {
        Meta {
            id: "ddb.item-compare",
            name: "Compare items",
            summary: "Semantic diff between two DynamoDB items",
            category: "DynamoDB",
            class: Class::Read,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::new("table")
                .label("Table")
                .required(true)
                .suggest(suggest_tables()),
            Param::new("key-a").label("Key A").required(true).help(
                r#"item key: {"TenantId":"acme","Version":"V0"} or TenantId=acme,Version=V0"#,
            ),
            Param::new("key-b")
                .label("Key B")
                .required(true)
                .help("second item's key, same formats"),
            Param::new("table-b")
                .label("Table B")
                .help("compare across tables; defaults to Table")
                .suggest(suggest_tables()),
            Param::new("ignore")
                .label("Ignore paths")
                .ty(ParamType::Json)
                .help(r#"extra ignore globs, e.g. "**.updatedAt""#),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let ddb = port(&r.deps)?;
        let table_a = input.str("table");
        let table_b = match input.str("table-b").trim() {
            "" => table_a,
            other => other,
        };

        let (info_a, info_b) = r
            .step("describe tables", |_| async {
                if table_a == table_b {
                    let info = ddb.describe_table(table_a).await?;
                    return Ok((info.clone(), info));
                }
                let (info_a, info_b) =
                    tokio::try_join!(ddb.describe_table(table_a), ddb.describe_table(table_b),)?;
                Ok((info_a, info_b))
            })
            .await?;
        let key_a = parse_key(input.str("key-a"), &info_a).map_err(|e| e.context("key A"))?;
        let key_b = parse_key(input.str("key-b"), &info_b).map_err(|e| e.context("key B"))?;
        let (label_a, label_b) = (
            format!("{table_a} {}", key_string(&key_a)),
            format!("{table_b} {}", key_string(&key_b)),
        );

        let (item_a, item_b) = r
            .step("fetch both items", |_| async {
                let (item_a, item_b) = tokio::try_join!(
                    ddb.get_item(table_a, &key_a),
                    ddb.get_item(table_b, &key_b),
                )?;
                Ok((item_a, item_b))
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
            ..Opts::default()
        };
        let delta = diff::json(&Value::Object(item_a), &Value::Object(item_b), &opts);

        Ok(OpResult {
            views: vec![
                View::summary([
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
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::ddb::fixture;
    use crate::ops::{Deps, Input, RunFailure, Runner, View, find_cause};
    use crate::svc::SvcError;
    use crate::svc::ddb::{FakeDdb, Item, contract};
    use serde_json::{Value, json};

    fn input(pairs: &[(&str, &str)]) -> Input {
        pairs.iter().copied().collect()
    }

    async fn run(ddb: FakeDdb, pairs: &[(&str, &str)]) -> Vec<View> {
        Runner::new(fixture::deps(ddb))
            .execute(&ItemCompare, input(pairs))
            .await
            .expect("compare succeeds")
            .views
    }

    async fn fail(deps: Deps, pairs: &[(&str, &str)]) -> RunFailure {
        Runner::new(deps)
            .execute(&ItemCompare, input(pairs))
            .await
            .expect_err("run fails")
    }

    fn paths(view: &View) -> Vec<String> {
        let View::Diff { delta, .. } = view else {
            panic!("expected a diff view");
        };
        delta.changes.iter().map(|c| c.path.dotted()).collect()
    }

    #[tokio::test]
    async fn compares_two_keys_in_both_key_syntaxes() {
        let views = run(
            fixture::fake(),
            &[
                ("table", "tenants"),
                ("key-a", "TenantId=acme,Version=V0"),
                ("key-b", r#"{"TenantId":"beta","Version":"V0"}"#),
            ],
        )
        .await;
        let View::Diff { left, right, delta } = &views[1] else {
            panic!("second view is a diff");
        };
        assert_eq!(left, "tenants TenantId=acme|Version=V0");
        assert_eq!(right, "tenants TenantId=beta|Version=V0");
        assert_eq!(
            views[0],
            View::summary([
                ("A", "tenants TenantId=acme|Version=V0"),
                ("B", "tenants TenantId=beta|Version=V0"),
                ("Changes", delta.stats.to_string().as_str()),
            ])
        );
        let changed = paths(&views[1]);
        assert!(changed.contains(&"Tier".to_owned()), "{changed:?}");
        assert!(changed.contains(&"limits.rps".to_owned()), "{changed:?}");
        assert!(
            changed.iter().all(|p| !p.contains("updatedAt")),
            "{changed:?}"
        );
    }

    #[tokio::test]
    async fn identical_items_report_no_changes() {
        let views = run(
            fixture::fake(),
            &[
                ("table", "tenants"),
                ("key-a", "TenantId=acme,Version=V0"),
                ("key-b", "TenantId=acme,Version=V0"),
            ],
        )
        .await;
        assert_eq!(
            views[0],
            View::summary([
                ("A", "tenants TenantId=acme|Version=V0"),
                ("B", "tenants TenantId=acme|Version=V0"),
                ("Changes", "no changes"),
            ])
        );
        let View::Diff { delta, .. } = &views[1] else {
            panic!("second view is a diff");
        };
        assert_eq!(delta.stats.total(), 0);
    }

    #[tokio::test]
    async fn compares_across_tables() {
        let ddb = fixture::fake();
        ddb.create_table(contract::table_info("tenants-qa"));
        let Value::Object(qa) = json!({
            "TenantId": "acme",
            "Version": "V0",
            "Tier": "platinum",
            "limits": {"rps": 999},
            "updatedAt": "2026-07-V0",
        }) else {
            unreachable!("an object literal is an object")
        };
        let qa: Item = qa;
        ddb.put("tenants-qa", qa);
        let views = run(
            ddb,
            &[
                ("table", "tenants"),
                ("key-a", "TenantId=acme,Version=V0"),
                ("key-b", "TenantId=acme,Version=V0"),
                ("table-b", "tenants-qa"),
            ],
        )
        .await;
        let View::Diff { left, right, .. } = &views[1] else {
            panic!("second view is a diff");
        };
        assert!(left.starts_with("tenants "), "{left}");
        assert!(right.starts_with("tenants-qa "), "{right}");
        assert_eq!(paths(&views[1]), ["limits.rps"]);
    }

    #[tokio::test]
    async fn ignore_param_adds_to_config_globs() {
        let base = [
            ("table", "tenants"),
            ("key-a", "TenantId=acme,Version=V0"),
            ("key-b", "TenantId=acme,Version=V1"),
        ];
        let before = paths(&run(fixture::fake(), &base).await[1]);
        assert!(before.contains(&"Tier".to_owned()), "{before:?}");

        let mut ignoring = base.to_vec();
        ignoring.push(("ignore", "**.Tier"));
        let after = paths(&run(fixture::fake(), &ignoring).await[1]);
        assert!(!after.contains(&"Tier".to_owned()), "{after:?}");
        assert!(after.contains(&"limits.rps".to_owned()), "{after:?}");
    }

    #[tokio::test]
    async fn a_missing_item_fails_as_not_found() {
        let f = fail(
            fixture::deps(fixture::fake()),
            &[
                ("table", "tenants"),
                ("key-a", "TenantId=acme,Version=V0"),
                ("key-b", "TenantId=nope,Version=V0"),
            ],
        )
        .await;
        assert!(
            find_cause::<SvcError>(&f.error).is_some_and(SvcError::is_not_found),
            "{:#}",
            f.error
        );
        assert!(
            format!("{:#}", f.error).starts_with("fetch both items: "),
            "{:#}",
            f.error
        );
    }

    #[tokio::test]
    async fn an_unknown_table_fails_as_not_found_in_the_describe_step() {
        let f = fail(
            fixture::deps(fixture::fake()),
            &[
                ("table", "missing"),
                ("key-a", "TenantId=acme,Version=V0"),
                ("key-b", "TenantId=acme,Version=V1"),
            ],
        )
        .await;
        assert!(
            find_cause::<SvcError>(&f.error).is_some_and(SvcError::is_not_found),
            "{:#}",
            f.error
        );
        assert!(
            format!("{:#}", f.error).starts_with("describe tables: "),
            "{:#}",
            f.error
        );
    }

    #[tokio::test]
    async fn a_malformed_key_fails_before_any_fetch() {
        let f = fail(
            fixture::deps(fixture::fake()),
            &[
                ("table", "tenants"),
                ("key-a", "nonsense"),
                ("key-b", "TenantId=acme,Version=V1"),
            ],
        )
        .await;
        let text = format!("{:#}", f.error);
        assert!(text.contains("is not name=value"), "{text}");
        assert!(!text.contains("fetch both items"), "{text}");
    }

    #[tokio::test]
    async fn without_ddb_the_run_fails_clearly() {
        let f = fail(
            Deps::default(),
            &[
                ("table", "tenants"),
                ("key-a", "TenantId=acme,Version=V0"),
                ("key-b", "TenantId=acme,Version=V1"),
            ],
        )
        .await;
        assert!(
            format!("{:#}", f.error).contains("DynamoDB is not configured"),
            "{:#}",
            f.error
        );
    }
}
