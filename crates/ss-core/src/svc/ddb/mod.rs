//! The DynamoDB port and the types it exchanges.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde_json::Value;

use super::SvcError;
use crate::jsonutil;

#[cfg(any(test, feature = "test-support"))]
pub mod contract;
#[cfg(any(test, feature = "test-support"))]
mod fake;
mod lazy;

pub use lazy::Lazy;

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeDdb;

/// A DynamoDB item (or key) as a JSON object of attribute name to value.
pub type Item = serde_json::Map<String, Value>;

/// The key attributes identifying one item, or the position a query resumes from.
pub type Key = Item;

/// A key-condition query against a table or index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuerySpec {
    /// Table to query; ignored by [`DdbPort::query_index`], which takes the
    /// table as an argument.
    pub table: String,
    /// Key-condition expression, e.g. `#pk = :pk`.
    pub key_condition: String,
    /// Expression attribute names, e.g. `#pk` to `TenantId`.
    pub names: BTreeMap<String, String>,
    /// Expression attribute values, e.g. `:pk` to `"acme"`.
    pub values: BTreeMap<String, Value>,
    /// Per-request page size; `None` leaves it to the service default.
    pub limit: Option<i32>,
    /// Whether to return results in reverse sort-key order.
    pub descending: bool,
    /// Position to resume from, taken from a previous page's `next`.
    pub exclusive_start: Option<Key>,
}

/// One page of query results.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemPage {
    /// Items on this page.
    pub items: Vec<Item>,
    /// Position to resume from; `None` on the last page.
    pub next: Option<Key>,
}

/// Which attributes a secondary index carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// Every attribute of the item.
    All,
    /// Only the key attributes.
    KeysOnly,
    /// The key attributes plus a chosen set.
    Include,
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::All => "ALL",
            Self::KeysOnly => "KEYS_ONLY",
            Self::Include => "INCLUDE",
        })
    }
}

/// The scalar type of a key attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrType {
    /// String.
    S,
    /// Number.
    N,
    /// Binary.
    B,
}

impl fmt::Display for AttrType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::S => "S",
            Self::N => "N",
            Self::B => "B",
        })
    }
}

/// A global secondary index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexInfo {
    /// Index name.
    pub name: String,
    /// Partition-key attribute.
    pub hash_key: String,
    /// Sort-key attribute, when the index has one.
    pub sort_key: Option<String>,
    /// Projection type, when reported.
    pub projection: Option<Projection>,
}

/// The key schema of a table and its global secondary indexes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableInfo {
    /// Table name.
    pub name: String,
    /// Partition-key attribute.
    pub hash_key: String,
    /// Sort-key attribute, when the table has one.
    pub sort_key: Option<String>,
    /// Key attribute to its declared scalar type.
    pub attr_types: BTreeMap<String, AttrType>,
    /// Global secondary indexes.
    pub gsis: Vec<IndexInfo>,
}

impl TableInfo {
    /// The global secondary index called exactly `name`, if any.
    pub fn index(&self, name: &str) -> Option<&IndexInfo> {
        self.gsis.iter().find(|i| i.name == name)
    }
}

/// A change to top-level attributes of one existing item, and what the
/// change was based on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemChange {
    /// Attributes to write; each value replaces the whole stored value.
    pub set: Item,
    /// Attributes to delete.
    pub remove: Vec<String>,
    /// What the item must still hold for the change to be made: an
    /// attribute's value as it was read, or `None` for an attribute that was
    /// not there.
    pub expect: BTreeMap<String, Option<Value>>,
}

impl ItemChange {
    /// Checks that the change can be made to the item at `key` at all.
    ///
    /// # Errors
    ///
    /// Returns [`SvcError::Other`] when the change sets and removes nothing,
    /// names a key attribute, or has an attribute both set and removed.
    pub fn check(&self, key: &Key) -> Result<(), SvcError> {
        if self.set.is_empty() && self.remove.is_empty() {
            return Err(SvcError::Other(anyhow::anyhow!(
                "the change sets and removes nothing"
            )));
        }
        let touched = || self.set.keys().chain(&self.remove);
        if let Some(name) = touched().find(|name| key.contains_key(name.as_str())) {
            return Err(SvcError::Other(anyhow::anyhow!(
                "{name} is a key attribute and cannot be changed"
            )));
        }
        if let Some(name) = self.remove.iter().find(|name| self.set.contains_key(*name)) {
            return Err(SvcError::Other(anyhow::anyhow!(
                "{name} is both set and removed"
            )));
        }
        Ok(())
    }

