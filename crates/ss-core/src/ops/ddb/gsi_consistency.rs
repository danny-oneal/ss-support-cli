//! `ddb.gsi-consistency`: checks one item against its entry in a global
//! secondary index.

use async_trait::async_trait;
use serde_json::Value;

use super::{parse_key, port, suggest_indexes, suggest_tables};
use crate::diff::{self, Opts};
use crate::guard::Class;
use crate::ops::{Input, Meta, OpResult, Operation, Param, Runner, View};
use crate::svc::SvcError;
use crate::svc::ddb::{DdbPort, IndexInfo, Item, QuerySpec, TableInfo, key_string};

/// The most index entries one run reads under the item's index hash value
/// before it gives up with an inconclusive answer.
pub(crate) const MAX_CANDIDATES: usize = 10_000;

/// Compares an item with its entry in a global secondary index, on the
/// attributes the index projects.
#[derive(Clone, Copy, Debug, Default)]
pub struct GsiConsistency;

/// How the search for an item's index entry ended.
#[derive(Debug)]
enum Scan {
    /// The entry whose base-table key is the item's.
    Found(Item),
    /// The index holds this many entries under the hash value, none of them
    /// the item's.
    Missing(usize),
    /// [`MAX_CANDIDATES`] entries were read without finding the item's.
    Capped,
}

/// The base-table key attributes of `item` rendered by [`key_string`], so two
/// spellings of one number compare equal.
fn base_key_string(info: &TableInfo, item: &Item) -> String {
    let key: Item = std::iter::once(info.hash_key.as_str())
        .chain(info.sort_key.as_deref())
        .filter_map(|name| item.get(name).map(|v| (name.to_owned(), v.clone())))
        .collect();
    key_string(&key)
}

/// Reads `index` of `table` under `hash_value` page by page until the entry
/// whose base key renders as `target` turns up, the partition ends, or
/// [`MAX_CANDIDATES`] entries have been read.
async fn find_entry(
    ddb: &dyn DdbPort,
    info: &TableInfo,
    index: &IndexInfo,
    hash_value: Value,
    target: &str,
) -> Result<Scan, SvcError> {
    let mut spec = QuerySpec {
        key_condition: "#h = :h".to_owned(),
        names: [("#h".to_owned(), index.hash_key.clone())].into(),
        values: [(":h".to_owned(), hash_value)].into(),
        ..QuerySpec::default()
    };
    let mut seen = 0usize;
    loop {
        let page = ddb.query_index(&info.name, &index.name, &spec).await?;
        for entry in page.items {
            seen += 1;
            if base_key_string(info, &entry) == target {
                return Ok(Scan::Found(entry));
            }
            if seen >= MAX_CANDIDATES {
                return Ok(Scan::Capped);
            }
        }
        match page.next {
            Some(next) => spec.exclusive_start = Some(next),
            None => return Ok(Scan::Missing(seen)),
        }
    }
}

/// A result of one summary view: `fields` followed by the `Result` field.
fn done(mut fields: Vec<(String, String)>, result: String) -> anyhow::Result<OpResult> {
    fields.push(("Result".to_owned(), result));
    Ok(OpResult {
        views: vec![View::summary(fields)],
    })
}

