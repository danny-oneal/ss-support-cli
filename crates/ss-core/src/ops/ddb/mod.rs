//! The DynamoDB operations and the helpers they share: the table and index
//! suggestion chain, key parsing, and version-label handling.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, bail};
use serde_json::{Number, Value};

use super::{Choice, Deps, Input, SuggestFn};
use crate::jsonutil;
use crate::svc::ddb::{AttrType, DdbPort, Item, Key, TableInfo};

#[cfg(test)]
pub(crate) mod fixture;
mod gsi_consistency;
mod item_compare;
mod item_history;

pub use gsi_consistency::GsiConsistency;
pub use item_compare::ItemCompare;
pub use item_history::ItemHistory;

/// The DynamoDB port of `deps`, or an error when the invocation has none.
///
/// # Errors
///
/// Returns an error when `deps` carries no DynamoDB port.
pub(crate) fn port(deps: &Deps) -> anyhow::Result<Arc<dyn DdbPort>> {
    deps.ddb
        .clone()
        .ok_or_else(|| anyhow!("DynamoDB is not configured"))
}

/// Suggests every table name.
pub(crate) fn suggest_tables() -> SuggestFn {
    Arc::new(|deps: Deps, _input: Input| {
        Box::pin(async move {
            let Some(ddb) = deps.ddb else {
                return Ok(Vec::new());
            };
            Ok(ddb
                .list_tables()
                .await?
                .into_iter()
                .map(|name| Choice {
                    value: name,
                    label: None,
                })
                .collect())
        })
    })
}

/// Suggests the global secondary indexes of the typed `table`.
pub(crate) fn suggest_indexes() -> SuggestFn {
    Arc::new(|deps: Deps, input: Input| {
        Box::pin(async move {
            let table = input.str("table");
            let Some(ddb) = deps.ddb.filter(|_| !table.is_empty()) else {
                return Ok(Vec::new());
            };
            let info = ddb.describe_table(table).await?;
            Ok(info
                .gsis
                .into_iter()
                .map(|gsi| {
                    let mut label = format!("{} — hash {}", gsi.name, gsi.hash_key);
                    if let Some(sort) = &gsi.sort_key {
                        label.push_str(&format!(", sort {sort}"));
                    }
                    if let Some(projection) = gsi.projection {
                        label.push_str(&format!(" ({projection})"));
                    }
                    Choice {
                        value: gsi.name,
                        label: Some(label),
                    }
                })
                .collect())
        })
    })
}

/// Parses a key given as a JSON object or as `name=value` pairs separated by
/// commas, converting each value to the type `info` declares for its
/// attribute. Pairs split on the first `=` and trim both sides.
///
/// # Errors
///
/// Returns an error when `spec` is blank, is not valid JSON though it starts
/// with `{`, or has a part that is not `name=value`.
pub(crate) fn parse_key(spec: &str, info: &TableInfo) -> anyhow::Result<Key> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty key");
    }
    let mut key = if spec.starts_with('{') {
        let value = jsonutil::parse(spec.as_bytes()).context("key is not valid JSON")?;
        let Value::Object(map) = value else {
            bail!("key is not a JSON object");
        };
        map
    } else {
        let mut map = Key::new();
        for part in spec.split(',') {
            let (name, value) = part
                .split_once('=')
                .map(|(n, v)| (n.trim(), v.trim()))
                .filter(|(n, _)| !n.is_empty())
                .ok_or_else(|| anyhow!("key part {:?} is not name=value", part.trim()))?;
            map.insert(name.to_owned(), Value::String(value.to_owned()));
        }
        map
    };
    for (name, value) in &mut key {
        let attr_type = info.attr_types.get(name).copied();
        *value = coerce(std::mem::take(value), attr_type);
    }
    Ok(key)
}