    /// The first attribute, by name, that `stored` no longer holds as the
    /// change expects; `None` when every expectation holds. Numbers compare
    /// by value, so `1` and `1.0` are the same.
    pub fn first_broken_expectation(&self, stored: &Item) -> Option<&str> {
        self.expect
            .iter()
            .find(|(name, expected)| !same(stored.get(*name), expected.as_ref()))
            .map(|(name, _)| name.as_str())
    }
}

/// Whether two attribute values, either of which may be absent, are the
/// same once their numbers are compared by value.
fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            let (mut a, mut b) = (a.clone(), b.clone());
            jsonutil::normalize(&mut a);
            jsonutil::normalize(&mut b);
            a == b
        }
        _ => false,
    }
}

/// A one-line rendering of `key`: attributes sorted by name as `name=value`
/// joined by `|`, string values unquoted and other values as compact JSON.
pub fn key_string(key: &Key) -> String {
    let mut entries: Vec<(&String, &Value)> = key.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let parts: Vec<String> = entries
        .into_iter()
        .map(|(name, value)| match value {
            Value::String(s) => format!("{name}={s}"),
            other => format!("{name}={}", jsonutil::compact(other)),
        })
        .collect();
    parts.join("|")
}

/// Access to DynamoDB: reads, and one write, which changes attributes of
/// one existing item.
#[async_trait]
pub trait DdbPort: Send + Sync {
    /// The names of every table visible to the caller, sorted.
    ///
    /// # Errors
    ///
    /// Returns [`SvcError::Other`] when the service call fails.
    async fn list_tables(&self) -> Result<Vec<String>, SvcError>;

    /// The key schema and global secondary indexes of `table`.
    ///
    /// # Errors
    ///
    /// Returns [`SvcError::NotFound`] when the table does not exist, and
    /// [`SvcError::Other`] when the service call fails.
    async fn describe_table(&self, table: &str) -> Result<TableInfo, SvcError>;

    /// The item identified by `key` in `table`.
    ///
    /// # Errors
    ///
    /// Returns [`SvcError::NotFound`] when no such item exists, and
    /// [`SvcError::Other`] when the service call fails.
    async fn get_item(&self, table: &str, key: &Key) -> Result<Item, SvcError>;

    /// One page of the items in `q.table` matching the key condition.
    ///
    /// # Errors
    ///
    /// Returns [`SvcError::NotFound`] when the table does not exist, and
    /// [`SvcError::Other`] when the service call fails.
    async fn query(&self, q: &QuerySpec) -> Result<ItemPage, SvcError>;

    /// One page of the items matching the key condition, read through
    /// `index` of `table`; `q.table` is ignored.
    ///
    /// # Errors
    ///
    /// Returns [`SvcError::NotFound`] when the table does not exist. A missing
    /// index is reported as an error, not necessarily [`SvcError::NotFound`]:
    /// DynamoDB answers it with a validation error, which surfaces as
    /// [`SvcError::Other`]. Any other service failure is also
    /// [`SvcError::Other`].
    async fn query_index(
        &self,
        table: &str,
        index: &str,
        q: &QuerySpec,
    ) -> Result<ItemPage, SvcError>;

