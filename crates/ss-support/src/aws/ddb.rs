//! The DynamoDB port backed by the AWS SDK.
//!
//! A missing table or index becomes [`SvcError::NotFound`]; every other SDK
//! failure is kept whole as [`SvcError::Other`]. The one write is a
//! conditional update of attributes of one item; no other write call is
//! issued.

use std::collections::HashMap;

use anyhow::anyhow;
use async_trait::async_trait;
use aws_config::SdkConfig;
use aws_sdk_dynamodb::config::http::HttpResponse;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::operation::query::builders::QueryFluentBuilder;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::{
    AttributeValue, KeyType, ProjectionType, ScalarAttributeType, TableDescription,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Number, Value};
use ss_core::jsonutil;
use ss_core::svc::SvcError;
use ss_core::svc::ddb::{
    AttrType, DdbPort, IndexInfo, Item, ItemChange, ItemPage, Key, Projection, QuerySpec,
    TableInfo, key_string,
};

/// How [`AwsDdb`] reaches DynamoDB.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    /// A DynamoDB-compatible endpoint URL (DynamoDB Local, LocalStack).
    pub endpoint: Option<String>,
}

/// [`DdbPort`] over the AWS SDK client.
#[derive(Debug, Clone)]
pub(crate) struct AwsDdb {
    client: aws_sdk_dynamodb::Client,
}

impl AwsDdb {
    /// Builds the adapter from resolved SDK configuration.
    pub(crate) fn new(cfg: &SdkConfig, opts: Options) -> Self {
        let mut builder = aws_sdk_dynamodb::config::Builder::from(cfg);
        if let Some(endpoint) = opts.endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        Self {
            client: aws_sdk_dynamodb::Client::from_conf(builder.build()),
        }
    }

    /// The `Query` request for `q`, reading through `index` when one is named,
    /// exactly as it will be sent. Base-table queries are strongly consistent;
    /// DynamoDB does not allow that on an index, so index queries are
    /// eventually consistent. Empty expression attribute names and values are
    /// left unset.
    ///
    /// # Errors
    ///
    /// Fails when a value or the exclusive start key cannot be encoded.
    fn query_builder(
        &self,
        q: &QuerySpec,
        index: Option<&str>,
    ) -> Result<QueryFluentBuilder, SvcError> {
        let names = (!q.names.is_empty()).then(|| {
            q.names
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<HashMap<_, _>>()
        });
        let values = if q.values.is_empty() {
            None
        } else {
            let mut out = HashMap::with_capacity(q.values.len());
            for (name, value) in &q.values {
                let av: AttributeValue = serde_dynamo::to_attribute_value(value).map_err(|e| {
                    SvcError::Other(anyhow::Error::from(e).context(format!("encode value {name}")))
                })?;
                out.insert(name.clone(), av);
            }
            Some(out)
        };
        let start = q.exclusive_start.as_ref().map(key_to_av).transpose()?;
        Ok(self
            .client
            .query()
            .table_name(&q.table)
            .set_index_name(index.map(str::to_owned))
            .set_consistent_read(index.is_none().then_some(true))
            .key_condition_expression(&q.key_condition)
            .set_expression_attribute_names(names)
            .set_expression_attribute_values(values)
            .set_exclusive_start_key(start)
            .set_limit(q.limit)
            .scan_index_forward(!q.descending))
    }

    /// Runs one `Query` call for `q` and shapes the response.
    async fn run_query(&self, q: &QuerySpec, index: Option<&str>) -> Result<ItemPage, SvcError> {
        let out = self
            .query_builder(q, index)?
            .send()
            .await
            .map_err(|e| svc_error(&q.table, e))?;
        let items = out
            .items()
            .iter()
            .map(|av| item_from_av(av.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let next = match out.last_evaluated_key() {
            Some(key) if !key.is_empty() => Some(item_from_av(key.clone())?),
            _ => None,
        };
        Ok(ItemPage { items, next })
    }
}

#[async_trait]
impl DdbPort for AwsDdb {
    async fn list_tables(&self) -> Result<Vec<String>, SvcError> {
        let mut names = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let out = self
                .client
                .list_tables()
                .set_exclusive_start_table_name(start)
                .send()
                .await
                .map_err(|e| svc_error("tables", e))?;
            names.extend(out.table_names().iter().cloned());
            start = out.last_evaluated_table_name().map(str::to_owned);
            if start.is_none() {
                break;
            }
        }
        names.sort();
        Ok(names)
    }

    async fn describe_table(&self, table: &str) -> Result<TableInfo, SvcError> {
        let out = self
            .client
            .describe_table()
            .table_name(table)
            .send()
            .await
            .map_err(|e| svc_error(table, e))?;
        let desc = out
            .table()
            .ok_or_else(|| SvcError::Other(anyhow!("{table}: the response described no table")))?;
        Ok(table_info_from(desc))
    }

    async fn get_item(&self, table: &str, key: &Key) -> Result<Item, SvcError> {
        let what = format!("{table} {}", key_string(key));
        let out = self
            .client
            .get_item()
            .table_name(table)
            .set_key(Some(key_to_av(key)?))
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| svc_error(&what, e))?;
        match out.item {
            Some(av) if !av.is_empty() => item_from_av(av),
            _ => Err(SvcError::not_found(what)),
        }
    }

