//! `ddb.item-edit`: sets and removes attributes of one DynamoDB item.

use async_trait::async_trait;
use serde_json::Value;

use super::{parse_key_param, port, suggest_tables};
use crate::diff::{self, Opts};
use crate::guard::Class;
use crate::jsonutil;
use crate::ops::{Input, Meta, OpResult, Operation, Param, ParamType, Runner, RunnerError, View};
use crate::svc::ddb::{Item, ItemChange, key_string};

/// The smallest whole number whose neighbour a double cannot tell from it:
/// 2^53. The preview and the comparison handle numbers as doubles, so a
/// whole number from here on could not be shown as it would be written.
const EXACT_LIMIT: f64 = 9_007_199_254_740_992.0;

/// Edits one DynamoDB item: named top-level attributes are replaced or
/// deleted, and every other attribute is left alone.
///
/// DynamoDB keeps no history, so the operation is destructive: it shows the
/// difference between the stored item and the edited one, and the operator
/// types the table name to proceed. The write is made only while every
/// attribute it touches still holds what was shown.
#[derive(Clone, Copy, Debug, Default)]
pub struct ItemEdit;

#[async_trait]
impl Operation for ItemEdit {
    fn meta(&self) -> Meta {
        Meta {
            id: "ddb.item-edit",
            name: "Edit item",
            summary: "Set and remove attributes of one DynamoDB item",
            category: "DynamoDB",
            class: Class::Destructive,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::new("table")
                .label("Table")
                .required(true)
                .suggest(suggest_tables()),
            Param::new("key").label("Key").required(true).help(
                r#"item key: {"TenantId":"acme","Version":"V0"} or TenantId=acme,Version=V0"#,
            ),
            Param::new("set").label("Set").ty(ParamType::Json).help(
                r#"attributes to write, as a JSON object; each replaces the whole stored value: {"limits":{"rps":250}}"#,
            ),
            Param::new("remove")
                .label("Remove")
                .help(r#"attributes to delete: note,legacyFlag or ["note","legacyFlag"]"#),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let ddb = port(&r.deps)?;
        let table = input.str("table");
        let (set, remove) = requested(input)?;

        let info = r
            .step("describe the table", |_| async {
                Ok(ddb.describe_table(table).await?)
            })
            .await?;
        let key = parse_key_param("key", input.str("key"), &info)?;
        let key_attribute = |param: &str, name: &String| {
            invalid(
                param,
                name,
                format!("{name} is a key attribute; the key of an item cannot be edited"),
            )
        };
        if let Some(name) = set.keys().find(|name| key.contains_key(name.as_str())) {
            return Err(key_attribute("set", name).into());
        }
        if let Some(name) = remove.iter().find(|name| key.contains_key(name.as_str())) {
            return Err(key_attribute("remove", name).into());
        }
        let shown_key = key_string(&key);

        let stored = r
            .step("fetch the item", |_| async {
                Ok(ddb.get_item(table, &key).await?)
            })
            .await?;
        let mut edited = stored.clone();
        edited.extend(set.clone());
        for name in &remove {
            edited.remove(name);
        }
        // Compared with numbers by value, as the item is read back.
        let (before, after) = (comparable(&stored), comparable(&edited));
        if before == after {
            return Ok(OpResult {
                views: vec![View::summary([
                    ("Table", table),
                    ("Key", shown_key.as_str()),
                    ("Result", "the item already holds this; nothing was changed"),
                ])],
            });
        }
        // What the diff defaults ignore, or match out of order, is written
        // all the same, so the preview uses none of them.
        let delta = diff::json(&before, &after, &Opts::default());
        let changes = delta.stats.to_string();
        let names = |names: Vec<&str>| match names.as_slice() {
            [] => "—".to_owned(),
            names => names.join(", "),
        };
        let setting = names(set.keys().map(String::as_str).collect());
        let removing = names(remove.iter().map(String::as_str).collect());

        r.preview([
            View::summary([
                ("Table", table),
                ("Key", shown_key.as_str()),
                ("Set", setting.as_str()),
                ("Remove", removing.as_str()),
                ("Changes", changes.as_str()),
            ]),
            View::Diff {
                left: "stored".to_owned(),
                right: "edited".to_owned(),
                delta: delta.clone(),
            },
        ]);
        r.set_resource(table);

        // The write is made only while what it touches is as it was shown.
        let change = ItemChange {
            expect: set
                .keys()
                .chain(&remove)
                .map(|name| (name.clone(), stored.get(name).cloned()))
                .collect(),
            set,
            remove,
        };
        r.mutate("update the item", |_| async {
            Ok(ddb.update_item(table, &key, &change).await?)
        })
        .await?;

        Ok(OpResult {
            views: vec![
                View::summary([
                    ("Table", table),
                    ("Key", shown_key.as_str()),
                    ("Result", "updated"),
                    ("Changes", changes.as_str()),
                ]),
                View::Diff {
                    left: "before".to_owned(),
                    right: "after".to_owned(),
                    delta,
                },
            ],
        })
    }
}

/// A usage error for `param`, given as `value`.
fn invalid(param: &str, value: &str, why: String) -> RunnerError {
    RunnerError::InvalidParam {
        param: param.to_owned(),
        value: value.to_owned(),
        source: why.into(),
    }
}

/// The attributes to set and the attributes to remove, as given; a name
/// given twice to remove counts once.
///
/// # Errors
///
/// A usage error when `set` is not a JSON object, or holds a whole number
/// too large to be shown exactly; when `remove` starts like a JSON array but
/// is not an array of names; when a name is empty; when nothing is to be set
/// or removed; and when an attribute is to be both set and removed.
fn requested(input: &Input) -> Result<(Item, Vec<String>), RunnerError> {
    let given = input.str("set").trim();
    let set = if given.is_empty() {
        Item::new()
    } else {
        // Parsed as written: a whole number keeps its own digits.
        match serde_json::from_str::<Value>(given) {
            Ok(Value::Object(set)) => set,
            Ok(_) => {
                return Err(invalid(
                    "set",
                    given,
                    "a JSON object of attribute names to values is needed".to_owned(),
                ));
            }
            Err(e) => return Err(invalid("set", given, format!("not valid JSON: {e}"))),
        }
    };
    if let Some(number) = set.values().find_map(inexact_number) {
        return Err(invalid(
            "set",
            given,
            format!(
                "the whole number {number} is too large for the change to be shown exactly (numbers are handled with double precision)"
            ),
        ));
    }
    if set.contains_key("") {
        return Err(invalid(
            "set",
            given,
            "an attribute name cannot be empty".to_owned(),
        ));
    }

    let listed = input.str("remove").trim();
    let names: Vec<String> = if listed.starts_with('[') {
        // What starts like a JSON array is one, or is refused: it is never
        // read as names separated by commas.
        serde_json::from_str(listed).map_err(|e| {
            invalid(
                "remove",
                listed,
                format!("not a JSON array of attribute names: {e}"),
            )
        })?
    } else {
        listed
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut remove: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if name.is_empty() {
            return Err(invalid(
                "remove",
                listed,
                "an attribute name cannot be empty".to_owned(),
            ));
        }
        if !remove.contains(&name) {
            remove.push(name);
        }
    }

    if set.is_empty() && remove.is_empty() {
        return Err(invalid(
            "set",
            given,
            "nothing to set or remove; give set, remove, or both".to_owned(),
        ));
    }
    if let Some(name) = remove.iter().find(|name| set.contains_key(*name)) {
        return Err(invalid("remove", name, format!("{name} is also being set")));
    }
    Ok((set, remove))
}

/// The first whole number in `value`, at any depth, that is too large to be
/// told from its neighbours once it is handled as a double.
fn inexact_number(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .filter(|f| f.fract() == 0.0 && f.abs() >= EXACT_LIMIT)
            .map(|_| n.to_string()),
        Value::Array(items) => items.iter().find_map(inexact_number),
        Value::Object(map) => map.values().find_map(inexact_number),
        _ => None,
    }
}

/// `item` as one value with every number compared by value.
fn comparable(item: &Item) -> Value {
    let mut value = Value::Object(item.clone());
    jsonutil::normalize(&mut value);
    value
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::ops::asked::Asked;
    use crate::ops::ddb::fixture;
    use crate::ops::{FailureKind, RunFailure, find_cause};
    use crate::svc::SvcError;
    use crate::svc::ddb::{DdbPort, FakeDdb, Item};

    const KEY: &str = "TenantId=acme,Version=V1";

    fn input(pairs: &[(&str, &str)]) -> Input {
        [("table", fixture::TABLE), ("key", KEY)]
            .into_iter()
            .chain(pairs.iter().copied())
            .collect()
    }

    async fn edit(
        ddb: &FakeDdb,
        asked: &Arc<Asked>,
        pairs: &[(&str, &str)],
    ) -> Result<OpResult, RunFailure> {
        Runner::new(fixture::deps(ddb.clone()))
            .confirmer(asked.clone())
            .execute(&ItemEdit, input(pairs))
            .await
    }

    fn key_of(version: &str) -> Item {
        let Value::Object(key) = json!({"TenantId": "acme", "Version": version}) else {
            unreachable!("a JSON object literal is an object")
        };
        key
    }

    async fn stored(ddb: &FakeDdb) -> Value {
        Value::Object(
            ddb.get_item(fixture::TABLE, &key_of("V1"))
                .await
                .expect("the item"),
        )
    }

    fn field<'a>(view: &'a View, name: &str) -> &'a str {
        let View::Summary { fields } = view else {
            panic!("expected a summary, got {view:?}");
        };
        fields
            .iter()
            .find_map(|(k, v)| (k == name).then_some(v.as_str()))
            .unwrap_or_else(|| panic!("no {name} in {view:?}"))
    }

    fn usage(failure: &RunFailure, param: &str) {
        assert_eq!(failure.kind, FailureKind::Usage, "{:#}", failure.error);
        assert!(
            matches!(
                find_cause::<RunnerError>(&failure.error),
                Some(RunnerError::InvalidParam { param: named, .. }) if named == param
            ),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn setting_and_removing_attributes_changes_only_those() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        let result = edit(
            &ddb,
            &asked,
            &[
                ("set", r#"{"Tier":"silver","note":"moved down"}"#),
                ("remove", "updatedAt"),
            ],
        )
        .await
        .expect("edited");

        assert_eq!(
            stored(&ddb).await,
            json!({
                "TenantId": "acme",
                "Version": "V1",
                "Tier": "silver",
                "note": "moved down",
                "limits": {"rps": 100},
            })
        );
        assert_eq!(field(&result.views[0], "Result"), "updated");
        assert_eq!(field(&result.views[0], "Changes"), "+1 −1 ~1");
        assert!(matches!(&result.views[1], View::Diff { left, right, .. }
            if left == "before" && right == "after"));
        // The item beside it is as it was.
        let other = ddb
            .get_item(fixture::TABLE, &key_of("V0"))
            .await
            .expect("the other item");
        assert_eq!(other["Tier"], "platinum");
    }

    #[tokio::test]
    async fn the_preview_is_the_diff_from_the_stored_item_to_the_edited_one() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        edit(
            &ddb,
            &asked,
            &[
                ("set", r#"{"limits":{"rps":250},"updatedAt":"2026-07-09"}"#),
                ("remove", "Tier"),
            ],
        )
        .await
        .expect("edited");

        let seen = asked.seen.lock().unwrap();
        let spec = &seen[0];
        // The table is what the operator types to confirm.
        assert_eq!(
            (spec.resource.as_str(), spec.class),
            ("tenants", Class::Destructive)
        );
        assert_eq!(field(&spec.preview[0], "Table"), "tenants");
        assert_eq!(field(&spec.preview[0], "Key"), "TenantId=acme|Version=V1");
        assert_eq!(field(&spec.preview[0], "Set"), "limits, updatedAt");
        assert_eq!(field(&spec.preview[0], "Remove"), "Tier");
        let View::Diff { left, right, delta } = &spec.preview[1] else {
            panic!("the second preview view is a diff: {:?}", spec.preview);
        };
        assert_eq!((left.as_str(), right.as_str()), ("stored", "edited"));
        let mut paths: Vec<_> = delta.changes.iter().map(|c| c.path.dotted()).collect();
        paths.sort();
        // A path the diff defaults ignore is written all the same, so it is
        // shown.
        assert_eq!(paths, ["Tier", "limits.rps", "updatedAt"]);
    }

    #[tokio::test]
    async fn an_edit_that_changes_nothing_writes_nothing_and_asks_nothing() {
        let ddb = fixture::fake();
        let before = stored(&ddb).await;
        let asked = Asked::answering(true);

        let result = edit(
            &ddb,
            &asked,
            &[
                ("set", r#"{"Tier":"gold","limits":{"rps":100.0}}"#),
                ("remove", "never-there"),
            ],
        )
        .await
        .expect("nothing to do");

        assert_eq!(asked.times(), 0);
        assert_eq!(stored(&ddb).await, before);
        assert!(
            field(&result.views[0], "Result").contains("nothing was changed"),
            "{:?}",
            result.views[0]
        );
    }

    #[tokio::test]
    async fn a_key_attribute_cannot_be_set_or_removed() {
        for (pairs, param) in [
            ([("set", r#"{"Version":"V9"}"#)], "set"),
            ([("remove", "TenantId")], "remove"),
        ] {
            let ddb = fixture::fake();
            let before = stored(&ddb).await;
            let asked = Asked::answering(true);

            let failure = edit(&ddb, &asked, &pairs)
                .await
                .expect_err("a key cannot be edited");

            usage(&failure, param);
            assert!(
                format!("{:#}", failure.error).contains("key attribute"),
                "{:#}",
                failure.error
            );
            assert_eq!(asked.times(), 0);
            assert_eq!(stored(&ddb).await, before);
        }
    }

    #[tokio::test]
    async fn set_must_be_a_json_object() {
        for bad in [r#"["Tier"]"#, r#""gold""#, "{Tier: gold}", "42"] {
            let ddb = fixture::fake();
            let asked = Asked::answering(true);

            let failure = edit(&ddb, &asked, &[("set", bad)])
                .await
                .expect_err("not an object of attributes");

            usage(&failure, "set");
            assert_eq!(asked.times(), 0, "{bad}");
        }
    }

    #[tokio::test]
    async fn an_edit_needs_something_to_set_or_remove() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        for pairs in [&[][..], &[("set", "{}")][..], &[("remove", " , ")][..]] {
            let failure = edit(&ddb, &asked, pairs)
                .await
                .expect_err("nothing to do was asked for");
            usage(&failure, "set");
        }
        assert_eq!(asked.times(), 0);
    }

    #[tokio::test]
    async fn an_attribute_cannot_be_both_set_and_removed() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        let failure = edit(
            &ddb,
            &asked,
            &[("set", r#"{"Tier":"silver"}"#), ("remove", "note,Tier")],
        )
        .await
        .expect_err("which is it");

        usage(&failure, "remove");
        assert!(
            format!("{:#}", failure.error).contains("Tier"),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn a_whole_number_too_large_to_be_shown_exactly_is_refused() {
        let ddb = fixture::fake();
        let before = stored(&ddb).await;
        let asked = Asked::answering(true);

        // The first is the smallest whole number whose neighbour a double
        // cannot tell from it.
        for number in [
            "9007199254740992",
            "9007199254740993",
            "-9007199254740993",
            "123456789012345678901234567890",
            "1.5e300",
        ] {
            let set = format!(r#"{{"limits":{{"quota":[{number}]}}}}"#);
            let failure = edit(&ddb, &asked, &[("set", &set)])
                .await
                .expect_err("the number could not be shown as it would be written");

            usage(&failure, "set");
            assert!(
                format!("{:#}", failure.error).contains("too large"),
                "{number}: {:#}",
                failure.error
            );
        }
        assert_eq!(asked.times(), 0);
        assert_eq!(stored(&ddb).await, before);
    }

    #[tokio::test]
    async fn numbers_are_stored_as_numbers() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        edit(
            &ddb,
            &asked,
            &[(
                "set",
                r#"{"count":9007199254740991,"ratio":0.5,"limits":{"rps":250}}"#,
            )],
        )
        .await
        .expect("edited");

        let item = stored(&ddb).await;
        assert_eq!(item["count"].as_u64(), Some(9_007_199_254_740_991));
        assert_eq!(item["ratio"], json!(0.5));
        assert_eq!(item["limits"], json!({"rps": 250}));
    }

    #[tokio::test]
    async fn a_missing_item_is_not_found_and_nothing_is_written() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        let failure = Runner::new(fixture::deps(ddb.clone()))
            .confirmer(asked.clone())
            .execute(
                &ItemEdit,
                [
                    ("table", fixture::TABLE),
                    ("key", "TenantId=acme,Version=V9"),
                    ("set", r#"{"Tier":"silver"}"#),
                ]
                .into_iter()
                .collect(),
            )
            .await
            .expect_err("no such item");

        let cause = find_cause::<SvcError>(&failure.error).expect("a service error");
        assert!(cause.is_not_found(), "{:#}", failure.error);
        assert_eq!(failure.kind, FailureKind::Failed);
        assert_eq!(asked.times(), 0);
        assert!(ddb.get_item(fixture::TABLE, &key_of("V9")).await.is_err());
    }

    /// The fixture item at `V1` with `Tier` and the rate limit replaced.
    fn rewritten(tier: &str, rps: i64) -> Item {
        let Value::Object(item) = json!({
            "TenantId": "acme",
            "Version": "V1",
            "Tier": tier,
            "limits": {"rps": rps},
            "updatedAt": "2026-07-V1",
        }) else {
            unreachable!("a JSON object literal is an object")
        };
        item
    }

    #[tokio::test]
    async fn a_change_after_the_preview_stops_the_edit() {
        let ddb = fixture::fake();
        let writer = ddb.clone();
        let asked = Asked::after(true, move || {
            writer.put(fixture::TABLE, rewritten("platinum", 100));
        });

        let failure = edit(&ddb, &asked, &[("set", r#"{"Tier":"silver"}"#)])
            .await
            .expect_err("the item is not what was previewed");

        let cause = find_cause::<SvcError>(&failure.error).expect("a service error");
        assert!(cause.is_changed(), "{:#}", failure.error);
        assert!(
            format!("{:#}", failure.error).contains("Tier"),
            "{:#}",
            failure.error
        );
        // The other writer's value stands.
        assert_eq!(stored(&ddb).await["Tier"], "platinum");
    }

    #[tokio::test]
    async fn a_change_to_another_attribute_after_the_preview_is_kept() {
        let ddb = fixture::fake();
        let writer = ddb.clone();
        let asked = Asked::after(true, move || {
            writer.put(fixture::TABLE, rewritten("gold", 999));
        });

        edit(&ddb, &asked, &[("set", r#"{"Tier":"silver"}"#)])
            .await
            .expect("the attribute edited is as it was previewed");

        let item = stored(&ddb).await;
        assert_eq!(item["Tier"], "silver");
        assert_eq!(
            item["limits"],
            json!({"rps": 999}),
            "the other write survives"
        );
    }

    #[tokio::test]
    async fn a_declined_edit_writes_nothing() {
        let ddb = fixture::fake();
        let before = stored(&ddb).await;
        let asked = Asked::answering(false);

        let failure = edit(&ddb, &asked, &[("set", r#"{"Tier":"silver"}"#)])
            .await
            .expect_err("declined");

        assert_eq!(failure.kind, FailureKind::Declined);
        assert_eq!(stored(&ddb).await, before);
    }

    #[tokio::test]
    async fn a_dry_run_previews_and_writes_nothing() {
        let ddb = fixture::fake();
        let before = stored(&ddb).await;
        let asked = Asked::answering(true);

        let result = Runner::new(fixture::deps(ddb.clone()))
            .confirmer(asked.clone())
            .dry_run(true)
            .execute(&ItemEdit, input(&[("set", r#"{"Tier":"silver"}"#)]))
            .await
            .expect("a dry run succeeds");

        assert_eq!(asked.times(), 0);
        assert_eq!(stored(&ddb).await, before);
        assert!(
            result.views.iter().any(|v| matches!(v, View::Diff { .. })),
            "{:?}",
            result.views
        );
    }

    #[tokio::test]
    async fn yes_does_not_confirm_an_edit() {
        let ddb = fixture::fake();
        let before = stored(&ddb).await;

        Runner::new(fixture::deps(ddb.clone()))
            .yes(true)
            .execute(&ItemEdit, input(&[("set", r#"{"Tier":"silver"}"#)]))
            .await
            .expect_err("only the typed table name confirms it");

        assert_eq!(stored(&ddb).await, before);
    }

    #[tokio::test]
    async fn remove_takes_a_json_array_for_names_with_commas() {
        let ddb = fixture::fake();
        let Value::Object(mut item) = stored(&ddb).await else {
            unreachable!("an item is an object")
        };
        item.insert("a,b".to_owned(), json!(1));
        ddb.put(fixture::TABLE, item);
        let asked = Asked::answering(true);

        edit(&ddb, &asked, &[("remove", r#"["a,b"]"#)])
            .await
            .expect("edited");

        assert!(stored(&ddb).await.get("a,b").is_none());
    }

    #[tokio::test]
    async fn a_remove_list_that_is_not_a_json_array_of_names_is_a_usage_error() {
        let ddb = fixture::fake();
        let before = stored(&ddb).await;
        let asked = Asked::answering(true);

        for bad in [r#"["Tier",]"#, r#"["Tier", 1]"#, "[Tier]", r#"[""]"#] {
            let failure = edit(&ddb, &asked, &[("remove", bad)])
                .await
                .expect_err("not a list of names");
            usage(&failure, "remove");
        }
        assert_eq!(asked.times(), 0);
        assert_eq!(stored(&ddb).await, before);
    }

    #[tokio::test]
    async fn an_empty_attribute_name_is_a_usage_error() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        let failure = edit(&ddb, &asked, &[("set", r#"{"":1}"#)])
            .await
            .expect_err("an attribute has a name");

        usage(&failure, "set");
        assert_eq!(asked.times(), 0);
    }

    #[tokio::test]
    async fn a_name_given_twice_is_removed_once() {
        let ddb = fixture::fake();
        let asked = Asked::answering(true);

        edit(&ddb, &asked, &[("remove", "Tier, updatedAt,Tier")])
            .await
            .expect("edited");

        let seen = asked.seen.lock().unwrap();
        assert_eq!(field(&seen[0].preview[0], "Remove"), "Tier, updatedAt");
    }

    #[tokio::test]
    async fn a_reordered_list_is_shown_though_the_diff_defaults_match_it_by_key() {
        let ddb = fixture::fake();
        let Value::Object(mut item) = stored(&ddb).await else {
            unreachable!("an item is an object")
        };
        item.insert("rules".to_owned(), json!([{"id": "a"}, {"id": "b"}]));
        ddb.put(fixture::TABLE, item);
        let mut deps = fixture::deps(ddb.clone());
        deps.diff
            .array_match
            .insert("$.rules[*]".to_owned(), "id".to_owned());
        let asked = Asked::answering(true);

        Runner::new(deps)
            .confirmer(asked.clone())
            .execute(
                &ItemEdit,
                input(&[("set", r#"{"rules":[{"id":"b"},{"id":"a"}]}"#)]),
            )
            .await
            .expect("edited");

        let seen = asked.seen.lock().unwrap();
        let View::Diff { delta, .. } = &seen[0].preview[1] else {
            panic!("the second preview view is a diff: {:?}", seen[0].preview);
        };
        assert!(
            delta.stats.total() > 0,
            "the reorder is written, so it is shown"
        );
        assert_ne!(field(&seen[0].preview[0], "Changes"), "no changes");
    }
}