    /// Applies `change` to the item at `key` in `table`, only if the item
    /// exists and still holds everything `change.expect` says. Attributes
    /// the change does not name are not touched.
    ///
    /// # Errors
    ///
    /// Returns [`SvcError::NotFound`] when the table or the item does not
    /// exist, and [`SvcError::Changed`] when an expectation no longer holds;
    /// nothing is written in either case. Returns [`SvcError::Other`] when
    /// the change cannot be made at all (see [`ItemChange::check`]), when an
    /// attribute to set holds a value that cannot be written back from
    /// JSON, or when the service call fails.
    async fn update_item(
        &self,
        table: &str,
        key: &Key,
        change: &ItemChange,
    ) -> Result<(), SvcError>;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item(v: serde_json::Value) -> Item {
        match v {
            serde_json::Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    #[test]
    fn key_string_sorts_names_and_unquotes_strings() {
        assert_eq!(
            key_string(&item(json!({"Version":"V0","TenantId":"acme"}))),
            "TenantId=acme|Version=V0"
        );
        assert_eq!(key_string(&item(json!({"Id":42}))), "Id=42");
        assert_eq!(key_string(&item(json!({"Id2":"b","Id":"a"}))), "Id=a|Id2=b");
    }

    #[test]
    fn index_lookup_is_by_exact_name() {
        let table = TableInfo {
            name: "Tenants".into(),
            hash_key: "TenantId".into(),
            gsis: vec![IndexInfo {
                name: "TierIndex".into(),
                hash_key: "Tier".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(table.index("TierIndex").is_some());
        assert!(table.index("tierindex").is_none());
    }

    /// A port with exactly the five read methods, the one write, and
    /// nothing else.
    struct ReadsAndOneWrite;

    #[async_trait]
    impl DdbPort for ReadsAndOneWrite {
        async fn update_item(
            &self,
            _table: &str,
            _key: &Key,
            _change: &ItemChange,
        ) -> Result<(), SvcError> {
            Ok(())
        }

        async fn list_tables(&self) -> Result<Vec<String>, SvcError> {
            Ok(Vec::new())
        }

        async fn describe_table(&self, _table: &str) -> Result<TableInfo, SvcError> {
            Ok(TableInfo::default())
        }

        async fn get_item(&self, _table: &str, _key: &Key) -> Result<Item, SvcError> {
            Ok(Item::new())
        }

        async fn query(&self, _q: &QuerySpec) -> Result<ItemPage, SvcError> {
            Ok(ItemPage::default())
        }

        async fn query_index(
            &self,
            _table: &str,
            _index: &str,
            _q: &QuerySpec,
        ) -> Result<ItemPage, SvcError> {
            Ok(ItemPage::default())
        }
    }

    #[test]
    fn the_only_write_in_the_port_is_an_update_of_one_item() {
        // Compiles only while the trait has no required method beyond the
        // five reads and the one write above.
        let _port: &dyn DdbPort = &ReadsAndOneWrite;

        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("split yields at least one part");
        assert!(
            production.contains("trait DdbPort"),
            "the production half of the port module must hold the trait"
        );
        let needles = [
            concat!("put", "_item"),
            concat!("delete", "_item"),
            concat!("batch", "_write"),
            concat!("transact", "_write"),
            concat!("execute", "_statement"),
            concat!("batch_execute", "_statement"),
            concat!("execute", "_transaction"),
            concat!("create", "_table"),
            concat!("update", "_table"),
            concat!("delete", "_table"),
        ];
        for needle in needles {
            assert!(
                !production.contains(needle),
                "the port source mentions {needle}"
            );
        }
    }

    #[test]
    fn projection_and_attribute_types_display_the_service_names() {
        assert_eq!(Projection::All.to_string(), "ALL");
        assert_eq!(Projection::KeysOnly.to_string(), "KEYS_ONLY");
        assert_eq!(Projection::Include.to_string(), "INCLUDE");
        assert_eq!(
            [AttrType::S, AttrType::N, AttrType::B].map(|t| t.to_string()),
            ["S", "N", "B"]
        );
    }

    fn change(set: serde_json::Value, remove: &[&str]) -> ItemChange {
        ItemChange {
            set: item(set),
            remove: remove.iter().map(|name| (*name).to_owned()).collect(),
            expect: BTreeMap::new(),
        }
    }

    #[test]
    fn a_change_must_do_something_and_leave_the_key_alone() {
        let key = item(json!({"TenantId": "acme", "Version": "V0"}));

        assert!(
            change(json!({"Tier": "gold"}), &["note"])
                .check(&key)
                .is_ok()
        );
        for (bad, why) in [
            (change(json!({}), &[]), "sets and removes nothing"),
            (change(json!({"Version": "V1"}), &[]), "key attribute"),
            (change(json!({}), &["TenantId"]), "key attribute"),
            (
                change(json!({"Tier": "gold"}), &["Tier"]),
                "both set and removed",
            ),
        ] {
            let err = bad.check(&key).expect_err(why);
            assert!(err.to_string().contains(why), "{err}");
            assert!(!err.is_not_found() && !err.is_changed(), "{err}");
        }
    }

    #[test]
    fn an_expectation_holds_while_the_attribute_is_as_it_was_read() {
        let stored = item(json!({"Tier": "gold", "limits": {"rps": 50, "burst": 100.0}}));
        let expecting = |name: &str, value: Option<serde_json::Value>| ItemChange {
            expect: [(name.to_owned(), value)].into(),
            ..ItemChange::default()
        };

        for held in [
            expecting("Tier", Some(json!("gold"))),
            // Numbers compare by value, and members in any order.
            expecting("limits", Some(json!({"burst": 100, "rps": 50.0}))),
            expecting("note", None),
        ] {
            assert_eq!(held.first_broken_expectation(&stored), None, "{held:?}");
        }
        for (broken, name) in [
            (expecting("Tier", Some(json!("platinum"))), "Tier"),
            (expecting("Tier", None), "Tier"),
            (expecting("note", Some(json!("x"))), "note"),
            (expecting("limits", Some(json!({"rps": 50}))), "limits"),
        ] {
            assert_eq!(
                broken.first_broken_expectation(&stored),
                Some(name),
                "{broken:?}"
            );
        }
    }
}