    async fn query(&self, q: &QuerySpec) -> Result<ItemPage, SvcError> {
        self.run_query(q, None).await
    }

    async fn query_index(
        &self,
        table: &str,
        index: &str,
        q: &QuerySpec,
    ) -> Result<ItemPage, SvcError> {
        let spec = QuerySpec {
            table: table.to_owned(),
            ..q.clone()
        };
        self.run_query(&spec, Some(index)).await
    }

    /// The item is read first, so that what the change expects is compared
    /// with the stored values as DynamoDB holds them (a set stays a set),
    /// and the write is made conditional on those same stored values.
    async fn update_item(
        &self,
        table: &str,
        key: &Key,
        change: &ItemChange,
    ) -> Result<(), SvcError> {
        change.check(key)?;
        let what = format!("{table} {}", key_string(key));
        let key_av = key_to_av(key)?;
        let stored = self
            .client
            .get_item()
            .table_name(table)
            .set_key(Some(key_av.clone()))
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| svc_error(&what, e))?
            .item
            .filter(|item| !item.is_empty())
            .ok_or_else(|| SvcError::not_found(what.clone()))?;
        let request = update_request(&what, key, change, &stored)?;
        self.client
            .update_item()
            .table_name(table)
            .set_key(Some(key_av))
            .update_expression(request.update)
            .condition_expression(request.condition)
            .set_expression_attribute_names(Some(request.names))
            .set_expression_attribute_values((!request.values.is_empty()).then_some(request.values))
            .send()
            .await
            .map_err(|e| {
                // The item changed, or went, between the read and the write.
                if e.code() == Some("ConditionalCheckFailedException") {
                    SvcError::changed(what.clone())
                } else {
                    svc_error(&what, e)
                }
            })?;
        Ok(())
    }
}

/// One conditional `UpdateItem` request, as it will be sent.
#[derive(Debug)]
struct UpdateRequest {
    /// The update expression: a `SET` clause, a `REMOVE` clause, or both.
    update: String,
    /// The condition: the item exists and holds what the change expects.
    condition: String,
    /// Every attribute name the expressions use, by placeholder.
    names: HashMap<String, String>,
    /// Every value the expressions use, by placeholder.
    values: HashMap<String, AttributeValue>,
}

/// The request that applies `change` to the item `what` names, at `key`,
/// which holds `stored`. An expected value is compared in the condition as
/// it is stored, not as JSON renders it.
///
/// # Errors
///
/// [`SvcError::Changed`] when `stored` no longer holds what the change
/// expects. [`SvcError::Other`] when an attribute to set holds a set or
/// binary data, which JSON cannot express, so that writing it back would
/// change its type; or when a value cannot be encoded.
fn update_request(
    what: &str,
    key: &Key,
    change: &ItemChange,
    stored: &HashMap<String, AttributeValue>,
) -> Result<UpdateRequest, SvcError> {
    let rendered = item_from_av(stored.clone())?;
    if let Some(name) = change.first_broken_expectation(&rendered) {
        return Err(SvcError::changed(format!("{what}: attribute {name}")));
    }
    let mut names = HashMap::new();
    let mut values = HashMap::new();
    let mut conditions = Vec::new();
    // Every item has its key attributes, so one of them being there means
    // the item is.
    if let Some(name) = key.keys().next() {
        names.insert("#k0".to_owned(), name.clone());
        conditions.push("attribute_exists(#k0)".to_owned());
    }
    let mut sets = Vec::new();
    for (i, (name, value)) in change.set.iter().enumerate() {
        if let Some(kind) = stored.get(name).and_then(not_json) {
            return Err(SvcError::Other(anyhow!(
                "{what}: attribute {name} holds {kind}, which cannot be written back from JSON"
            )));
        }
        let av: AttributeValue = serde_dynamo::to_attribute_value(value).map_err(|e| {
            SvcError::Other(anyhow::Error::from(e).context(format!("encode attribute {name}")))
        })?;
        names.insert(format!("#s{i}"), name.clone());
        values.insert(format!(":s{i}"), av);
        sets.push(format!("#s{i} = :s{i}"));
    }
    let mut removes = Vec::new();
    for (i, name) in change.remove.iter().enumerate() {
        names.insert(format!("#r{i}"), name.clone());
        removes.push(format!("#r{i}"));
    }
    for (i, name) in change.expect.keys().enumerate() {
        names.insert(format!("#e{i}"), name.clone());
        match stored.get(name) {
            Some(av) => {
                values.insert(format!(":e{i}"), av.clone());
                conditions.push(format!("#e{i} = :e{i}"));
            }
            None => conditions.push(format!("attribute_not_exists(#e{i})")),
        }
    }
    let mut clauses = Vec::new();
    if !sets.is_empty() {
        clauses.push(format!("SET {}", sets.join(", ")));
    }
    if !removes.is_empty() {
        clauses.push(format!("REMOVE {}", removes.join(", ")));
    }
    Ok(UpdateRequest {
        update: clauses.join(" "),
        condition: conditions.join(" AND "),
        names,
        values,
    })
}

