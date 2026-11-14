//! The assertions every [`DdbPort`] implementation must satisfy.
//!
//! [`run`] checks one seeded table against a port and panics, naming the
//! failed check and the value it observed, on the first violation. It backs
//! the in-memory fake and, under the `test-support` feature, the tests of
//! crates that implement the port against a real endpoint.

use std::fmt::Debug;

use serde_json::{Value, json};

use super::{
    AttrType, DdbPort, IndexInfo, Item, ItemChange, ItemPage, Key, Projection, QuerySpec, TableInfo,
};
use crate::svc::SvcError;

/// The key schema the suite expects `table` to have: hash `TenantId` (S),
/// sort `Version` (S), and a `TierIndex` index on `Tier` projecting every
/// attribute.
pub fn table_info(name: &str) -> TableInfo {
    TableInfo {
        name: name.to_owned(),
        hash_key: "TenantId".to_owned(),
        sort_key: Some("Version".to_owned()),
        attr_types: [
            ("TenantId".to_owned(), AttrType::S),
            ("Version".to_owned(), AttrType::S),
            ("Tier".to_owned(), AttrType::S),
        ]
        .into(),
        gsis: vec![IndexInfo {
            name: "TierIndex".to_owned(),
            hash_key: "Tier".to_owned(),
            sort_key: None,
            projection: Some(Projection::All),
        }],
    }
}

/// The items the suite expects in the table: tenant `acme` at versions `V0`
/// (platinum, 200 rps), `V1` (gold, 100 rps) and `V2` (gold, 50 rps).
pub fn seed_items() -> Vec<Item> {
    [
        ("V0", "platinum", 200),
        ("V1", "gold", 100),
        ("V2", "gold", 50),
    ]
    .into_iter()
    .map(|(version, tier, rps)| {
        let Value::Object(item) = json!({
            "TenantId": "acme",
            "Version": version,
            "Tier": tier,
            "limits": {"rps": rps, "burst": rps * 2},
        }) else {
            unreachable!("a JSON object literal is an object")
        };
        item
    })
    .collect()
}

