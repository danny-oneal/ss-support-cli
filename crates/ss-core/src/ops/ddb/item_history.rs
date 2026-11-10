//! `ddb.item-history`: the versions of one item and a diff between two of them.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::{
    coerce, compact_preview, item_version, port, sort_by_version, suggest_tables, version_num,
};
use crate::diff::{self, Opts};
use crate::guard::Class;
use crate::ops::{
    Choice, Deps, Input, Meta, OpResult, Operation, Param, ParamType, Runner, SuggestFn, View,
};
use crate::svc::SvcError;
use crate::svc::ddb::{DdbPort, Item, QuerySpec, TableInfo};

/// The attribute holding an item's version label unless `version-attr` says
/// otherwise.
pub(crate) const DEFAULT_VERSION_ATTR: &str = "Version";

/// The number of versions fetched when `limit` is missing or not positive.
const DEFAULT_LIMIT: usize = 50;

/// The most versions one run fetches.
const MAX_LIMIT: usize = 10_000;

/// The widest item preview in the version table, in characters.
const PREVIEW_WIDTH: usize = 80;

/// Lists the versions stored under one partition key (`V0` is the latest) and
/// diffs two of them.
#[derive(Clone, Copy, Debug, Default)]
pub struct ItemHistory;

/// The fetch limit for a raw `limit` value: positive values are capped at
/// [`MAX_LIMIT`], anything else falls back to [`DEFAULT_LIMIT`].
fn clamp_limit(raw: i64) -> usize {
    usize::try_from(raw)
        .ok()
        .filter(|n| *n > 0)
        .map_or(DEFAULT_LIMIT, |n| n.min(MAX_LIMIT))
}

/// `value` unless it is blank, in which case `fallback`.
fn or_default<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    match value.trim() {
        "" => fallback,
        other => other,
    }
}

/// Up to `limit` items of `table` under the partition key `pk`, in version
/// order. Pages are drained until the service reports no more or `limit`
/// items are held, so the page size never changes the result.
async fn query_versions(
    ddb: &dyn DdbPort,
    info: &TableInfo,
    table: &str,
    pk: &str,
    version_attr: &str,
    limit: usize,
) -> Result<Vec<Item>, SvcError> {
    let pk_value = coerce(
        Value::String(pk.to_owned()),
        info.attr_types.get(&info.hash_key).copied(),
    );
    let mut spec = QuerySpec {
        table: table.to_owned(),
        key_condition: "#pk = :pk".to_owned(),
        names: [("#pk".to_owned(), info.hash_key.clone())].into(),
        values: [(":pk".to_owned(), pk_value)].into(),
        limit: i32::try_from(limit).ok(),
        ..QuerySpec::default()
    };
    let mut items = Vec::new();
    loop {
        let page = ddb.query(&spec).await?;
        items.extend(page.items);
        match page.next {
            Some(next) if items.len() < limit => spec.exclusive_start = Some(next),
            _ => break,
        }
    }
    items.truncate(limit);
    sort_by_version(&mut items, version_attr);
    Ok(items)
}

/// Suggests the version labels present under the typed `pk`, the latest
/// marked as such.
pub(crate) fn suggest_item_versions() -> SuggestFn {
    Arc::new(|deps: Deps, input: Input| {
        Box::pin(async move {
            let (table, pk) = (input.str("table"), input.str("pk"));
            let Some(ddb) = deps
                .ddb
                .filter(|_| !table.is_empty() && !pk.trim().is_empty())
            else {
                return Ok(Vec::new());
            };
            let attr = or_default(input.str("version-attr"), DEFAULT_VERSION_ATTR);
            let info = ddb.describe_table(table).await?;
            let items = query_versions(&*ddb, &info, table, pk, attr, DEFAULT_LIMIT).await?;
            Ok(items
                .iter()
                .map(|item| item_version(item, attr))
                .filter(|label| !label.is_empty())
                .map(|label| {
                    let latest = version_num(&label) == Some(0);
                    Choice {
                        label: latest.then(|| format!("{label} — latest")),
                        value: label,
                    }
                })
                .collect())
        })
    })
}