/// Converts `val` to the declared attribute type: a numeric string becomes a
/// number for `N`, and a number becomes its text for `S`. Anything else, and
/// every value of an attribute without a declared type, is returned as is.
pub(crate) fn coerce(val: Value, attr_type: Option<AttrType>) -> Value {
    match (attr_type, val) {
        (Some(AttrType::N), Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map_or(Value::String(s), Value::Number),
        (Some(AttrType::S), Value::Number(n)) => Value::String(jsonutil::fmt_number(&n)),
        (_, other) => other,
    }
}

/// The number in a version label: `V` or `v` followed by one or more ASCII
/// digits that fit a `u32`, ignoring surrounding whitespace.
pub(crate) fn version_num(label: &str) -> Option<u32> {
    let digits = label.trim().strip_prefix(['V', 'v'])?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The version label of `item`: the string at `attr` as is, a number `n` as
/// `V{n}`, and an empty string for any other value or when `attr` is missing.
pub(crate) fn item_version(item: &Item, attr: &str) -> String {
    match item.get(attr) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => format!("V{}", jsonutil::fmt_number(n)),
        _ => String::new(),
    }
}

/// Sorts `items` by their version label at `attr`, keeping the order of equal
/// labels: labels [`version_num`] understands come first in ascending numeric
/// order, then the rest alphabetically.
pub(crate) fn sort_by_version(items: &mut [Item], attr: &str) {
    items.sort_by_cached_key(|item| {
        let label = item_version(item, attr);
        match version_num(&label) {
            Some(n) => (false, n, String::new()),
            None => (true, 0, label),
        }
    });
}

/// A one-line summary of `item`: `name=value` pairs in name order, values as
/// compact JSON, leaving out the names in `skip`. Longer than `max`
/// characters, it is cut so that it ends in `…` and has exactly `max`
/// characters.
pub(crate) fn compact_preview(item: &Item, skip: &BTreeSet<String>, max: usize) -> String {
    let mut entries: Vec<(&String, &Value)> = item
        .iter()
        .filter(|(name, _)| !skip.contains(*name))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let line = entries
        .into_iter()
        .map(|(name, value)| format!("{name}={}", jsonutil::compact(value)))
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let keep = max.saturating_sub(1);
    let mut cut: String = line.chars().take(keep).collect();
    if max > 0 {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::svc::ddb::{AttrType, IndexInfo, Item, Projection, TableInfo};

    fn item(v: Value) -> Item {
        match v {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    fn typed_info() -> TableInfo {
        TableInfo {
            name: "t".into(),
            hash_key: "Id".into(),
            sort_key: Some("Sort".into()),
            attr_types: [
                ("Id".to_owned(), AttrType::N),
                ("Sort".to_owned(), AttrType::S),
            ]
            .into(),
            gsis: vec![IndexInfo {
                name: "ById".into(),
                hash_key: "Id".into(),
                ..Default::default()
            }],
        }
    }

    fn tenants_info() -> TableInfo {
        crate::svc::ddb::contract::table_info(fixture::TABLE)
    }

    #[test]
    fn version_num_accepts_only_v_and_digits() {
        for (label, want) in [
            ("V0", Some(0)),
            ("v12", Some(12)),
            (" V3 ", Some(3)),
            ("V", None),
            ("V+1", None),
            ("v-3", None),
            ("V1x", None),
            ("weird", None),
            ("V99999999999", None),
            ("", None),
        ] {
            assert_eq!(version_num(label), want, "{label:?}");
        }
    }

    #[test]
    fn sort_by_version_puts_latest_first_and_strangers_last() {
        let mut items: Vec<Item> = ["V10", "V2", "weird", "V0", ""]
            .into_iter()
            .map(|v| item(json!({"Version": v})))
            .collect();
        sort_by_version(&mut items, "Version");
        let order: Vec<String> = items.iter().map(|i| item_version(i, "Version")).collect();
        assert_eq!(order, ["V0", "V2", "V10", "", "weird"]);
    }

    #[test]
    fn sort_by_version_is_stable_for_equal_numbers() {
        let mut items: Vec<Item> = [("v1", 1), ("V1", 2), ("V0", 3)]
            .into_iter()
            .map(|(v, n)| item(json!({"Version": v, "n": n})))
            .collect();
        sort_by_version(&mut items, "Version");
        let order: Vec<i64> = items.iter().filter_map(|i| i["n"].as_i64()).collect();
        assert_eq!(order, [3, 1, 2]);
    }

    #[test]
    fn item_version_reads_strings_and_numbers() {
        assert_eq!(
            item_version(&item(json!({"Version": "V3"})), "Version"),
            "V3"
        );
        assert_eq!(item_version(&item(json!({"Version": 0})), "Version"), "V0");
        assert_eq!(
            item_version(&item(json!({"Version": 2.5})), "Version"),
            "V2.5"
        );
        assert_eq!(item_version(&item(json!({"Version": true})), "Version"), "");
        assert_eq!(item_version(&item(json!({"Other": "V1"})), "Version"), "");
    }

    #[test]
    fn parse_key_accepts_json_and_pairs_and_coerces_to_the_declared_type() -> anyhow::Result<()> {
        let info = typed_info();
        let want = item(json!({"Id": 42.0, "Sort": "7"}));
        assert_eq!(parse_key("Id=42,Sort=7", &info)?, want);
        assert_eq!(parse_key(r#"{"Id":"42","Sort":7}"#, &info)?, want);
        assert_eq!(parse_key(" Id = 42 , Sort = 7 ", &info)?, want);

        let info = tenants_info();
        assert_eq!(
            parse_key("TenantId=acme,Version=V0", &info)?,
            parse_key(r#"{"TenantId":"acme","Version":"V0"}"#, &info)?
        );
        Ok(())
    }

    #[test]
    fn parse_key_splits_on_the_first_equals_only() -> anyhow::Result<()> {
        let key = parse_key("Name=a=b", &TableInfo::default())?;
        assert_eq!(key, item(json!({"Name": "a=b"})));
        Ok(())
    }

    #[test]
    fn parse_key_rejects_blank_malformed_and_non_object_specs() {
        let info = tenants_info();
        for blank in ["", "   "] {
            let err = parse_key(blank, &info).expect_err("blank");
            assert_eq!(err.to_string(), "empty key");
        }
        let err = parse_key("nonsense", &info).expect_err("no equals");
        assert!(
            err.to_string()
                .contains(r#"key part "nonsense" is not name=value"#),
            "{err}"
        );
        let err = parse_key("{bad", &info).expect_err("bad json");
        assert!(
            err.to_string().starts_with("key is not valid JSON"),
            "{err}"
        );
        assert!(parse_key("[1]", &info).is_err());
        assert!(parse_key("=x", &info).is_err());
    }

    #[test]
    fn coerce_follows_the_declared_type() {
        assert_eq!(coerce(json!("7"), None), json!("7"));
        assert_eq!(coerce(json!(" 7.5 "), Some(AttrType::N)), json!(7.5));
        assert_eq!(coerce(json!("abc"), Some(AttrType::N)), json!("abc"));
        assert_eq!(coerce(json!("inf"), Some(AttrType::N)), json!("inf"));
        assert_eq!(coerce(json!(7), Some(AttrType::N)), json!(7));
        assert_eq!(coerce(json!(7), Some(AttrType::S)), json!("7"));
        assert_eq!(coerce(json!(2.5), Some(AttrType::S)), json!("2.5"));
        assert_eq!(coerce(json!(true), Some(AttrType::S)), json!(true));
        assert_eq!(coerce(json!(7), Some(AttrType::B)), json!(7));
    }

    #[test]
    fn compact_preview_sorts_skips_and_truncates_on_a_character_boundary() {
        let skip: BTreeSet<String> = ["TenantId".to_owned()].into();
        let it = item(json!({"b": 1, "a": "x", "TenantId": "t"}));
        assert_eq!(compact_preview(&it, &skip, 100), r#"a="x" b=1"#);
        assert_eq!(compact_preview(&it, &skip, 9), r#"a="x" b=1"#);
        assert_eq!(compact_preview(&it, &skip, 8), r#"a="x" b…"#);

        let wide = item(json!({"a": "é", "b": 1}));
        for max in 0..12 {
            let out = compact_preview(&wide, &BTreeSet::new(), max);
            assert!(out.chars().count() <= max, "{max}: {out:?}");
            if max > 0 && max < 9 {
                assert!(out.ends_with('…'), "{max}: {out:?}");
            }
        }
        assert_eq!(compact_preview(&wide, &BTreeSet::new(), 6), r#"a="é"…"#);
        assert_eq!(compact_preview(&Item::new(), &skip, 5), "");
    }

    fn call(
        f: SuggestFn,
        d: &Deps,
        pairs: &[(&str, &str)],
    ) -> crate::ops::BoxFuture<anyhow::Result<Vec<Choice>>> {
        f(d.clone(), pairs.iter().copied().collect())
    }

    #[tokio::test]
    async fn suggestions_list_tables_and_label_indexes() -> anyhow::Result<()> {
        let d = fixture::deps(fixture::fake());
        assert_eq!(
            call(suggest_tables(), &d, &[]).await?,
            [Choice {
                value: "tenants".into(),
                label: None
            }]
        );
        assert_eq!(
            call(suggest_indexes(), &d, &[("table", "tenants")]).await?,
            [Choice {
                value: "TierIndex".into(),
                label: Some("TierIndex — hash Tier (ALL)".into())
            }]
        );

        let none = Deps::default();
        assert!(call(suggest_tables(), &none, &[]).await?.is_empty());
        assert!(
            call(suggest_indexes(), &none, &[("table", "tenants")])
                .await?
                .is_empty()
        );
        assert!(call(suggest_indexes(), &d, &[]).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn index_labels_include_the_sort_key_and_omit_an_unknown_projection() -> anyhow::Result<()>
    {
        let ddb = crate::svc::ddb::FakeDdb::new();
        ddb.create_table(TableInfo {
            name: "t".into(),
            hash_key: "Id".into(),
            gsis: vec![
                IndexInfo {
                    name: "A".into(),
                    hash_key: "H".into(),
                    sort_key: Some("S".into()),
                    projection: Some(Projection::KeysOnly),
                },
                IndexInfo {
                    name: "B".into(),
                    hash_key: "H".into(),
                    sort_key: None,
                    projection: None,
                },
            ],
            ..Default::default()
        });
        let d = fixture::deps(ddb);
        let labels: Vec<_> = call(suggest_indexes(), &d, &[("table", "t")])
            .await?
            .into_iter()
            .filter_map(|c| c.label)
            .collect();
        assert_eq!(labels, ["A — hash H, sort S (KEYS_ONLY)", "B — hash H"]);
        Ok(())
    }

    #[tokio::test]
    async fn suggestion_errors_propagate() {
        let d = fixture::deps(fixture::fake());
        assert!(
            call(suggest_indexes(), &d, &[("table", "missing")])
                .await
                .is_err()
        );
    }

    #[test]
    fn port_names_the_missing_configuration() {
        let err = port(&Deps::default()).err().expect("no port configured");
        assert!(
            err.to_string().contains("DynamoDB is not configured"),
            "{err}"
        );
        assert!(port(&fixture::deps(fixture::fake())).is_ok());
    }
}