/// Checks `port` against the suite.
///
/// `table` must have been created from [`table_info`] and seeded with
/// [`seed_items`]; `missing_table` must not exist.
///
/// # Panics
///
/// Panics, naming the failed check and the observed value, on the first
/// violation.
pub async fn run(port: &dyn DdbPort, table: &str, missing_table: &str) {
    let tables = ok("list_tables", port.list_tables().await);
    check(
        "list_tables contains the fixture table",
        tables.iter().any(|t| t == table),
        &tables,
    );

    let info = ok("describe_table", port.describe_table(table).await);
    check_eq(
        "describe_table hash key",
        &info.hash_key,
        &"TenantId".to_owned(),
    );
    check_eq(
        "describe_table sort key",
        &info.sort_key,
        &Some("Version".to_owned()),
    );
    check_eq(
        "describe_table type of TenantId",
        &info.attr_types.get("TenantId").copied(),
        &Some(AttrType::S),
    );
    let gsi = info.index("TierIndex");
    check(
        "describe_table reports TierIndex",
        gsi.is_some(),
        &info.gsis,
    );
    if let Some(gsi) = gsi {
        check_eq("TierIndex hash key", &gsi.hash_key, &"Tier".to_owned());
        check_eq(
            "TierIndex projection",
            &gsi.projection,
            &Some(Projection::All),
        );
    }

    let item = ok("get_item", port.get_item(table, &key("acme", "V0")).await);
    check_eq(
        "get_item limits.rps as f64",
        &item["limits"]["rps"].as_f64(),
        &Some(200.0),
    );
    check_eq("get_item Tier", &item["Tier"], &json!("platinum"));

    not_found(
        "get_item of a missing item",
        port.get_item(table, &key("acme", "V9")).await,
    );
    not_found(
        "get_item on a missing table",
        port.get_item(missing_table, &key("acme", "V0")).await,
    );
    not_found(
        "describe_table on a missing table",
        port.describe_table(missing_table).await,
    );
    not_found(
        "query on a missing table",
        port.query(&by_attr(missing_table, "TenantId", "acme"))
            .await,
    );

    let ascending = ok(
        "query",
        port.query(&by_attr(table, "TenantId", "acme")).await,
    );
    check_eq(
        "query ascending",
        &versions(&ascending),
        &["V0", "V1", "V2"].map(String::from).to_vec(),
    );
    check(
        "query ascending has no next page",
        ascending.next.is_none(),
        &ascending.next,
    );

    let newest = ok(
        "query descending",
        port.query(&QuerySpec {
            descending: true,
            limit: Some(1),
            ..by_attr(table, "TenantId", "acme")
        })
        .await,
    );
    check_eq(
        "query descending with limit 1",
        &versions(&newest),
        &vec!["V2".to_owned()],
    );

    let mut drained = Vec::new();
    let mut page_sizes = Vec::new();
    let mut start: Option<Key> = None;
    let mut last_next = None;
    for _ in 0..10 {
        let page = ok(
            "query paging",
            port.query(&QuerySpec {
                limit: Some(2),
                exclusive_start: start.take(),
                ..by_attr(table, "TenantId", "acme")
            })
            .await,
        );
        page_sizes.push(page.items.len());
        drained.extend(versions(&page));
        last_next = page.next.clone();
        start = page.next;
        if start.is_none() {
            break;
        }
    }
    check_eq("query paging page sizes", &page_sizes, &vec![2, 1]);
    check_eq(
        "query paging concatenation",
        &drained,
        &["V0", "V1", "V2"].map(String::from).to_vec(),
    );
    check(
        "query paging ends with no next",
        last_next.is_none(),
        &last_next,
    );

    let gold = ok(
        "query_index gold",
        port.query_index(table, "TierIndex", &by_attr(table, "Tier", "gold"))
            .await,
    );
    let mut gold_versions = versions(&gold);
    gold_versions.sort();
    check_eq(
        "query_index Tier=gold",
        &gold_versions,
        &["V1", "V2"].map(String::from).to_vec(),
    );

    let none = ok(
        "query_index unused tier",
        port.query_index(table, "TierIndex", &by_attr(table, "Tier", "bronze"))
            .await,
    );
    check(
        "query_index for an unused tier is empty",
        none.items.is_empty(),
        &none.items,
    );
}