#[async_trait]
impl Operation for ItemHistory {
    fn meta(&self) -> Meta {
        Meta {
            id: "ddb.item-history",
            name: "Item history",
            summary: "List an item's versions (V0 = latest) and diff two of them",
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
            Param::new("pk")
                .label("Partition key value")
                .required(true)
                .help("value of the table's partition key, e.g. acme"),
            Param::new("a")
                .label("Version A")
                .default("V1")
                .help("older side of the diff")
                .suggest(suggest_item_versions()),
            Param::new("b")
                .label("Version B")
                .default("V0")
                .help("newer side of the diff; V0 = latest")
                .suggest(suggest_item_versions()),
            Param::new("version-attr")
                .label("Version attribute")
                .default(DEFAULT_VERSION_ATTR)
                .help("attribute holding the version label"),
            Param::new("limit")
                .label("Max versions")
                .ty(ParamType::Int)
                .default("50"),
            Param::new("ignore")
                .label("Ignore paths")
                .ty(ParamType::Json)
                .help(r#"extra ignore globs, e.g. "**.updatedAt""#),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let ddb = port(&r.deps)?;
        let (table, pk) = (input.str("table"), input.str("pk"));
        let label_a = or_default(input.str("a"), "V1");
        let label_b = or_default(input.str("b"), "V0");
        let version_attr = or_default(input.str("version-attr"), DEFAULT_VERSION_ATTR);
        let limit = clamp_limit(input.int("limit"));

        let (info, items) = r
            .step("query versions", |_| async {
                let info = ddb.describe_table(table).await?;
                let items = query_versions(&*ddb, &info, table, pk, version_attr, limit).await?;
                Ok((info, items))
            })
            .await?;
        if items.is_empty() {
            anyhow::bail!("no items found for {}={pk:?} in {table}", info.hash_key);
        }

        let skip: BTreeSet<String> = [Some(info.hash_key.clone()), info.sort_key.clone()]
            .into_iter()
            .flatten()
            .chain(std::iter::once(version_attr.to_owned()))
            .collect();
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|item| {
                let label = item_version(item, version_attr);
                let latest = version_num(&label) == Some(0);
                vec![
                    label,
                    if latest { "latest" } else { "" }.to_owned(),
                    compact_preview(item, &skip, PREVIEW_WIDTH),
                ]
            })
            .collect();

        let mut summary = vec![
            ("Item".to_owned(), format!("{table} {}={pk}", info.hash_key)),
            ("Versions".to_owned(), items.len().to_string()),
        ];
        let mut diff_view = None;
        if items.len() < 2 {
            summary.push((
                "Note".to_owned(),
                "only one version exists; nothing to diff".to_owned(),
            ));
        } else {
            let find = |label: &str| {
                items
                    .iter()
                    .find(|i| item_version(i, version_attr) == label)
            };
            let (item_a, item_b) = match (find(label_a), find(label_b)) {
                (Some(a), Some(b)) => (a, b),
                (found_a, _) => {
                    let missing = if found_a.is_none() { label_a } else { label_b };
                    let have: Vec<String> = items
                        .iter()
                        .map(|i| item_version(i, version_attr))
                        .filter(|l| !l.is_empty())
                        .collect();
                    anyhow::bail!("version {missing:?} not found (have: {})", have.join(", "));
                }
            };
            let opts = Opts {
                ignore: std::iter::once(version_attr.to_owned())
                    .chain(r.deps.diff.ignore.iter().cloned())
                    .chain(input.globs("ignore"))
                    .collect(),
                array_match: r.deps.diff.array_match.clone(),
                ..Opts::default()
            };
            let delta = diff::json(
                &Value::Object(item_a.clone()),
                &Value::Object(item_b.clone()),
                &opts,
            );
            summary.push((
                "Diff".to_owned(),
                format!("{label_a} → {label_b}: {}", delta.stats),
            ));
            diff_view = Some(View::Diff {
                left: label_a.to_owned(),
                right: label_b.to_owned(),
                delta,
            });
        }

        let mut views = vec![
            View::summary(summary),
            View::Table {
                title: "Versions (V0 = latest)".to_owned(),
                cols: vec!["VERSION".to_owned(), String::new(), "ITEM".to_owned()],
                rows,
            },
        ];
        views.extend(diff_view);
        Ok(OpResult { views })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::ddb::fixture;
    use crate::ops::{Choice, Deps, Input, Runner, View};
    use crate::svc::ddb::{AttrType, FakeDdb, Item, TableInfo, contract};
    use serde_json::{Value, json};

    fn input(pairs: &[(&str, &str)]) -> Input {
        pairs.iter().copied().collect()
    }

    fn obj(v: Value) -> Item {
        match v {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    /// A `tenants` table holding `n` versions `V0..V{n-1}` of `acme`, with
    /// `rps = 100 + i`, served `page` items per response.
    fn paged_deps(n: usize, page: usize) -> Deps {
        let ddb = FakeDdb::new();
        ddb.create_table(contract::table_info("tenants"));
        for i in 0..n {
            ddb.put(
                "tenants",
                obj(json!({
                    "TenantId": "acme",
                    "Version": format!("V{i}"),
                    "Tier": "gold",
                    "limits": {"rps": 100 + i},
                })),
            );
        }
        fixture::deps(ddb.paged(page))
    }

    async fn try_run(deps: Deps, pairs: &[(&str, &str)]) -> anyhow::Result<Vec<View>> {
        Runner::new(deps)
            .execute(&ItemHistory, input(pairs))
            .await
            .map(|r| r.views)
            .map_err(|f| f.error)
    }

    async fn run(deps: Deps, pairs: &[(&str, &str)]) -> Vec<View> {
        try_run(deps, pairs).await.expect("history succeeds")
    }

    fn rows(view: &View) -> &[Vec<String>] {
        let View::Table { rows, .. } = view else {
            panic!("expected a table view");
        };
        rows
    }

    fn paths(view: &View) -> Vec<String> {
        let View::Diff { delta, .. } = view else {
            panic!("expected a diff view");
        };
        delta.changes.iter().map(|c| c.path.dotted()).collect()
    }

    #[tokio::test]
    async fn defaults_to_v1_versus_v0() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[("table", "tenants"), ("pk", "acme")],
        )
        .await;
        assert_eq!(views.len(), 3);
        assert_eq!(
            views[0],
            View::summary([
                ("Item", "tenants TenantId=acme"),
                ("Versions", "3"),
                ("Diff", "V1 → V0: ~2"),
            ])
        );
        let View::Table { title, cols, rows } = &views[1] else {
            panic!("second view is a table");
        };
        assert_eq!(title, "Versions (V0 = latest)");
        assert_eq!(cols, &["VERSION", "", "ITEM"]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][..2], ["V0", "latest"]);
        assert_eq!(rows[1][..2], ["V1", ""]);
        assert_eq!(rows[2][0], "V2");
        assert!(rows[0][2].contains(r#"Tier="platinum""#), "{}", rows[0][2]);
        assert!(!rows[0][2].contains("TenantId"), "{}", rows[0][2]);
        assert!(!rows[0][2].contains("Version"), "{}", rows[0][2]);

        assert_eq!(paths(&views[2]), ["Tier", "limits.rps"]);
    }

    #[tokio::test]
    async fn an_explicit_pair_diffs_just_those_versions() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[
                ("table", "tenants"),
                ("pk", "acme"),
                ("a", "V2"),
                ("b", "V1"),
            ],
        )
        .await;
        assert_eq!(paths(&views[2]), ["limits.rps"]);
        let View::Summary { fields } = &views[0] else {
            panic!("first view is a summary");
        };
        assert_eq!(fields.last().map(|(_, v)| v.as_str()), Some("V2 → V1: ~1"));
    }

    #[tokio::test]
    async fn an_unknown_version_lists_the_available_ones() {
        let err = try_run(
            fixture::deps(fixture::fake()),
            &[("table", "tenants"), ("pk", "acme"), ("a", "V9")],
        )
        .await
        .expect_err("unknown version");
        let text = format!("{err:#}");
        assert!(text.contains(r#"version "V9" not found"#), "{text}");
        assert!(text.contains("V0, V1, V2"), "{text}");

        let err = try_run(
            fixture::deps(fixture::fake()),
            &[("table", "tenants"), ("pk", "acme"), ("b", "V7")],
        )
        .await
        .expect_err("unknown version");
        assert!(
            format!("{err:#}").contains(r#"version "V7" not found"#),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_single_version_lists_without_diffing() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[("table", "tenants"), ("pk", "beta")],
        )
        .await;
        assert_eq!(views.len(), 2);
        assert_eq!(
            views[0],
            View::summary([
                ("Item", "tenants TenantId=beta"),
                ("Versions", "1"),
                ("Note", "only one version exists; nothing to diff"),
            ])
        );
        assert_eq!(rows(&views[1]).len(), 1);
        assert!(!views.iter().any(|v| matches!(v, View::Diff { .. })));
    }

    #[tokio::test]
    async fn no_items_is_an_error_naming_the_key() {
        let err = try_run(
            fixture::deps(fixture::fake()),
            &[("table", "tenants"), ("pk", "zzz")],
        )
        .await
        .expect_err("no items");
        assert!(
            format!("{err:#}").contains(r#"no items found for TenantId="zzz" in tenants"#),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn page_size_does_not_change_the_answer() {
        let pairs = [
            ("table", "tenants"),
            ("pk", "acme"),
            ("a", "V5"),
            ("b", "V0"),
            ("limit", "50"),
        ];
        let baseline = run(paged_deps(6, 100), &pairs).await;
        assert_eq!(rows(&baseline[1]).len(), 6);
        assert_eq!(paths(&baseline[2]), ["limits.rps"]);
        for page in [1, 2, 3, 5, 6, 7, 100] {
            assert_eq!(
                run(paged_deps(6, page), &pairs).await,
                baseline,
                "page {page}"
            );
        }
    }

    #[tokio::test]
    async fn limit_caps_the_total() {
        let views = run(
            paged_deps(50, 3),
            &[
                ("table", "tenants"),
                ("pk", "acme"),
                ("a", "V1"),
                ("b", "V0"),
                ("limit", "4"),
            ],
        )
        .await;
        let table = rows(&views[1]);
        assert_eq!(table.len(), 4);
        assert_eq!(table[0][0], "V0");
        assert_eq!(table[0][1], "latest");
    }

    #[test]
    fn limit_is_clamped_and_defaults_to_fifty() {
        assert_eq!(clamp_limit(0), 50);
        assert_eq!(clamp_limit(-7), 50);
        assert_eq!(clamp_limit(1), 1);
        assert_eq!(clamp_limit(10_000), 10_000);
        assert_eq!(clamp_limit(10_001), 10_000);
        assert_eq!(clamp_limit(i64::MAX), 10_000);
    }

    #[tokio::test]
    async fn a_non_positive_limit_means_fifty() {
        let views = run(
            paged_deps(60, 100),
            &[("table", "tenants"), ("pk", "acme"), ("limit", "0")],
        )
        .await;
        assert_eq!(rows(&views[1]).len(), 50);
    }

    fn rev_deps() -> Deps {
        let ddb = FakeDdb::new();
        ddb.create_table(TableInfo {
            name: "revs".into(),
            hash_key: "TenantId".into(),
            sort_key: Some("Rev".into()),
            attr_types: [
                ("TenantId".to_owned(), AttrType::S),
                ("Rev".to_owned(), AttrType::S),
            ]
            .into(),
            gsis: Vec::new(),
        });
        for (rev, rps) in [("V0", 20), ("V1", 10)] {
            ddb.put(
                "revs",
                obj(json!({"TenantId": "acme", "Rev": rev, "limits": {"rps": rps}})),
            );
        }
        fixture::deps(ddb)
    }

    #[tokio::test]
    async fn a_custom_version_attribute_is_used_and_ignored() {
        let views = run(
            rev_deps(),
            &[("table", "revs"), ("pk", "acme"), ("version-attr", "Rev")],
        )
        .await;
        let table = rows(&views[1]);
        assert_eq!(table[0][..2], ["V0", "latest"]);
        assert_eq!(table[1][0], "V1");
        assert!(!table[0][2].contains("Rev"), "{}", table[0][2]);
        assert_eq!(paths(&views[2]), ["limits.rps"]);
    }

    #[tokio::test]
    async fn unparseable_labels_sort_last_without_failing() {
        let ddb = fixture::fake();
        ddb.put(
            "tenants",
            obj(json!({"TenantId": "acme", "Version": "weird", "Tier": "x"})),
        );
        ddb.put(
            "tenants",
            obj(json!({"TenantId": "acme", "Version": "V+1", "Tier": "y"})),
        );
        let views = run(fixture::deps(ddb), &[("table", "tenants"), ("pk", "acme")]).await;
        let labels: Vec<&str> = rows(&views[1]).iter().map(|r| r[0].as_str()).collect();
        assert_eq!(labels, ["V0", "V1", "V2", "V+1", "weird"]);
        assert_eq!(rows(&views[1])[3][1], "");
    }

    #[tokio::test]
    async fn a_numeric_partition_key_is_coerced_to_its_declared_type() {
        let ddb = FakeDdb::new();
        ddb.create_table(TableInfo {
            name: "nums".into(),
            hash_key: "Id".into(),
            sort_key: Some("Version".into()),
            attr_types: [
                ("Id".to_owned(), AttrType::N),
                ("Version".to_owned(), AttrType::S),
            ]
            .into(),
            gsis: Vec::new(),
        });
        for v in ["V0", "V1"] {
            ddb.put("nums", obj(json!({"Id": 42, "Version": v, "n": v})));
        }
        let views = run(fixture::deps(ddb), &[("table", "nums"), ("pk", "42")]).await;
        assert_eq!(rows(&views[1]).len(), 2);
    }

    #[tokio::test]
    async fn without_ddb_the_run_fails_clearly() {
        let err = try_run(Deps::default(), &[("table", "tenants"), ("pk", "acme")])
            .await
            .expect_err("no ddb");
        assert!(
            format!("{err:#}").contains("DynamoDB is not configured"),
            "{err:#}"
        );
    }

    async fn suggest(d: &Deps, pairs: &[(&str, &str)]) -> Vec<Choice> {
        suggest_item_versions()(d.clone(), input(pairs))
            .await
            .expect("suggestions")
    }

    #[tokio::test]
    async fn item_version_suggestions_label_the_latest() {
        let d = fixture::deps(fixture::fake());
        let choices = suggest(&d, &[("table", "tenants"), ("pk", "acme")]).await;
        let values: Vec<&str> = choices.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, ["V0", "V1", "V2"]);
        assert_eq!(choices[0].label.as_deref(), Some("V0 — latest"));
        assert_eq!(choices[1].label, None);

        assert!(suggest(&d, &[("table", "tenants")]).await.is_empty());
        assert!(suggest(&d, &[("pk", "acme")]).await.is_empty());
        assert!(
            suggest(&Deps::default(), &[("table", "tenants"), ("pk", "acme")])
                .await
                .is_empty()
        );
    }

    #[test]
    fn meta_and_params_match_the_contract() {
        let meta = ItemHistory.meta();
        assert_eq!(meta.id, "ddb.item-history");
        assert_eq!(meta.category, "DynamoDB");
        let names: Vec<&str> = ItemHistory.params().iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            ["table", "pk", "a", "b", "version-attr", "limit", "ignore"]
        );
    }
}