/// What `av` holds, at any depth, that JSON cannot express: a set or binary
/// data. `None` when the whole value can be written back from JSON.
fn not_json(av: &AttributeValue) -> Option<&'static str> {
    match av {
        AttributeValue::Ss(_) => Some("a string set"),
        AttributeValue::Ns(_) => Some("a number set"),
        AttributeValue::Bs(_) => Some("a binary set"),
        AttributeValue::B(_) => Some("binary data"),
        AttributeValue::L(list) => list.iter().find_map(not_json),
        AttributeValue::M(map) => map.values().find_map(not_json),
        _ => None,
    }
}

/// Converts an SDK failure into a [`SvcError`], naming `what` when it is a
/// not-found.
fn svc_error<E>(what: &str, err: SdkError<E, HttpResponse>) -> SvcError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    if is_not_found(err.code()) {
        SvcError::not_found_from(what, err)
    } else {
        SvcError::Other(anyhow::Error::from(err))
    }
}

/// Whether a DynamoDB error code means the table or index does not exist.
fn is_not_found(code: Option<&str>) -> bool {
    code == Some("ResourceNotFoundException")
}

/// Unmarshals an attribute map into plain JSON, with every number passed
/// through [`jsonutil::normalize`] so it compares as an `f64`.
///
/// Binary values (`B`) become standard-alphabet, padded base64 strings and
/// binary sets (`BS`) arrays of them. String and number sets (`SS`, `NS`)
/// become plain arrays, so a set is indistinguishable from a list afterwards.
fn item_from_av(av: HashMap<String, AttributeValue>) -> Result<Item, SvcError> {
    let mut item = Item::with_capacity(av.len());
    for (name, value) in av {
        let json = json_from_av(value)
            .map_err(|e| SvcError::Other(e.context(format!("decode attribute {name}"))))?;
        item.insert(name, json);
    }
    item.values_mut().for_each(jsonutil::normalize);
    Ok(item)
}

/// One attribute value as JSON; see [`item_from_av`] for the mapping.
fn json_from_av(av: AttributeValue) -> anyhow::Result<Value> {
    let number = |n: &str| {
        n.parse::<Number>()
            .map(Value::Number)
            .map_err(|e| anyhow::Error::from(e).context(format!("number {n:?}")))
    };
    let base64 = |b: &Blob| Value::String(STANDARD.encode(b.as_ref()));
    Ok(match av {
        AttributeValue::S(s) => Value::String(s),
        AttributeValue::N(n) => number(&n)?,
        AttributeValue::B(b) => base64(&b),
        AttributeValue::Bool(b) => Value::Bool(b),
        AttributeValue::Null(_) => Value::Null,
        AttributeValue::Ss(set) => set.into_iter().map(Value::String).collect(),
        AttributeValue::Ns(set) => set
            .iter()
            .map(|n| number(n))
            .collect::<anyhow::Result<Value>>()?,
        AttributeValue::Bs(set) => set.iter().map(base64).collect(),
        AttributeValue::L(list) => list
            .into_iter()
            .map(json_from_av)
            .collect::<anyhow::Result<Value>>()?,
        AttributeValue::M(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| Ok((k, json_from_av(v)?)))
                .collect::<anyhow::Result<serde_json::Map<_, _>>>()?,
        ),
        other => return Err(anyhow!("unsupported attribute value {other:?}")),
    })
}

/// Marshals a key into an attribute map: JSON numbers become `N`, strings `S`.
fn key_to_av(key: &Key) -> Result<HashMap<String, AttributeValue>, SvcError> {
    serde_dynamo::to_item(key)
        .map_err(|e| SvcError::Other(anyhow::Error::from(e).context("encode key")))
}

/// The key schema, key attribute types and global secondary indexes of a
/// described table. A key attribute whose scalar type is not `S`, `N` or `B`
/// is left out of `attr_types`.
fn table_info_from(table: &TableDescription) -> TableInfo {
    let (hash_key, sort_key) = key_names(table.key_schema());
    let attr_types = table
        .attribute_definitions()
        .iter()
        .filter_map(|def| {
            let kind = match def.attribute_type() {
                ScalarAttributeType::S => AttrType::S,
                ScalarAttributeType::N => AttrType::N,
                ScalarAttributeType::B => AttrType::B,
                _ => return None,
            };
            Some((def.attribute_name().to_owned(), kind))
        })
        .collect();
    let gsis = table
        .global_secondary_indexes()
        .iter()
        .map(|gsi| {
            let (hash_key, sort_key) = key_names(gsi.key_schema());
            IndexInfo {
                name: gsi.index_name().unwrap_or_default().to_owned(),
                hash_key,
                sort_key,
                projection: gsi.projection().and_then(|p| p.projection_type()).and_then(
                    |t| match t {
                        ProjectionType::All => Some(Projection::All),
                        ProjectionType::KeysOnly => Some(Projection::KeysOnly),
                        ProjectionType::Include => Some(Projection::Include),
                        _ => None,
                    },
                ),
            }
        })
        .collect();
    TableInfo {
        name: table.table_name().unwrap_or_default().to_owned(),
        hash_key,
        sort_key,
        attr_types,
        gsis,
    }
}