/// Checks the port's one write against the suite. It changes item `acme`
/// `V2` of `table`, so call it after [`run`] or on a table of its own.
///
/// `table` must have been created from [`table_info`] and seeded with
/// [`seed_items`]; `missing_table` must not exist.
///
/// # Panics
///
/// Panics, naming the failed check and the observed value, on the first
/// violation.
pub async fn run_writes(port: &dyn DdbPort, table: &str, missing_table: &str) {
    let target = key("acme", "V2");
    let read = || async { ok("get_item", port.get_item(table, &target).await) };
    let before = read().await;

    let set = change(
        json!({"limits": {"rps": 75, "burst": 100}, "note": "raised"}),
        &[],
        &[("limits", before.get("limits").cloned()), ("note", None)],
    );
    ok(
        "update_item sets attributes",
        port.update_item(table, &target, &set).await,
    );
    let after_set = read().await;
    check_eq(
        "update_item wrote the new attributes",
        &(after_set.get("limits"), after_set.get("note")),
        &(
            Some(&json!({"rps": 75, "burst": 100})),
            Some(&json!("raised")),
        ),
    );
    check_eq(
        "update_item left the other attributes alone",
        &(
            after_set.get("Tier"),
            after_set.get("TenantId"),
            after_set.get("Version"),
        ),
        &(
            Some(&json!("gold")),
            Some(&json!("acme")),
            Some(&json!("V2")),
        ),
    );

    // The expectation spells a number differently from how it was written.
    let remove = change(
        json!({}),
        &["note"],
        &[
            ("note", Some(json!("raised"))),
            ("limits", Some(json!({"burst": 100.0, "rps": 75}))),
        ],
    );
    ok(
        "update_item removes an attribute",
        port.update_item(table, &target, &remove).await,
    );
    let after_remove = read().await;
    check(
        "update_item removed the attribute",
        !after_remove.contains_key("note"),
        &after_remove,
    );

    for (name, expect) in [
        ("a value that differs", ("Tier", Some(json!("platinum")))),
        ("an attribute expected absent that is there", ("Tier", None)),
        (
            "an attribute expected that is absent",
            ("note", Some(json!("raised"))),
        ),
    ] {
        let stale = change(json!({"Tier": "silver"}), &[], &[expect]);
        match port.update_item(table, &target, &stale).await {
            Err(e) if e.is_changed() => {}
            other => {
                panic!("contract: update_item with {name} should report a change, got {other:?}")
            }
        }
    }
    check_eq(
        "a refused update_item writes nothing",
        &read().await,
        &after_remove,
    );

    let harmless = change(json!({"note": "x"}), &[], &[]);
    not_found(
        "update_item of a missing item",
        port.update_item(table, &key("acme", "V9"), &harmless).await,
    );
    not_found(
        "get_item after update_item of a missing item",
        port.get_item(table, &key("acme", "V9")).await,
    );
    not_found(
        "update_item in a missing table",
        port.update_item(missing_table, &target, &harmless).await,
    );
    for (name, bad) in [
        (
            "a key attribute to set",
            change(json!({"Version": "V7"}), &[], &[]),
        ),
        (
            "a key attribute to remove",
            change(json!({}), &["TenantId"], &[]),
        ),
        ("nothing to set or remove", change(json!({}), &[], &[])),
    ] {
        check(
            &format!("update_item with {name} is refused"),
            port.update_item(table, &target, &bad).await.is_err(),
            &bad,
        );
    }
    check_eq(
        "a refused update_item writes nothing",
        &read().await,
        &after_remove,
    );
}

fn change(set: Value, remove: &[&str], expect: &[(&str, Option<Value>)]) -> ItemChange {
    let Value::Object(set) = set else {
        unreachable!("the attributes to set are a JSON object literal")
    };
    ItemChange {
        set,
        remove: remove.iter().map(|name| (*name).to_owned()).collect(),
        expect: expect
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect(),
    }
}

fn key(tenant: &str, version: &str) -> Key {
    let Value::Object(key) = json!({"TenantId": tenant, "Version": version}) else {
        unreachable!("a JSON object literal is an object")
    };
    key
}

fn by_attr(table: &str, attr: &str, value: &str) -> QuerySpec {
    QuerySpec {
        table: table.to_owned(),
        key_condition: "#k = :v".to_owned(),
        names: [("#k".to_owned(), attr.to_owned())].into(),
        values: [(":v".to_owned(), json!(value))].into(),
        ..QuerySpec::default()
    }
}

fn versions(page: &ItemPage) -> Vec<String> {
    page.items
        .iter()
        .map(|i| i["Version"].as_str().unwrap_or("<missing>").to_owned())
        .collect()
}

fn ok<T>(check: &str, result: Result<T, SvcError>) -> T {
    match result {
        Ok(v) => v,
        Err(e) => panic!("contract: {check} failed: {e:#}"),
    }
}

fn not_found<T: Debug>(check: &str, result: Result<T, SvcError>) {
    match result {
        Err(e) if e.is_not_found() => {}
        other => panic!("contract: {check} should report not found, got {other:?}"),
    }
}

fn check(name: &str, passed: bool, observed: &dyn Debug) {
    assert!(passed, "contract: {name}: observed {observed:?}");
}

fn check_eq<T: PartialEq + Debug>(name: &str, got: &T, want: &T) {
    assert!(
        got == want,
        "contract: {name}: expected {want:?}, observed {got:?}"
    );
}
