//! Shared test data for the cross-service operations: one menu record in
//! DynamoDB pointing at an S3 object whose current version differs from the
//! ConfigMap deployed in the cluster.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use crate::config::{DiffDefaults, Environment, TraceConfig, TraceDomain};
use crate::ops::Deps;
use crate::svc::ddb::{AttrType, FakeDdb, Item, TableInfo};
use crate::svc::kubectl::FakeKubectl;
use crate::svc::s3::FakeS3;

/// The bucket the menu object lives in.
pub(crate) const BUCKET: &str = "ss-config-dev";

/// The object key the menu record points at.
pub(crate) const KEY: &str = "menu/12345.json";

/// The table holding the menu records.
pub(crate) const TABLE: &str = "ss-menu-dev";

/// The ConfigMap deployed for the menu domain.
pub(crate) const CONFIGMAP: &str = "menu-config";

/// The time of the oldest object version.
pub(crate) fn t0() -> DateTime<Utc> {
    "2026-08-01T10:00:00Z".parse().expect("valid timestamp")
}

/// The three fakes behind the scenario, kept apart so a test can alter them
/// before building [`Deps`] with [`deps_from`].
pub(crate) struct Fakes {
    /// The bucket: `menu/12345.json` at `ver-1` and `ver-2`.
    pub(crate) s3: FakeS3,
    /// The table: one record pointing at `ver-2`.
    pub(crate) ddb: FakeDdb,
    /// The cluster: the `menu-config` ConfigMap in namespace `menu`.
    pub(crate) kubectl: FakeKubectl,
}

/// A fresh set of fakes holding the scenario's data.
pub(crate) fn fakes() -> Fakes {
    let s3 = FakeS3::new();
    s3.insert(
        BUCKET,
        KEY,
        "ver-1",
        r#"{"items":[{"id":"latte","price":5}],"updatedAt":"a"}"#,
        t0(),
    );
    s3.insert(
        BUCKET,
        KEY,
        "ver-2",
        r#"{"items":[{"id":"latte","price":6},{"id":"mocha","price":7}],"updatedAt":"b"}"#,
        t0() + Duration::hours(1),
    );

    let ddb = FakeDdb::new();
    ddb.create_table(TableInfo {
        name: TABLE.to_owned(),
        hash_key: "storeNumber".to_owned(),
        sort_key: Some("localeChannel".to_owned()),
        attr_types: BTreeMap::from([
            ("storeNumber".to_owned(), AttrType::S),
            ("localeChannel".to_owned(), AttrType::S),
        ]),
        gsis: Vec::new(),
    });
    let Value::Object(record) = json!({
        "storeNumber": "12345",
        "localeChannel": "en-US#mobile",
        "version": "V0",
        "key": KEY,
        "versionId": "ver-2",
    }) else {
        unreachable!("a JSON object literal is an object")
    };
    ddb.put(TABLE, Item::from(record));

    let kubectl = FakeKubectl::new();
    kubectl.insert(
        "menu",
        "configmap",
        CONFIGMAP,
        r#"{"data":{"menu.json":"{\"items\":[{\"id\":\"latte\",\"price\":5},{\"id\":\"mocha\",\"price\":7}],\"updatedAt\":\"b\"}"}}"#,
    );

    Fakes { s3, ddb, kubectl }
}

/// Dependencies for the `dev` environment backed by `fakes`, with the diff
/// defaults and the `menu` trace domain the operations are tested against.
pub(crate) fn deps_from(fakes: Fakes) -> Deps {
    Deps {
        env: Environment {
            name: "dev".into(),
            ..Environment::default()
        },
        diff: DiffDefaults {
            ignore: vec!["**.updatedAt".into()],
            array_match: BTreeMap::from([("$.items[*]".to_owned(), "id".to_owned())]),
        },
        trace: TraceConfig {
            domains: BTreeMap::from([(
                "menu".to_owned(),
                TraceDomain {
                    table: "ss-menu-{env}".into(),
                    bucket: "ss-config-{env}".into(),
                    hash_attr: "storeNumber".into(),
                    sort_attr: "localeChannel".into(),
                    sort_format: "{locale}#{channel}".into(),
                    labels: vec!["storeNumber".into(), "version".into()],
                    namespace: "menu".into(),
                    ..TraceDomain::default()
                },
            )]),
        },
        s3: Some(Arc::new(fakes.s3)),
        ddb: Some(Arc::new(fakes.ddb)),
        kubectl: Some(Arc::new(fakes.kubectl)),
        ..Deps::default()
    }
}

/// Dependencies over a fresh [`fakes`] scenario.
pub(crate) fn deps() -> Deps {
    deps_from(fakes())
}