/// The partition-key and sort-key attribute names of a key schema.
fn key_names(schema: &[aws_sdk_dynamodb::types::KeySchemaElement]) -> (String, Option<String>) {
    let named = |kind: KeyType| {
        schema
            .iter()
            .find(|e| *e.key_type() == kind)
            .map(|e| e.attribute_name().to_owned())
    };
    (
        named(KeyType::Hash).unwrap_or_default(),
        named(KeyType::Range),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    #[test]
    fn the_only_write_the_adapter_calls_is_one_update_of_an_item() {
        let production = include_str!("ddb.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("split yields at least one part");
        assert!(
            production.contains("impl DdbPort for AwsDdb"),
            "the production half of the adapter must hold the port implementation"
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
                "the adapter source mentions {needle}"
            );
        }
        assert_eq!(
            production.matches(concat!(".update", "_item()")).count(),
            1,
            "the adapter sends exactly one kind of write"
        );
        assert!(
            production.contains(".condition_expression("),
            "the write is conditional"
        );
    }

    fn object(value: Value) -> Item {
        match value {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other}"),
        }
    }

    fn a_change(set: Value, remove: &[&str], expect: &[(&str, Option<Value>)]) -> ItemChange {
        ItemChange {
            set: object(set),
            remove: remove.iter().map(|name| (*name).to_owned()).collect(),
            expect: expect
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        }
    }

    fn stored(attributes: &[(&str, AttributeValue)]) -> HashMap<String, AttributeValue> {
        attributes
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect()
    }

    fn s(text: &str) -> AttributeValue {
        AttributeValue::S(text.to_owned())
    }

    fn n(text: &str) -> AttributeValue {
        AttributeValue::N(text.to_owned())
    }

    const WHAT: &str = "tenants TenantId=acme";

    fn tenant_key() -> Key {
        object(json!({"TenantId": "acme"}))
    }

    #[test]
    fn an_update_names_every_attribute_through_a_placeholder() {
        let held = stored(&[
            ("TenantId", s("acme")),
            ("name", s("old")),
            ("size", n("3")),
        ]);
        let change = a_change(
            json!({"name": "new", "a b.c": 1}),
            &["size"],
            &[
                ("name", Some(json!("old"))),
                ("a b.c", None),
                ("size", Some(json!(3))),
            ],
        );

        let request = update_request(WHAT, &tenant_key(), &change, &held).expect("request");

        assert_eq!(request.update, "SET #s0 = :s0, #s1 = :s1 REMOVE #r0");
        assert_eq!(
            request.condition,
            "attribute_exists(#k0) AND attribute_not_exists(#e0) AND #e1 = :e1 AND #e2 = :e2"
        );
        let mut names: Vec<(&str, &str)> = request
            .names
            .iter()
            .map(|(placeholder, name)| (placeholder.as_str(), name.as_str()))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                ("#e0", "a b.c"),
                ("#e1", "name"),
                ("#e2", "size"),
                ("#k0", "TenantId"),
                ("#r0", "size"),
                ("#s0", "a b.c"),
                ("#s1", "name"),
            ]
        );
        assert_eq!(request.values.get(":s0"), Some(&n("1")));
        assert_eq!(request.values.get(":s1"), Some(&s("new")));
        // What is expected is compared as it is stored.
        assert_eq!(request.values.get(":e1"), Some(&s("old")));
        assert_eq!(request.values.get(":e2"), Some(&n("3")));
        assert_eq!(request.values.len(), 4);
    }

    #[test]
    fn an_update_that_only_removes_has_no_set_clause() {
        let held = stored(&[("TenantId", s("acme"))]);
        let change = a_change(json!({}), &["gone"], &[("gone", None)]);

        let request = update_request(WHAT, &tenant_key(), &change, &held).expect("request");

        assert_eq!(request.update, "REMOVE #r0");
        assert_eq!(
            request.condition,
            "attribute_exists(#k0) AND attribute_not_exists(#e0)"
        );
        assert!(request.values.is_empty());
    }

    #[test]
    fn an_expectation_the_stored_item_no_longer_meets_is_a_change() {
        let held = stored(&[("TenantId", s("acme")), ("name", s("old"))]);

        for expect in [
            ("name", Some(json!("other"))),
            ("name", None),
            ("note", Some(json!("x"))),
        ] {
            let change = a_change(json!({"name": "new"}), &[], std::slice::from_ref(&expect));
            let err = update_request(WHAT, &tenant_key(), &change, &held)
                .expect_err("the item is not as expected");
            assert!(err.is_changed(), "{expect:?}: {err}");
            assert!(err.to_string().contains(WHAT), "{err}");
        }
    }

    #[test]
    fn an_expected_number_matches_the_stored_one_whatever_its_spelling() {
        let held = stored(&[("TenantId", s("acme")), ("rps", n("1.0"))]);
        let change = a_change(json!({"rps": 2}), &[], &[("rps", Some(json!(1)))]);

        let request = update_request(WHAT, &tenant_key(), &change, &held).expect("request");

        assert_eq!(request.values.get(":e0"), Some(&n("1.0")));
    }

    #[test]
    fn a_whole_number_is_written_with_its_own_digits() {
        let held = stored(&[("TenantId", s("acme"))]);
        let change = a_change(
            json!({"big": 9_007_199_254_740_993_u64, "neg": -42}),
            &[],
            &[],
        );

        let request = update_request(WHAT, &tenant_key(), &change, &held).expect("request");

        assert_eq!(request.values.get(":s0"), Some(&n("9007199254740993")));
        assert_eq!(request.values.get(":s1"), Some(&n("-42")));
    }

    #[test]
    fn a_stored_set_is_compared_as_stored_and_cannot_be_set_from_json() {
        let tags = AttributeValue::Ss(vec!["a".to_owned(), "b".to_owned()]);
        let nested = AttributeValue::M(
            [(
                "raw".to_owned(),
                AttributeValue::B(Blob::new(b"\x00\x01".to_vec())),
            )]
            .into(),
        );
        let held = stored(&[
            ("TenantId", s("acme")),
            ("tags", tags.clone()),
            ("blob", nested),
        ]);

        // Removing it, or expecting it while changing something else, works
        // on the set itself.
        let remove = a_change(json!({}), &["tags"], &[("tags", Some(json!(["a", "b"])))]);
        let request = update_request(WHAT, &tenant_key(), &remove, &held).expect("request");
        assert_eq!(request.values.get(":e0"), Some(&tags));

        // Writing JSON over it would turn the set into a list.
        for (name, kind) in [("tags", "a string set"), ("blob", "binary data")] {
            let change = a_change(json!({name: ["a"]}), &[], &[]);
            let err = update_request(WHAT, &tenant_key(), &change, &held)
                .expect_err("JSON cannot say what is stored");
            assert!(!err.is_changed() && !err.is_not_found(), "{err}");
            assert!(
                err.to_string().contains(name) && err.to_string().contains(kind),
                "{err}"
            );
        }
    }

    use aws_sdk_dynamodb::primitives::Blob;
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, AttributeValue, GlobalSecondaryIndexDescription, KeySchemaElement,
        KeyType, Projection as SdkProjection, ProjectionType, ScalarAttributeType,
        TableDescription,
    };
    use serde_json::{Value, json};
    use ss_core::jsonutil;
    use ss_core::svc::ddb::{
        AttrType, IndexInfo, Item, ItemChange, Key, Projection, QuerySpec, TableInfo,
    };

    use super::*;

    fn obj(v: Value) -> Item {
        match v {
            Value::Object(m) => m,
            other => panic!("expected an object, got {other}"),
        }
    }

    #[test]
    fn classify_not_found() {
        assert!(is_not_found(Some("ResourceNotFoundException")));
        for code in [
            Some("AccessDeniedException"),
            Some("ValidationException"),
            None,
        ] {
            assert!(!is_not_found(code), "{code:?}");
        }
    }

    #[test]
    fn items_unmarshal_to_plain_json_with_f64_numbers() {
        let av: HashMap<String, AttributeValue> = HashMap::from([
            ("TenantId".to_owned(), AttributeValue::S("acme".into())),
            ("rps".to_owned(), AttributeValue::N("200".into())),
            ("on".to_owned(), AttributeValue::Bool(true)),
            ("gone".to_owned(), AttributeValue::Null(true)),
            (
                "list".to_owned(),
                AttributeValue::L(vec![
                    AttributeValue::N("1".into()),
                    AttributeValue::S("x".into()),
                ]),
            ),
            (
                "nested".to_owned(),
                AttributeValue::M(HashMap::from([(
                    "n".to_owned(),
                    AttributeValue::N("2.5".into()),
                )])),
            ),
        ]);
        let item = item_from_av(av).expect("unmarshal");
        assert_eq!(item["rps"].as_f64(), Some(200.0));
        let mut want = json!({
            "TenantId": "acme", "rps": 200.0, "on": true, "gone": null,
            "list": [1.0, "x"], "nested": {"n": 2.5}
        });
        jsonutil::normalize(&mut want);
        assert_eq!(Value::Object(item), want);
    }

    #[test]
    fn keys_marshal_numbers_as_numbers_and_strings_as_strings() {
        let key: Key = obj(json!({"Id": 42.0, "Name": "n"}));
        let av = key_to_av(&key).expect("marshal");
        assert_eq!(av["Id"], AttributeValue::N("42".into()));
        assert_eq!(av["Name"], AttributeValue::S("n".into()));
    }

    fn key_element(name: &str, kind: KeyType) -> KeySchemaElement {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(kind)
            .build()
            .expect("key element")
    }

    fn attr(name: &str, kind: ScalarAttributeType) -> AttributeDefinition {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(kind)
            .build()
            .expect("attribute definition")
    }

    #[test]
    fn table_info_maps_key_schema_attribute_types_and_gsis() {
        let table = TableDescription::builder()
            .table_name("tenants")
            .key_schema(key_element("TenantId", KeyType::Hash))
            .key_schema(key_element("Version", KeyType::Range))
            .attribute_definitions(attr("TenantId", ScalarAttributeType::S))
            .attribute_definitions(attr("Version", ScalarAttributeType::S))
            .attribute_definitions(attr("Tier", ScalarAttributeType::S))
            .global_secondary_indexes(
                GlobalSecondaryIndexDescription::builder()
                    .index_name("TierIndex")
                    .key_schema(key_element("Tier", KeyType::Hash))
                    .projection(
                        SdkProjection::builder()
                            .projection_type(ProjectionType::All)
                            .build(),
                    )
                    .build(),
            )
            .build();
        let info = table_info_from(&table);
        assert_eq!(
            info,
            TableInfo {
                name: "tenants".into(),
                hash_key: "TenantId".into(),
                sort_key: Some("Version".into()),
                attr_types: [
                    ("TenantId".to_owned(), AttrType::S),
                    ("Version".to_owned(), AttrType::S),
                    ("Tier".to_owned(), AttrType::S),
                ]
                .into(),
                gsis: vec![IndexInfo {
                    name: "TierIndex".into(),
                    hash_key: "Tier".into(),
                    sort_key: None,
                    projection: Some(Projection::All),
                }],
            }
        );
    }

    /// An adapter whose client is configured but never connects.
    fn offline_port() -> AwsDdb {
        let cfg = aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .build();
        AwsDdb::new(&cfg, Options::default())
    }

    fn tenants_spec() -> QuerySpec {
        QuerySpec {
            table: "tenants".into(),
            key_condition: "#pk = :pk".into(),
            names: [("#pk".to_owned(), "TenantId".to_owned())].into(),
            values: [(":pk".to_owned(), json!("acme"))].into(),
            limit: Some(2),
            descending: true,
            exclusive_start: Some(obj(json!({"TenantId": "acme", "Version": "V1"}))),
        }
    }

    #[test]
    fn an_index_query_is_sent_eventually_consistent_with_every_mapped_field() {
        let port = offline_port();
        let sent = port
            .query_builder(&tenants_spec(), Some("TierIndex"))
            .expect("request");
        let q = sent.as_input();
        assert_eq!(q.get_table_name().as_deref(), Some("tenants"));
        assert_eq!(q.get_index_name().as_deref(), Some("TierIndex"));
        assert_eq!(q.get_consistent_read(), &None);
        assert_eq!(q.get_scan_index_forward(), &Some(false));
        assert_eq!(q.get_limit(), &Some(2));
        assert_eq!(
            q.get_key_condition_expression().as_deref(),
            Some("#pk = :pk")
        );
        assert_eq!(
            q.get_expression_attribute_names()
                .as_ref()
                .and_then(|m| m.get("#pk")),
            Some(&"TenantId".to_owned())
        );
        assert_eq!(
            q.get_expression_attribute_values()
                .as_ref()
                .and_then(|m| m.get(":pk")),
            Some(&AttributeValue::S("acme".into()))
        );
        let start = q.get_exclusive_start_key().as_ref().expect("start key");
        assert_eq!(start["Version"], AttributeValue::S("V1".into()));
    }

    #[test]
    fn a_base_table_query_is_sent_strongly_consistent_without_empty_expressions() {
        let port = offline_port();
        let spec = QuerySpec {
            names: Default::default(),
            values: Default::default(),
            exclusive_start: None,
            descending: false,
            limit: None,
            ..tenants_spec()
        };
        let sent = port.query_builder(&spec, None).expect("request");
        let q = sent.as_input();
        assert_eq!(q.get_table_name().as_deref(), Some("tenants"));
        assert_eq!(q.get_index_name(), &None);
        assert_eq!(q.get_consistent_read(), &Some(true));
        assert_eq!(q.get_scan_index_forward(), &Some(true));
        assert_eq!(q.get_limit(), &None);
        assert!(q.get_expression_attribute_names().is_none());
        assert!(q.get_expression_attribute_values().is_none());
        assert!(q.get_exclusive_start_key().is_none());
    }

    #[test]
    fn items_with_binary_attributes_decode_as_base64() {
        let blob = |bytes: &[u8]| AttributeValue::B(Blob::new(bytes.to_vec()));
        let av: HashMap<String, AttributeValue> = HashMap::from([
            ("payload".to_owned(), blob(b"hello")),
            (
                "blobs".to_owned(),
                AttributeValue::Bs(vec![Blob::new(b"a".to_vec()), Blob::new(b"bc".to_vec())]),
            ),
            (
                "list".to_owned(),
                AttributeValue::L(vec![blob(&[0xff, 0xfe])]),
            ),
            (
                "nested".to_owned(),
                AttributeValue::M(HashMap::from([("b".to_owned(), blob(b""))])),
            ),
        ]);
        let item = item_from_av(av).expect("unmarshal");
        assert_eq!(
            Value::Object(item),
            json!({
                "payload": "aGVsbG8=",
                "blobs": ["YQ==", "YmM="],
                "list": ["//4="],
                "nested": {"b": ""}
            })
        );
    }

    #[test]
    fn string_and_number_sets_decode_as_arrays() {
        let av: HashMap<String, AttributeValue> = HashMap::from([
            (
                "tags".to_owned(),
                AttributeValue::Ss(vec!["a".into(), "b".into()]),
            ),
            (
                "sizes".to_owned(),
                AttributeValue::Ns(vec!["1".into(), "2.5".into()]),
            ),
        ]);
        let item = item_from_av(av).expect("unmarshal");
        let mut want = json!({"tags": ["a", "b"], "sizes": [1.0, 2.5]});
        jsonutil::normalize(&mut want);
        assert_eq!(Value::Object(item), want);
    }
}

