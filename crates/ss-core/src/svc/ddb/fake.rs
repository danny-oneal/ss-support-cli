//! An in-memory [`DdbPort`] for tests.
//!
//! Table items and index entries are kept separately, so a test can make an
//! index stale or incomplete and observe how a caller copes. Items are held in
//! key order, one ordered map per partition, so seeding thousands of items and
//! draining them page by page stays cheap.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::Value;

use super::{
    DdbPort, IndexInfo, Item, ItemChange, ItemPage, Key, Projection, QuerySpec, TableInfo,
};
use crate::svc::SvcError;

/// A number with a total order, so numeric keys sort numerically.
#[derive(Debug, Clone, Copy)]
struct Num(f64);

impl PartialEq for Num {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Num {}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Num {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// A key attribute value: strings sort lexicographically, numbers numerically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Scalar {
    Num(Num),
    Str(String),
    Other(String),
}

impl Scalar {
    fn of(value: &Value) -> Self {
        match value {
            Value::String(s) => Self::Str(s.clone()),
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(f64::NAN);
                // Fold -0.0 into 0.0 so both name the same key.
                Self::Num(Num(if f == 0.0 { 0.0 } else { f }))
            }
            other => Self::Other(other.to_string()),
        }
    }
}

/// The hash and sort values identifying a table item.
type BaseKey = (Scalar, Option<Scalar>);

/// Why a key attribute could not be read from an item.
fn missing_attr(name: &str) -> SvcError {
    SvcError::Other(anyhow::anyhow!("key attribute {name:?} is missing"))
}

fn attr(item: &Item, name: &str) -> Result<Scalar, SvcError> {
    item.get(name)
        .map(Scalar::of)
        .ok_or_else(|| missing_attr(name))
}

fn base_key(info: &TableInfo, item: &Item) -> Result<BaseKey, SvcError> {
    let hash = attr(item, &info.hash_key)?;
    let sort = info
        .sort_key
        .as_deref()
        .map(|n| attr(item, n))
        .transpose()?;
    Ok((hash, sort))
}

/// The named attributes of `item` that it has.
fn pick(item: &Item, names: &[Option<&str>]) -> Item {
    names
        .iter()
        .flatten()
        .filter_map(|name| item.get(*name).map(|v| ((*name).to_owned(), v.clone())))
        .collect()
}

/// A global secondary index: entries grouped by the index's hash value.
#[derive(Debug, Default)]
struct Index {
    info: IndexInfo,
    /// Index hash value to entries ordered by index sort value then base key.
    parts: BTreeMap<Scalar, BTreeMap<(Option<Scalar>, BaseKey), Item>>,
    /// Base key to where that item's entry sits in `parts`.
    by_base: BTreeMap<BaseKey, (Scalar, Option<Scalar>)>,
}

impl Index {
    fn remove(&mut self, base: &BaseKey) {
        let Some((hash, sort)) = self.by_base.remove(base) else {
            return;
        };
        if let Some(part) = self.parts.get_mut(&hash) {
            part.remove(&(sort, base.clone()));
            if part.is_empty() {
                self.parts.remove(&hash);
            }
        }
    }

    /// Stores `entry`, replacing the entry for the same base key.
    fn place(&mut self, table: &TableInfo, entry: Item) -> Result<(), SvcError> {
        let base = base_key(table, &entry)?;
        let hash = attr(&entry, &self.info.hash_key)?;
        let sort = self
            .info
            .sort_key
            .as_deref()
            .map(|n| attr(&entry, n))
            .transpose()?;
        self.remove(&base);
        self.by_base
            .insert(base.clone(), (hash.clone(), sort.clone()));
        self.parts
            .entry(hash)
            .or_default()
            .insert((sort, base), entry);
        Ok(())
    }

    /// The entry an item produces, or `None` when it lacks an index key
    /// attribute (as in a sparse index).
    fn derive(&self, table: &TableInfo, item: &Item) -> Option<Item> {
        item.get(&self.info.hash_key)?;
        if let Some(sort) = &self.info.sort_key {
            item.get(sort)?;
        }
        Some(match self.info.projection {
            Some(Projection::KeysOnly | Projection::Include) => pick(item, &self.key_names(table)),
            Some(Projection::All) | None => item.clone(),
        })
    }

