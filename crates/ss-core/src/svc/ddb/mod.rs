//! The read-only DynamoDB port and the types it exchanges.

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

/// Read-only access to DynamoDB.
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
    /// Returns [`SvcError::NotFound`] when the table or index does not
    /// exist, and [`SvcError::Other`] when the service call fails.
    async fn query_index(
        &self,
        table: &str,
        index: &str,
        q: &QuerySpec,
    ) -> Result<ItemPage, SvcError>;
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

    /// A port with exactly the five read methods and nothing else.
    struct ReadOnly;

    #[async_trait]
    impl DdbPort for ReadOnly {
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
    fn the_port_trait_has_no_write_method() {
        // Compiles only while the trait has no required method beyond the
        // five reads above.
        let _port: &dyn DdbPort = &ReadOnly;

        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("split yields at least one part");
        let needles = [
            concat!("put", "_item"),
            concat!("update", "_item"),
            concat!("delete", "_item"),
            concat!("batch", "_write"),
            concat!("transact", "_write"),
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
}