#[async_trait]
impl Operation for GsiConsistency {
    fn meta(&self) -> Meta {
        Meta {
            id: "ddb.gsi-consistency",
            name: "GSI consistency check",
            summary: "Diff an item against its projection in a GSI",
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
            Param::new("index")
                .label("GSI")
                .required(true)
                .suggest(suggest_indexes()),
            Param::new("key").label("Item key").required(true).help(
                r#"base-table key: {"TenantId":"acme","Version":"V0"} or TenantId=acme,Version=V0"#,
            ),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let ddb = port(&r.deps)?;
        let (table, index_name) = (input.str("table"), input.str("index"));

        let info = r
            .step("describe table", |_| async {
                Ok(ddb.describe_table(table).await?)
            })
            .await?;
        let Some(index) = info.index(index_name) else {
            anyhow::bail!("table {table} has no GSI {index_name:?}");
        };
        let key = parse_key(input.str("key"), &info).map_err(|e| e.context("key"))?;

        let base = r
            .step("fetch base item", |_| async {
                Ok(ddb.get_item(table, &key).await?)
            })
            .await?;

        let mut summary = vec![
            ("Item".to_owned(), format!("{table} {}", key_string(&key))),
            (
                "GSI".to_owned(),
                format!(
                    "{index_name} (projection: {})",
                    index
                        .projection
                        .map_or_else(|| "unknown".to_owned(), |p| p.to_string())
                ),
            ),
        ];

        let Some(hash_value) = base.get(&index.hash_key).cloned() else {
            return done(
                summary,
                format!(
                    "item has no \"{}\" attribute — not projected into this GSI",
                    index.hash_key
                ),
            );
        };
        let hash_label = key_string(&Item::from_iter([(
            index.hash_key.clone(),
            hash_value.clone(),
        )]));
        let target = base_key_string(&info, &base);

        let scan = r
            .step("query GSI projection", |_| async {
                Ok(find_entry(&*ddb, &info, index, hash_value, &target).await?)
            })
            .await?;
        let entry = match scan {
            Scan::Found(entry) => entry,
            Scan::Missing(n) => {
                return done(
                    summary,
                    format!(
                        "MISSING — no GSI entry matches the base key ({n} candidates under {hash_label})"
                    ),
                );
            }
            Scan::Capped => {
                return done(
                    summary,
                    format!(
                        "INCONCLUSIVE — no matching GSI entry among the first {MAX_CANDIDATES} candidates under {hash_label}; the entry may exist further on"
                    ),
                );
            }
        };

        let projected: Item = base
            .iter()
            .filter(|(name, _)| entry.contains_key(*name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let opts = Opts {
            ignore: r.deps.diff.ignore.clone(),
            array_match: r.deps.diff.array_match.clone(),
            ..Opts::default()
        };
        summary.push((
            "Compared attributes".to_owned(),
            format!("{} (those present in the projection)", entry.len()),
        ));
        let delta = diff::json(&Value::Object(projected), &Value::Object(entry), &opts);
        if delta.stats.total() == 0 {
            return done(
                summary,
                "consistent — projection matches the base item".to_owned(),
            );
        }
        summary.push((
            "Result".to_owned(),
            format!(
                "DRIFT — projection disagrees with the base item ({})",
                delta.stats
            ),
        ));
        Ok(OpResult {
            views: vec![
                View::summary(summary),
                View::Diff {
                    left: "base table".to_owned(),
                    right: format!("GSI {index_name}"),
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
    use crate::svc::ddb::{AttrType, FakeDdb, IndexInfo, Item, Projection, TableInfo, contract};
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

    async fn run(ddb: FakeDdb, pairs: &[(&str, &str)]) -> Vec<View> {
        Runner::new(fixture::deps(ddb))
            .execute(&GsiConsistency, input(pairs))
            .await
            .expect("check succeeds")
            .views
    }

    async fn fail(deps: Deps, pairs: &[(&str, &str)]) -> RunFailure {
        Runner::new(deps)
            .execute(&GsiConsistency, input(pairs))
            .await
            .expect_err("run fails")
    }

    fn fields(view: &View) -> Vec<(&str, &str)> {
        let View::Summary { fields } = view else {
            panic!("expected a summary view");
        };
        fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn result(view: &View) -> String {
        fields(view)
            .into_iter()
            .find(|(k, _)| *k == "Result")
            .map(|(_, v)| v.to_owned())
            .expect("summary has a Result")
    }

    fn args(key: &str) -> [(&str, &str); 3] {
        [("table", "tenants"), ("index", "TierIndex"), ("key", key)]
    }

    #[tokio::test]
    async fn a_stale_projection_is_reported_as_drift() {
        let views = run(fixture::fake(), &args("TenantId=acme,Version=V0")).await;
        assert_eq!(views.len(), 2);
        let f = fields(&views[0]);
        assert_eq!(
            f[..3],
            [
                ("Item", "tenants TenantId=acme|Version=V0"),
                ("GSI", "TierIndex (projection: ALL)"),
                ("Compared attributes", "5 (those present in the projection)"),
            ]
        );
        assert!(
            result(&views[0]).starts_with("DRIFT — projection disagrees with the base item ("),
            "{f:?}"
        );
        let View::Diff { left, right, delta } = &views[1] else {
            panic!("second view is a diff");
        };
        assert_eq!(left, "base table");
        assert_eq!(right, "GSI TierIndex");
        let paths: Vec<String> = delta.changes.iter().map(|c| c.path.dotted()).collect();
        assert_eq!(paths, ["limits.rps"]);
    }

    #[tokio::test]
    async fn a_matching_projection_is_consistent_and_has_no_diff_view() {
        let views = run(fixture::fake(), &args("TenantId=beta,Version=V0")).await;
        assert_eq!(views.len(), 1);
        assert_eq!(
            result(&views[0]),
            "consistent — projection matches the base item"
        );
    }

    #[tokio::test]
    async fn a_missing_index_entry_is_reported_as_missing() {
        let ddb = fixture::fake();
        ddb.remove_index_entry(
            "tenants",
            "TierIndex",
            &obj(json!({"TenantId": "beta", "Version": "V0"})),
        );
        let views = run(ddb, &args("TenantId=beta,Version=V0")).await;
        assert_eq!(views.len(), 1);
        let text = result(&views[0]);
        assert!(
            text.starts_with("MISSING — no GSI entry matches the base key ("),
            "{text}"
        );
        assert!(text.contains("candidates under Tier=bronze)"), "{text}");
    }

    #[tokio::test]
    async fn an_item_without_the_index_key_is_not_projected() {
        let ddb = fixture::fake();
        ddb.put(
            "tenants",
            obj(json!({"TenantId": "acme", "Version": "V3", "limits": {"rps": 1}})),
        );
        let views = run(ddb, &args("TenantId=acme,Version=V3")).await;
        assert_eq!(views.len(), 1);
        assert_eq!(
            fields(&views[0]),
            [
                ("Item", "tenants TenantId=acme|Version=V3"),
                ("GSI", "TierIndex (projection: ALL)"),
                (
                    "Result",
                    "item has no \"Tier\" attribute — not projected into this GSI"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn keys_only_projections_compare_only_what_they_carry() {
        let ddb = FakeDdb::new();
        let mut info = contract::table_info("keyed");
        info.gsis = vec![IndexInfo {
            name: "TierIndex".into(),
            hash_key: "Tier".into(),
            sort_key: None,
            projection: Some(Projection::KeysOnly),
        }];
        ddb.create_table(info);
        ddb.put(
            "keyed",
            obj(json!({
                "TenantId": "acme", "Version": "V0", "Tier": "gold",
                "limits": {"rps": 5}, "extra": [1, 2, 3],
            })),
        );
        let views = run(
            ddb,
            &[
                ("table", "keyed"),
                ("index", "TierIndex"),
                ("key", "TenantId=acme,Version=V0"),
            ],
        )
        .await;
        assert_eq!(views.len(), 1);
        let f = fields(&views[0]);
        assert!(
            f.contains(&("GSI", "TierIndex (projection: KEYS_ONLY)")),
            "{f:?}"
        );
        assert!(
            f.contains(&("Compared attributes", "3 (those present in the projection)")),
            "{f:?}"
        );
        assert_eq!(
            result(&views[0]),
            "consistent — projection matches the base item"
        );
    }

    #[tokio::test]
    async fn an_unknown_index_is_reported() {
        let f = fail(
            fixture::deps(fixture::fake()),
            &[
                ("table", "tenants"),
                ("index", "Nope"),
                ("key", "TenantId=acme,Version=V0"),
            ],
        )
        .await;
        assert_eq!(
            format!("{:#}", f.error),
            r#"table tenants has no GSI "Nope""#
        );
    }

    #[tokio::test]
    async fn a_missing_base_item_fails_as_not_found() {
        let f = fail(
            fixture::deps(fixture::fake()),
            &args("TenantId=acme,Version=V9"),
        )
        .await;
        assert!(
            find_cause::<SvcError>(&f.error).is_some_and(SvcError::is_not_found),
            "{:#}",
            f.error
        );
        assert!(
            format!("{:#}", f.error).starts_with("fetch base item: "),
            "{:#}",
            f.error
        );
    }

    #[tokio::test]
    async fn a_malformed_key_fails_before_any_fetch() {
        let f = fail(fixture::deps(fixture::fake()), &args("nonsense")).await;
        let text = format!("{:#}", f.error);
        assert!(text.contains("is not name=value"), "{text}");
        assert!(!text.contains("fetch base item"), "{text}");
    }

    #[tokio::test]
    async fn the_entry_is_found_on_a_later_page() {
        let ddb = FakeDdb::new();
        ddb.create_table(contract::table_info("many"));
        for n in 0..20 {
            ddb.put(
                "many",
                obj(json!({
                    "TenantId": format!("t{n:02}"), "Version": "V0", "Tier": "gold",
                    "limits": {"rps": n},
                })),
            );
        }
        let views = run(
            ddb.paged(3),
            &[
                ("table", "many"),
                ("index", "TierIndex"),
                ("key", "TenantId=t19,Version=V0"),
            ],
        )
        .await;
        assert_eq!(views.len(), 1);
        assert_eq!(
            result(&views[0]),
            "consistent — projection matches the base item"
        );
    }

    #[tokio::test]
    async fn the_candidate_count_is_reported_when_the_entry_is_missing() {
        let ddb = FakeDdb::new();
        ddb.create_table(contract::table_info("many"));
        for n in 0..7 {
            ddb.put(
                "many",
                obj(json!({"TenantId": format!("t{n}"), "Version": "V0", "Tier": "gold"})),
            );
        }
        ddb.remove_index_entry(
            "many",
            "TierIndex",
            &obj(json!({"TenantId": "t3", "Version": "V0"})),
        );
        let views = run(
            ddb.paged(2),
            &[
                ("table", "many"),
                ("index", "TierIndex"),
                ("key", "TenantId=t3,Version=V0"),
            ],
        )
        .await;
        assert_eq!(
            result(&views[0]),
            "MISSING — no GSI entry matches the base key (6 candidates under Tier=gold)"
        );
    }

    #[tokio::test]
    async fn scan_stops_after_the_candidate_cap() {
        let ddb = FakeDdb::new();
        ddb.create_table(contract::table_info("hot"));
        for n in 0..=MAX_CANDIDATES {
            ddb.put(
                "hot",
                obj(json!({"TenantId": format!("t{n:05}"), "Version": "V0", "Tier": "gold"})),
            );
        }
        ddb.remove_index_entry(
            "hot",
            "TierIndex",
            &obj(json!({"TenantId": format!("t{MAX_CANDIDATES:05}"), "Version": "V0"})),
        );
        let key = format!("TenantId=t{MAX_CANDIDATES:05},Version=V0");
        let views = run(
            ddb.paged(1000),
            &[("table", "hot"), ("index", "TierIndex"), ("key", &key)],
        )
        .await;
        assert_eq!(views.len(), 1);
        assert_eq!(
            result(&views[0]),
            "INCONCLUSIVE — no matching GSI entry among the first 10000 candidates \
             under Tier=gold; the entry may exist further on"
        );
    }

    #[tokio::test]
    async fn numeric_base_keys_match_however_the_number_is_written() {
        let ddb = FakeDdb::new();
        ddb.create_table(TableInfo {
            name: "nums".into(),
            hash_key: "Id".into(),
            attr_types: [("Id".to_owned(), AttrType::N)].into(),
            gsis: vec![IndexInfo {
                name: "ByGroup".into(),
                hash_key: "Group".into(),
                sort_key: None,
                projection: Some(Projection::All),
            }],
            ..TableInfo::default()
        });
        ddb.put("nums", obj(json!({"Id": 42, "Group": "g"})));
        ddb.set_index_entry("nums", "ByGroup", obj(json!({"Id": 42.0, "Group": "g"})));
        let views = run(
            ddb,
            &[("table", "nums"), ("index", "ByGroup"), ("key", "Id=42")],
        )
        .await;
        assert_eq!(
            result(&views[0]),
            "consistent — projection matches the base item"
        );
    }

    #[tokio::test]
    async fn without_ddb_the_run_fails_clearly() {
        let f = fail(Deps::default(), &args("TenantId=acme,Version=V0")).await;
        assert!(
            format!("{:#}", f.error).contains("DynamoDB is not configured"),
            "{:#}",
            f.error
        );
    }
}