    /// Base-table and index key attribute names.
    fn key_names<'a>(&'a self, table: &'a TableInfo) -> [Option<&'a str>; 4] {
        [
            Some(table.hash_key.as_str()),
            table.sort_key.as_deref(),
            Some(self.info.hash_key.as_str()),
            self.info.sort_key.as_deref(),
        ]
    }
}

#[derive(Debug)]
struct Table {
    info: TableInfo,
    /// Hash value to items ordered by sort value.
    items: BTreeMap<Scalar, BTreeMap<Option<Scalar>, Item>>,
    indexes: BTreeMap<String, Index>,
}

impl Table {
    /// Stores `item`, replacing the item with the same key, and refreshes
    /// the entry of every index derived from it.
    fn store(&mut self, item: Item) -> Result<(), SvcError> {
        let (hash, sort) = base_key(&self.info, &item)?;
        for index in self.indexes.values_mut() {
            index.remove(&(hash.clone(), sort.clone()));
            if let Some(entry) = index.derive(&self.info, &item) {
                index.place(&self.info, entry)?;
            }
        }
        self.items.entry(hash).or_default().insert(sort, item);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct State {
    tables: BTreeMap<String, Table>,
}

impl State {
    fn table(&self, name: &str) -> Result<&Table, SvcError> {
        self.tables
            .get(name)
            .ok_or_else(|| SvcError::not_found(format!("table {name}")))
    }
}

/// An in-memory DynamoDB. Clones share the same data. `Include` projections
/// behave like `KeysOnly` in the fake.
#[derive(Debug, Clone, Default)]
pub struct FakeDdb {
    state: Arc<Mutex<State>>,
    page_size: Option<usize>,
}

impl FakeDdb {
    /// An empty fake whose responses are not capped.
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle onto the same data whose every response holds at most
    /// `page_size` items.
    ///
    /// # Panics
    ///
    /// Panics when `page_size` is zero, which could never make progress.
    pub fn paged(&self, page_size: usize) -> FakeDdb {
        assert!(page_size > 0, "page size must be at least 1");
        FakeDdb {
            state: Arc::clone(&self.state),
            page_size: Some(page_size),
        }
    }

    /// Creates an empty table, replacing any table with the same name.
    pub fn create_table(&self, info: TableInfo) {
        let indexes = info
            .gsis
            .iter()
            .map(|gsi| {
                let index = Index {
                    info: gsi.clone(),
                    ..Index::default()
                };
                (gsi.name.clone(), index)
            })
            .collect();
        let name = info.name.clone();
        let table = Table {
            info,
            items: BTreeMap::new(),
            indexes,
        };
        self.lock().tables.insert(name, table);
    }

    /// Stores `item`, replacing the item with the same key, and refreshes the
    /// entry of every index derived from it.
    ///
    /// # Panics
    ///
    /// Panics when the table does not exist or `item` lacks a key attribute.
    pub fn put(&self, table: &str, item: Item) {
        let mut state = self.lock();
        let t = state
            .tables
            .get_mut(table)
            .unwrap_or_else(|| panic!("put into missing table {table:?}"));
        t.store(item)
            .unwrap_or_else(|e| panic!("put into {table:?}: {e}"));
    }

    /// Replaces (or adds) the entry of `index` whose base key equals
    /// `entry`'s, leaving the table item alone.
    ///
    /// # Panics
    ///
    /// Panics when the table or index does not exist or `entry` lacks a base
    /// or index key attribute.
    pub fn set_index_entry(&self, table: &str, index: &str, entry: Item) {
        let mut state = self.lock();
        let t = state
            .tables
            .get_mut(table)
            .unwrap_or_else(|| panic!("missing table {table:?}"));
        let idx = t
            .indexes
            .get_mut(index)
            .unwrap_or_else(|| panic!("missing index {index:?} on {table:?}"));
        idx.place(&t.info, entry)
            .unwrap_or_else(|e| panic!("set_index_entry {index:?}: {e}"));
    }

    /// Removes the entry of `index` for the item with base key `key`, leaving
    /// the table item alone. Does nothing when there is no such entry.
    ///
    /// # Panics
    ///
    /// Panics when the table or index does not exist or `key` lacks a base
    /// key attribute.
    pub fn remove_index_entry(&self, table: &str, index: &str, key: &Key) {
        let mut state = self.lock();
        let t = state
            .tables
            .get_mut(table)
            .unwrap_or_else(|| panic!("missing table {table:?}"));
        let base =
            base_key(&t.info, key).unwrap_or_else(|e| panic!("remove_index_entry {index:?}: {e}"));
        t.indexes
            .get_mut(index)
            .unwrap_or_else(|| panic!("missing index {index:?} on {table:?}"))
            .remove(&base);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The attribute and value of a `<name> = <placeholder>` key condition.
fn parse_condition(q: &QuerySpec) -> Result<(String, Value), SvcError> {
    let unsupported = || {
        SvcError::Other(anyhow::anyhow!(
            "unsupported key condition {:?}: only `<name> = <placeholder>` is evaluated",
            q.key_condition
        ))
    };
    let (lhs, rhs) = q.key_condition.split_once('=').ok_or_else(unsupported)?;
    let (lhs, rhs) = (lhs.trim(), rhs.trim());
    let is_word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_');
    let name = match lhs.strip_prefix('#') {
        Some(alias) if is_word(alias) => q.names.get(lhs).cloned().ok_or_else(|| {
            SvcError::Other(anyhow::anyhow!("attribute name alias {lhs:?} is not bound"))
        })?,
        None if is_word(lhs) => lhs.to_owned(),
        _ => return Err(unsupported()),
    };
    match rhs.strip_prefix(':') {
        Some(word) if is_word(word) => {
            let value = q.values.get(rhs).cloned().ok_or_else(|| {
                SvcError::Other(anyhow::anyhow!("attribute value {rhs:?} is not bound"))
            })?;
            Ok((name, value))
        }
        _ => Err(unsupported()),
    }
}

/// The number of items a response may hold: the smaller of the request's
/// limit and the fake's page size.
fn effective_limit(q: &QuerySpec, page_size: Option<usize>) -> Result<usize, SvcError> {
    let requested = match q.limit {
        None => None,
        Some(n) => match usize::try_from(n) {
            Ok(n) if n > 0 => Some(n),
            _ => {
                return Err(SvcError::Other(anyhow::anyhow!(
                    "limit must be at least 1, got {n}"
                )));
            }
        },
    };
    Ok(match (requested, page_size) {
        (Some(a), Some(b)) => a.min(b),
        (Some(n), None) | (None, Some(n)) => n,
        (None, None) => usize::MAX,
    })
}

/// Up to `n` entries of `part` after `after` in the requested direction, and
/// whether more follow them.
fn window<'a, K: Ord>(
    part: &'a BTreeMap<K, Item>,
    after: Option<&K>,
    descending: bool,
    n: usize,
) -> (Vec<&'a Item>, bool) {
    let bounds = match (after, descending) {
        (None, _) => (Bound::Unbounded, Bound::Unbounded),
        (Some(k), false) => (Bound::Excluded(k), Bound::Unbounded),
        (Some(k), true) => (Bound::Unbounded, Bound::Excluded(k)),
    };
    let range = part.range::<K, _>(bounds);
    let mut entries: Box<dyn Iterator<Item = &'a Item> + 'a> = if descending {
        Box::new(range.rev().map(|(_, v)| v))
    } else {
        Box::new(range.map(|(_, v)| v))
    };
    let page: Vec<&Item> = entries.by_ref().take(n).collect();
    let more = entries.next().is_some();
    (page, more)
}

fn foreign_cursor(start: &Key) -> SvcError {
    SvcError::Other(anyhow::anyhow!(
        "exclusive start key {} was not returned by this query",
        super::key_string(start)
    ))
}

fn require_hash_condition(q: &QuerySpec, hash_key: &str, what: &str) -> Result<Scalar, SvcError> {
    let (name, value) = parse_condition(q)?;
    if name != hash_key {
        return Err(SvcError::Other(anyhow::anyhow!(
            "key condition on {name:?}, but the partition key of {what} is {hash_key:?}"
        )));
    }
    Ok(Scalar::of(&value))
}

fn query_table(
    table: &Table,
    q: &QuerySpec,
    page_size: Option<usize>,
) -> Result<ItemPage, SvcError> {
    let info = &table.info;
    let hash = require_hash_condition(q, &info.hash_key, &format!("table {}", info.name))?;
    let n = effective_limit(q, page_size)?;
    let empty = BTreeMap::new();
    let part = table.items.get(&hash).unwrap_or(&empty);
    let after = match &q.exclusive_start {
        None => None,
        Some(start) => {
            let (h, sort) = base_key(info, start)?;
            if h != hash || !part.contains_key(&sort) {
                return Err(foreign_cursor(start));
            }
            Some(sort)
        }
    };
    let (items, more) = window(part, after.as_ref(), q.descending, n);
    let next = match items.last() {
        Some(last) if more => Some(pick(
            last,
            &[Some(info.hash_key.as_str()), info.sort_key.as_deref()],
        )),
        _ => None,
    };
    Ok(ItemPage {
        items: items.into_iter().cloned().collect(),
        next,
    })
}

fn query_index_of(
    table: &Table,
    index: &Index,
    q: &QuerySpec,
    page_size: Option<usize>,
) -> Result<ItemPage, SvcError> {
    let what = format!("index {} of {}", index.info.name, table.info.name);
    let hash = require_hash_condition(q, &index.info.hash_key, &what)?;
    let n = effective_limit(q, page_size)?;
    let empty = BTreeMap::new();
    let part = index.parts.get(&hash).unwrap_or(&empty);
    let after = match &q.exclusive_start {
        None => None,
        Some(start) => {
            let base = base_key(&table.info, start)?;
            match index.by_base.get(&base) {
                Some((h, sort)) if *h == hash => Some((sort.clone(), base)),
                _ => return Err(foreign_cursor(start)),
            }
        }
    };
    let (items, more) = window(part, after.as_ref(), q.descending, n);
    let next = match items.last() {
        Some(last) if more => Some(pick(last, &index.key_names(&table.info))),
        _ => None,
    };
    Ok(ItemPage {
        items: items.into_iter().cloned().collect(),
        next,
    })
}

#[async_trait]
impl DdbPort for FakeDdb {
    async fn list_tables(&self) -> Result<Vec<String>, SvcError> {
        Ok(self.lock().tables.keys().cloned().collect())
    }

    async fn describe_table(&self, table: &str) -> Result<TableInfo, SvcError> {
        Ok(self.lock().table(table)?.info.clone())
    }

    async fn get_item(&self, table: &str, key: &Key) -> Result<Item, SvcError> {
        let state = self.lock();
        let t = state.table(table)?;
        let (hash, sort) = base_key(&t.info, key)?;
        t.items
            .get(&hash)
            .and_then(|part| part.get(&sort))
            .cloned()
            .ok_or_else(|| SvcError::not_found(format!("{table}/{}", super::key_string(key))))
    }

    async fn query(&self, q: &QuerySpec) -> Result<ItemPage, SvcError> {
        let state = self.lock();
        query_table(state.table(&q.table)?, q, self.page_size)
    }

    async fn update_item(
        &self,
        table: &str,
        key: &Key,
        change: &ItemChange,
    ) -> Result<(), SvcError> {
        change.check(key)?;
        let mut state = self.lock();
        let t = state
            .tables
            .get_mut(table)
            .ok_or_else(|| SvcError::not_found(format!("table {table}")))?;
        let what = format!("{table} {}", super::key_string(key));
        let (hash, sort) = base_key(&t.info, key)?;
        let Some(stored) = t.items.get(&hash).and_then(|part| part.get(&sort)) else {
            return Err(SvcError::not_found(what));
        };
        if let Some(name) = change.first_broken_expectation(stored) {
            return Err(SvcError::changed(format!("{what}: attribute {name}")));
        }
        let mut item = stored.clone();
        item.extend(change.set.clone());
        for name in &change.remove {
            item.remove(name);
        }
        t.store(item)
    }

    async fn query_index(
        &self,
        table: &str,
        index: &str,
        q: &QuerySpec,
    ) -> Result<ItemPage, SvcError> {
        let state = self.lock();
        let t = state.table(table)?;
        let idx = t
            .indexes
            .get(index)
            .ok_or_else(|| SvcError::not_found(format!("index {index} of table {table}")))?;
        query_index_of(t, idx, q, self.page_size)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::svc::ddb::{
        AttrType, DdbPort, Item, ItemPage, Key, Projection, QuerySpec, contract,
    };

    fn obj(v: Value) -> Item {
        match v {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    fn tenants() -> FakeDdb {
        let ddb = FakeDdb::new();
        ddb.create_table(contract::table_info("tenants"));
        for item in contract::seed_items() {
            ddb.put("tenants", item);
        }
        ddb
    }

    fn key(tenant: &str, version: &str) -> Key {
        obj(json!({"TenantId": tenant, "Version": version}))
    }

    fn by_attr(attr: &str, value: &str) -> QuerySpec {
        QuerySpec {
            table: "tenants".into(),
            key_condition: "#pk = :pk".into(),
            names: [("#pk".to_owned(), attr.to_owned())].into(),
            values: [(":pk".to_owned(), json!(value))].into(),
            ..Default::default()
        }
    }

    fn by_tenant(tenant: &str) -> QuerySpec {
        by_attr("TenantId", tenant)
    }

    fn by_tier(tier: &str) -> QuerySpec {
        by_attr("Tier", tier)
    }

    fn versions(page: &ItemPage) -> Vec<&str> {
        page.items
            .iter()
            .map(|i| i["Version"].as_str().expect("Version is a string"))
            .collect()
    }

    #[tokio::test]
    async fn get_item_and_missing_shapes() {
        let ddb = tenants();
        let got = ddb
            .get_item("tenants", &key("acme", "V0"))
            .await
            .expect("item");
        assert_eq!(got["Tier"], "platinum");
        let missing = ddb
            .get_item("tenants", &key("acme", "V9"))
            .await
            .expect_err("no such item");
        assert!(missing.is_not_found(), "{missing}");
        let no_table = ddb
            .get_item("nope", &key("acme", "V0"))
            .await
            .expect_err("no such table");
        assert!(no_table.is_not_found(), "{no_table}");
        assert_eq!(ddb.list_tables().await.expect("tables"), ["tenants"]);
    }

    #[tokio::test]
    async fn query_orders_by_sort_key_and_honours_descending_and_limit() {
        let ddb = tenants();
        let all = ddb.query(&by_tenant("acme")).await.expect("query");
        assert_eq!(versions(&all), ["V0", "V1", "V2"]);
        assert_eq!(all.next, None);

        let newest = ddb
            .query(&QuerySpec {
                descending: true,
                limit: Some(1),
                ..by_tenant("acme")
            })
            .await
            .expect("descending");
        assert_eq!(versions(&newest), ["V2"]);
        assert_eq!(newest.next, Some(key("acme", "V2")));
    }

    #[tokio::test]
    async fn paging_resumes_after_the_cursor_and_rejects_foreign_cursors() {
        let ddb = tenants();
        let first = ddb
            .query(&QuerySpec {
                limit: Some(2),
                ..by_tenant("acme")
            })
            .await
            .expect("page 1");
        assert_eq!(versions(&first), ["V0", "V1"]);
        assert!(first.next.is_some());

        let second = ddb
            .query(&QuerySpec {
                limit: Some(2),
                exclusive_start: first.next,
                ..by_tenant("acme")
            })
            .await
            .expect("page 2");
        assert_eq!(versions(&second), ["V2"]);
        assert_eq!(second.next, None);

        let err = ddb
            .query(&QuerySpec {
                exclusive_start: Some(key("acme", "V7")),
                ..by_tenant("acme")
            })
            .await
            .expect_err("foreign cursor");
        assert!(!err.is_not_found(), "{err}");
    }

    #[tokio::test]
    async fn paged_fake_caps_every_response() {
        let ddb = tenants().paged(1);
        let mut seen = Vec::new();
        let mut start = None;
        for _ in 0..3 {
            let page = ddb
                .query(&QuerySpec {
                    exclusive_start: start.take(),
                    ..by_tenant("acme")
                })
                .await
                .expect("page");
            assert_eq!(page.items.len(), 1);
            seen.push(versions(&page)[0].to_owned());
            start = page.next;
        }
        assert_eq!(seen, ["V0", "V1", "V2"]);
        assert_eq!(start, None);
    }

    #[tokio::test]
    async fn index_entries_follow_puts_and_can_be_made_stale() {
        let ddb = tenants();
        let gold = ddb
            .query_index("tenants", "TierIndex", &by_tier("gold"))
            .await
            .expect("gold");
        assert_eq!(versions(&gold), ["V1", "V2"]);

        let mut stale = ddb
            .get_item("tenants", &key("acme", "V1"))
            .await
            .expect("V1");
        stale["limits"] = json!({"rps": 1, "burst": 2});
        ddb.set_index_entry("tenants", "TierIndex", stale);
        let via_index = ddb
            .query_index("tenants", "TierIndex", &by_tier("gold"))
            .await
            .expect("gold");
        assert_eq!(via_index.items[0]["limits"]["rps"], 1);
        let via_table = ddb
            .get_item("tenants", &key("acme", "V1"))
            .await
            .expect("V1");
        assert_eq!(via_table["limits"]["rps"], 100);

        ddb.remove_index_entry("tenants", "TierIndex", &key("acme", "V2"));
        let gold = ddb
            .query_index("tenants", "TierIndex", &by_tier("gold"))
            .await
            .expect("gold");
        assert_eq!(versions(&gold), ["V1"]);
        ddb.get_item("tenants", &key("acme", "V2"))
            .await
            .expect("table still has V2");
    }

    #[tokio::test]
    async fn keys_only_index_projects_only_key_attributes() {
        let ddb = FakeDdb::new();
        let mut info = contract::table_info("tenants");
        info.gsis[0].projection = Some(Projection::KeysOnly);
        ddb.create_table(info);
        for item in contract::seed_items() {
            ddb.put("tenants", item);
        }
        let page = ddb
            .query_index("tenants", "TierIndex", &by_tier("gold"))
            .await
            .expect("gold");
        assert_eq!(page.items.len(), 2);
        for entry in &page.items {
            let names: Vec<&str> = entry.keys().map(String::as_str).collect();
            assert_eq!(names, ["TenantId", "Tier", "Version"], "{entry:?}");
        }
        let described = ddb.describe_table("tenants").await.expect("describe");
        assert_eq!(described.attr_types["TenantId"], AttrType::S);
    }

    #[tokio::test]
    async fn unsupported_key_conditions_are_rejected() {
        let ddb = tenants();
        let err = ddb
            .query(&QuerySpec {
                key_condition: "#pk = :pk AND begins_with(#sk, :p)".into(),
                names: [
                    ("#pk".to_owned(), "TenantId".to_owned()),
                    ("#sk".to_owned(), "Version".to_owned()),
                ]
                .into(),
                values: [
                    (":pk".to_owned(), json!("acme")),
                    (":p".to_owned(), json!("V")),
                ]
                .into(),
                ..by_tenant("acme")
            })
            .await
            .expect_err("unsupported");
        assert!(!err.is_not_found(), "{err}");
    }

    #[tokio::test]
    async fn many_puts_stay_fast_and_drain_in_order() {
        let ddb = FakeDdb::new();
        ddb.create_table(contract::table_info("tenants"));
        for n in 0..10_001 {
            ddb.put(
                "tenants",
                obj(json!({"TenantId": "big", "Version": format!("V{n:05}"), "Tier": "gold"})),
            );
        }
        let ddb = ddb.paged(1000);
        let mut count = 0;
        let mut start = None;
        loop {
            let page = ddb
                .query(&QuerySpec {
                    exclusive_start: start.take(),
                    ..by_tenant("big")
                })
                .await
                .expect("page");
            count += page.items.len();
            start = page.next;
            if start.is_none() {
                break;
            }
        }
        assert_eq!(count, 10_001);
    }

    #[tokio::test]
    async fn fake_satisfies_the_contract() {
        contract::run(&tenants(), "tenants", "nope").await;
    }

    fn editing(set: Value, remove: &[&str], expect: &[(&str, Option<Value>)]) -> ItemChange {
        let Value::Object(set) = set else {
            panic!("the attributes to set are an object");
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

    #[tokio::test]
    async fn update_sets_and_removes_attributes_and_leaves_the_rest() {
        let ddb = tenants();
        let before = ddb
            .get_item("tenants", &key("acme", "V1"))
            .await
            .expect("item");

        ddb.update_item(
            "tenants",
            &key("acme", "V1"),
            &editing(
                serde_json::json!({"note": "raised", "limits": {"rps": 75}}),
                &["Tier"],
                &[("Tier", Some(before["Tier"].clone())), ("note", None)],
            ),
        )
        .await
        .expect("update");

        let after = ddb
            .get_item("tenants", &key("acme", "V1"))
            .await
            .expect("item");
        assert_eq!(
            Value::Object(after),
            serde_json::json!({
                "TenantId": "acme",
                "Version": "V1",
                "note": "raised",
                "limits": {"rps": 75},
            })
        );
        // The item beside it is untouched.
        let other = ddb
            .get_item("tenants", &key("acme", "V0"))
            .await
            .expect("item");
        assert_eq!(other["Tier"], "platinum");
    }

    #[tokio::test]
    async fn update_of_a_missing_item_or_table_is_not_found() {
        let ddb = tenants();
        let change = editing(serde_json::json!({"note": "x"}), &[], &[]);

        let missing = ddb
            .update_item("tenants", &key("acme", "V9"), &change)
            .await
            .expect_err("no such item");
        assert!(missing.is_not_found(), "{missing}");
        let no_table = ddb
            .update_item("nope", &key("acme", "V0"), &change)
            .await
            .expect_err("no such table");
        assert!(no_table.is_not_found(), "{no_table}");
        // Nothing was created.
        assert!(ddb.get_item("tenants", &key("acme", "V9")).await.is_err());
    }

    #[tokio::test]
    async fn update_is_refused_as_changed_when_an_expectation_no_longer_holds() {
        let ddb = tenants();
        let before = ddb
            .get_item("tenants", &key("acme", "V1"))
            .await
            .expect("item");

        for expect in [
            ("Tier", Some(serde_json::json!("platinum"))),
            ("Tier", None),
            ("note", Some(serde_json::json!("was here"))),
        ] {
            let err = ddb
                .update_item(
                    "tenants",
                    &key("acme", "V1"),
                    &editing(
                        serde_json::json!({"Tier": "silver"}),
                        &[],
                        std::slice::from_ref(&expect),
                    ),
                )
                .await
                .expect_err("the item is not as expected");
            assert!(err.is_changed(), "{expect:?}: {err}");
            assert!(err.to_string().contains(expect.0), "{err}");
        }
        assert_eq!(
            ddb.get_item("tenants", &key("acme", "V1"))
                .await
                .expect("item"),
            before,
            "a refused update writes nothing"
        );
    }

    #[tokio::test]
    async fn an_expected_number_matches_whatever_its_spelling() {
        let ddb = tenants();

        ddb.update_item(
            "tenants",
            &key("acme", "V1"),
            &editing(
                serde_json::json!({"note": "ok"}),
                &[],
                &[(
                    "limits",
                    Some(serde_json::json!({"rps": 100.0, "burst": 200})),
                )],
            ),
        )
        .await
        .expect("the limits are as expected");
    }

    #[tokio::test]
    async fn update_refuses_key_attributes_and_an_empty_change() {
        let ddb = tenants();

        for change in [
            editing(serde_json::json!({"Version": "V7"}), &[], &[]),
            editing(serde_json::json!({}), &["TenantId"], &[]),
            editing(serde_json::json!({}), &[], &[]),
        ] {
            let err = ddb
                .update_item("tenants", &key("acme", "V1"), &change)
                .await
                .expect_err("not a change that can be made");
            assert!(!err.is_not_found() && !err.is_changed(), "{err}");
        }
        assert_eq!(
            versions(&ddb.query(&by_tenant("acme")).await.expect("query")),
            ["V0", "V1", "V2"]
        );
    }

    #[tokio::test]
    async fn index_entries_follow_an_update() {
        let ddb = tenants();

        ddb.update_item(
            "tenants",
            &key("acme", "V1"),
            &editing(serde_json::json!({"Tier": "silver"}), &[], &[]),
        )
        .await
        .expect("update");

        let silver = ddb
            .query_index("tenants", "TierIndex", &by_tier("silver"))
            .await
            .expect("index");
        assert_eq!(versions(&silver), ["V1"]);
        let gold = ddb
            .query_index("tenants", "TierIndex", &by_tier("gold"))
            .await
            .expect("index");
        assert_eq!(versions(&gold), ["V2"]);
    }
}
