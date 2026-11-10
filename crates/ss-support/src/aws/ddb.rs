//! The DynamoDB port backed by the AWS SDK.
//!
//! A missing table or index becomes [`SvcError::NotFound`]; every other SDK
//! failure is kept whole as [`SvcError::Other`]. The adapter only reads: it
//! never issues a write call.

use std::collections::HashMap;

use anyhow::anyhow;
use async_trait::async_trait;
use aws_config::SdkConfig;
use aws_sdk_dynamodb::config::http::HttpResponse;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::operation::query::QueryInput;
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
    AttrType, DdbPort, IndexInfo, Item, ItemPage, Key, Projection, QuerySpec, TableInfo, key_string,
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

    /// Runs one `Query` call for `input` and shapes the response.
    async fn run_query(&self, input: &QueryInput) -> Result<ItemPage, SvcError> {
        let what = input.table_name().unwrap_or_default().to_owned();
        let out = self
            .client
            .query()
            .set_table_name(input.table_name().map(str::to_owned))
            .set_index_name(input.index_name().map(str::to_owned))
            .set_key_condition_expression(input.key_condition_expression().map(str::to_owned))
            .set_expression_attribute_names(input.expression_attribute_names().cloned())
            .set_expression_attribute_values(input.expression_attribute_values().cloned())
            .set_exclusive_start_key(input.exclusive_start_key().cloned())
            .set_limit(input.limit())
            .set_scan_index_forward(input.scan_index_forward())
            .send()
            .await
            .map_err(|e| svc_error(&what, e))?;
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
        self.run_query(&query_input(q, None)?).await
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
        self.run_query(&query_input(&spec, Some(index))?).await
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

/// The SDK request for `q`, reading through `index` when one is named. Empty
/// expression attribute names and values are left unset.
fn query_input(q: &QuerySpec, index: Option<&str>) -> Result<QueryInput, SvcError> {
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
    QueryInput::builder()
        .table_name(&q.table)
        .set_index_name(index.map(str::to_owned))
        .key_condition_expression(&q.key_condition)
        .set_expression_attribute_names(names)
        .set_expression_attribute_values(values)
        .set_exclusive_start_key(start)
        .set_limit(q.limit)
        .scan_index_forward(!q.descending)
        .build()
        .map_err(|e| SvcError::Other(anyhow::Error::from(e).context("build query")))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use aws_sdk_dynamodb::primitives::Blob;
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, AttributeValue, GlobalSecondaryIndexDescription, KeySchemaElement,
        KeyType, Projection as SdkProjection, ProjectionType, ScalarAttributeType,
        TableDescription,
    };
    use serde_json::{Value, json};
    use ss_core::jsonutil;
    use ss_core::svc::ddb::{AttrType, IndexInfo, Item, Key, Projection, QuerySpec, TableInfo};

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

    #[test]
    fn query_input_maps_the_spec() {
        let spec = QuerySpec {
            table: "tenants".into(),
            key_condition: "#pk = :pk".into(),
            names: [("#pk".to_owned(), "TenantId".to_owned())].into(),
            values: [(":pk".to_owned(), json!("acme"))].into(),
            limit: Some(2),
            descending: true,
            exclusive_start: Some(obj(json!({"TenantId": "acme", "Version": "V1"}))),
        };
        let q = query_input(&spec, Some("TierIndex")).expect("input");
        assert_eq!(q.table_name(), Some("tenants"));
        assert_eq!(q.index_name(), Some("TierIndex"));
        assert_eq!(q.scan_index_forward(), Some(false));
        assert_eq!(q.limit(), Some(2));
        assert_eq!(q.key_condition_expression(), Some("#pk = :pk"));
        assert_eq!(
            q.expression_attribute_names().and_then(|m| m.get("#pk")),
            Some(&"TenantId".to_owned())
        );
        assert_eq!(
            q.expression_attribute_values().and_then(|m| m.get(":pk")),
            Some(&AttributeValue::S("acme".into()))
        );
        let start = q.exclusive_start_key().expect("start key");
        assert_eq!(start["Version"], AttributeValue::S("V1".into()));

        let plain = query_input(
            &QuerySpec {
                names: Default::default(),
                values: Default::default(),
                ..spec
            },
            None,
        )
        .expect("input");
        assert_eq!(plain.index_name(), None);
        assert!(plain.expression_attribute_names().is_none());
        assert!(plain.expression_attribute_values().is_none());
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