/// The adapter against a real DynamoDB-compatible endpoint, selected with
/// `SS_SUPPORT_DDB_TEST_ENDPOINT`; every test is skipped when it is unset.
#[cfg(test)]
mod dynalite {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use aws_config::{BehaviorVersion, Region, SdkConfig};
    use aws_sdk_dynamodb::config::Credentials;
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, AttributeValue, GlobalSecondaryIndex, KeySchemaElement, KeyType,
        Projection, ProjectionType, ProvisionedThroughput, ScalarAttributeType, TableStatus,
    };
    use serde_json::{Value, json};
    use ss_core::svc::ddb::contract;
    use ss_core::svc::ddb::{AttrType, DdbPort, Item};

    use super::{AwsDdb, Options};

    /// How long a created table may take to become active.
    const ACTIVE_TIMEOUT: Duration = Duration::from_secs(60);

    /// An SDK configuration and raw client pointed at the test endpoint.
    struct Env {
        cfg: SdkConfig,
        endpoint: String,
        raw: aws_sdk_dynamodb::Client,
        nanos: u128,
    }

    impl Env {
        fn port(&self) -> AwsDdb {
            AwsDdb::new(
                &self.cfg,
                Options {
                    endpoint: Some(self.endpoint.clone()),
                },
            )
        }
    }

    fn env_or(name: &str, default: &str) -> String {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_owned())
    }

    /// Connects to the configured endpoint, or returns `None` when none is
    /// set.
    async fn connect() -> Option<Env> {
        let Some(endpoint) = std::env::var("SS_SUPPORT_DDB_TEST_ENDPOINT")
            .ok()
            .filter(|v| !v.is_empty())
        else {
            eprintln!("skipping: SS_SUPPORT_DDB_TEST_ENDPOINT is not set");
            return None;
        };
        let creds = Credentials::new(
            env_or("SS_SUPPORT_DDB_TEST_KEY", "test"),
            env_or("SS_SUPPORT_DDB_TEST_SECRET", "test"),
            None,
            None,
            "static",
        );
        let cfg = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(creds)
            .load()
            .await;
        let raw = aws_sdk_dynamodb::Client::from_conf(
            aws_sdk_dynamodb::config::Builder::from(&cfg)
                .endpoint_url(&endpoint)
                .build(),
        );
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        Some(Env {
            cfg,
            endpoint,
            raw,
            nanos,
        })
    }

    fn key_element(name: &str, kind: KeyType) -> KeySchemaElement {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(kind)
            .build()
            .expect("key schema element")
    }

    fn definition(name: &str, kind: ScalarAttributeType) -> AttributeDefinition {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(kind)
            .build()
            .expect("attribute definition")
    }

    fn throughput() -> ProvisionedThroughput {
        ProvisionedThroughput::builder()
            .read_capacity_units(5)
            .write_capacity_units(5)
            .build()
            .expect("provisioned throughput")
    }

    /// Polls until the table and all of its indexes are active.
    async fn wait_active(raw: &aws_sdk_dynamodb::Client, table: &str) {
        let deadline = tokio::time::Instant::now() + ACTIVE_TIMEOUT;
        loop {
            let out = raw
                .describe_table()
                .table_name(table)
                .send()
                .await
                .expect("describe table while waiting");
            let desc = out.table().expect("described table");
            let table_active = desc.table_status() == Some(&TableStatus::Active);
            let indexes_active = desc
                .global_secondary_indexes()
                .iter()
                .all(|g| g.index_status().is_some_and(|s| s.as_str() == "ACTIVE"));
            if table_active && indexes_active {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "table {table} did not become active"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Creates the contract table (with its `TierIndex`) and seeds it.
    async fn create_contract_table(env: &Env, table: &str) {
        let info = contract::table_info(table);
        let mut create = env
            .raw
            .create_table()
            .table_name(table)
            .key_schema(key_element(&info.hash_key, KeyType::Hash))
            .provisioned_throughput(throughput());
        if let Some(sort) = &info.sort_key {
            create = create.key_schema(key_element(sort, KeyType::Range));
        }
        for (name, kind) in &info.attr_types {
            let scalar = match kind {
                AttrType::S => ScalarAttributeType::S,
                AttrType::N => ScalarAttributeType::N,
                AttrType::B => ScalarAttributeType::B,
            };
            create = create.attribute_definitions(definition(name, scalar));
        }
        for gsi in &info.gsis {
            let mut index = GlobalSecondaryIndex::builder()
                .index_name(&gsi.name)
                .key_schema(key_element(&gsi.hash_key, KeyType::Hash))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .provisioned_throughput(throughput());
            if let Some(sort) = &gsi.sort_key {
                index = index.key_schema(key_element(sort, KeyType::Range));
            }
            create = create.global_secondary_indexes(index.build().expect("global index"));
        }
        create.send().await.expect("create contract table");
        wait_active(&env.raw, table).await;

        for item in contract::seed_items() {
            put(&env.raw, table, &item).await;
        }
    }

    /// Writes `item` through the raw client; the port under test never writes.
    async fn put(raw: &aws_sdk_dynamodb::Client, table: &str, item: &Item) {
        let av: std::collections::HashMap<String, aws_sdk_dynamodb::types::AttributeValue> =
            serde_dynamo::to_item(item).expect("marshal item");
        raw.put_item()
            .table_name(table)
            .set_item(Some(av))
            .send()
            .await
            .expect("put item");
    }

    #[tokio::test]
    async fn contract_holds_against_dynalite() {
        let Some(env) = connect().await else { return };
        let table = format!("ss-it-{}", env.nanos);
        create_contract_table(&env, &table).await;

        contract::run(&env.port(), &table, &format!("ss-it-missing-{}", env.nanos)).await;
    }

    #[tokio::test]
    async fn write_contract_holds_against_dynalite() {
        let Some(env) = connect().await else { return };
        let table = format!("ss-it-writes-{}", env.nanos);
        create_contract_table(&env, &table).await;
        contract::run_writes(&env.port(), &table, &format!("ss-it-missing-{}", env.nanos)).await;
    }

    #[tokio::test]
    async fn a_stored_set_stays_a_set_when_another_attribute_is_edited() {
        let Some(env) = connect().await else { return };
        let table = format!("ss-it-sets-{}", env.nanos);
        env.raw
            .create_table()
            .table_name(&table)
            .key_schema(key_element("Id", KeyType::Hash))
            .attribute_definitions(definition("Id", ScalarAttributeType::S))
            .provisioned_throughput(throughput())
            .send()
            .await
            .expect("create table");
        wait_active(&env.raw, &table).await;
        let tags = AttributeValue::Ss(vec!["a".to_owned(), "b".to_owned()]);
        env.raw
            .put_item()
            .table_name(&table)
            .item("Id", AttributeValue::S("one".to_owned()))
            .item("tags", tags.clone())
            .item("note", AttributeValue::S("old".to_owned()))
            .send()
            .await
            .expect("put an item with a set");
        let Value::Object(key) = json!({"Id": "one"}) else {
            unreachable!("a JSON object literal is an object")
        };
        let port = env.port();
        let read = port.get_item(&table, &key).await.expect("item");

        let Value::Object(set) = json!({"note": "new"}) else {
            unreachable!("a JSON object literal is an object")
        };
        let change = ss_core::svc::ddb::ItemChange {
            set,
            remove: Vec::new(),
            expect: [
                ("note".to_owned(), read.get("note").cloned()),
                ("tags".to_owned(), read.get("tags").cloned()),
            ]
            .into(),
        };
        port.update_item(&table, &key, &change)
            .await
            .expect("the set is as it was read");

        let after = env
            .raw
            .get_item()
            .table_name(&table)
            .key("Id", AttributeValue::S("one".to_owned()))
            .consistent_read(true)
            .send()
            .await
            .expect("get")
            .item
            .expect("the item is there");
        assert_eq!(
            after.get("note"),
            Some(&AttributeValue::S("new".to_owned()))
        );
        assert!(
            matches!(after.get("tags"), Some(AttributeValue::Ss(set)) if set.len() == 2),
            "{:?}",
            after.get("tags")
        );
    }

    #[tokio::test]
    async fn numeric_keys_are_looked_up_as_numbers() {
        let Some(env) = connect().await else { return };
        let table = format!("ss-it-num-{}", env.nanos);
        env.raw
            .create_table()
            .table_name(&table)
            .key_schema(key_element("Id", KeyType::Hash))
            .attribute_definitions(definition("Id", ScalarAttributeType::N))
            .provisioned_throughput(throughput())
            .send()
            .await
            .expect("create numeric table");
        wait_active(&env.raw, &table).await;

        let Value::Object(item) = json!({"Id": 42, "name": "n"}) else {
            unreachable!("a JSON object literal is an object")
        };
        put(&env.raw, &table, &item).await;

        let port = env.port();
        let Value::Object(key) = json!({"Id": 42.0}) else {
            unreachable!("a JSON object literal is an object")
        };
        let found = port
            .get_item(&table, &key)
            .await
            .expect("a numeric key finds the item");
        assert_eq!(found["name"], json!("n"));

        let info = port.describe_table(&table).await.expect("describe");
        assert_eq!(info.attr_types.get("Id"), Some(&AttrType::N));
    }
}
