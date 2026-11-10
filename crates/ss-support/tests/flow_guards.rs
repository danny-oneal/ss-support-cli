//! Guards that keep the cross-service operations read-only: every one is
//! classified as a read, and each runs to completion where no environment
//! allows writes and no confirmation can be answered.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use ss_core::guard::Class;
use ss_core::svc::ddb::{AttrType, FakeDdb, Item, TableInfo};
use ss_core::svc::kubectl::FakeKubectl;
use ss_core::svc::s3::FakeS3;
use ss_core::{audit, ops};
use ss_support::{Exit, Io, Services};

const BUCKET: &str = "ss-config-dev";
const KEY: &str = "menu/12345.json";
const TABLE: &str = "ss-menu-dev";

/// A config naming the `menu` record shape, under a default environment that
/// is not on the allowlist.
const CONFIG: &str = "\
default_env: dev
diff:
  ignore: [\"**.updatedAt\"]
  array_match:
    \"$.items[*]\": id
trace:
  domains:
    menu:
      table: \"ss-menu-{env}\"
      bucket: \"ss-config-{env}\"
      hash_attr: storeNumber
      sort_attr: localeChannel
      sort_format: \"{locale}#{channel}\"
      labels: [storeNumber, version]
      namespace: menu
";

fn t0() -> DateTime<Utc> {
    "2026-08-01T10:00:00Z".parse().expect("rfc3339")
}

/// A bucket holding the menu object at two versions.
fn s3() -> FakeS3 {
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
    s3
}

/// A table holding the menu record, which points at the second version.
fn ddb() -> FakeDdb {
    let ddb = FakeDdb::new();
    ddb.create_table(TableInfo {
        name: TABLE.to_owned(),
        hash_key: "storeNumber".to_owned(),
        sort_key: Some("localeChannel".to_owned()),
        attr_types: [
            ("storeNumber".to_owned(), AttrType::S),
            ("localeChannel".to_owned(), AttrType::S),
        ]
        .into(),
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
    ddb
}

/// A cluster holding the `menu-config` ConfigMap.
fn kubectl() -> FakeKubectl {
    let k = FakeKubectl::new();
    k.insert(
        "menu",
        "configmap",
        "menu-config",
        r#"{"data":{"menu.json":"{\"items\":[{\"id\":\"latte\",\"price\":5},{\"id\":\"mocha\",\"price\":7}],\"updatedAt\":\"b\"}"}}"#,
    );
    k
}

/// Services under a home at `root` holding [`CONFIG`], serving the fakes.
fn services(root: &Path) -> Services {
    let config = root.join(".config/ss-support/config.yaml");
    fs::create_dir_all(config.parent().expect("config has a parent")).expect("mkdir");
    fs::write(&config, CONFIG).expect("write config");
    Services::isolated(root)
        .with_s3(Arc::new(s3()))
        .with_ddb(Arc::new(ddb()))
        .with_kubectl(Arc::new(kubectl()))
}

/// Runs `args` with `services` and empty stdin, returning the exit status
/// and the captured stdout and stderr.
async fn ss_with(services: Services, args: &[&str]) -> (Exit, String, String) {
    let cli = ss_support::parse_from(&services.registry, args.iter().copied())
        .unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
    let (io, captured) = Io::capture("");
    let exit = ss_support::run_with(cli, services, io).await;
    (exit, captured.stdout(), captured.stderr())
}

#[test]
fn every_cross_service_operation_is_a_read() {
    let mut ids = Vec::new();
    for op in ops::builtin().all() {
        let meta = op.meta();
        if meta.category == "Cross-service" {
            assert_eq!(meta.class, Class::Read, "{} is not a read", meta.id);
            ids.push(meta.id);
        }
    }
    ids.sort_unstable();
    assert_eq!(ids, ["config.drift", "record.trace"]);
}

#[tokio::test]
async fn cross_service_operations_run_where_writes_are_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let runs: [&[&str]; 3] = [
        &[
            "ss-support",
            "run",
            "config.drift",
            "--bucket",
            BUCKET,
            "--key",
            KEY,
            "--configmap",
            "menu-config:menu.json",
            "--namespace",
            "menu",
        ],
        &[
            "ss-support",
            "run",
            "record.trace",
            "--domain",
            "menu",
            "--id",
            "12345",
            "--vars",
            "locale=en-US,channel=mobile",
        ],
        &[
            "ss-support",
            "run",
            "record.trace",
            "--domain",
            "menu",
            "--id",
            "12345",
            "--vars",
            "locale=en-US,channel=mobile",
            "--configmap",
            "menu-config:menu.json",
        ],
    ];

    let audit_path = audit::default_path(&Services::isolated(root.path()).vars);
    for args in runs {
        let (exit, out, err) = ss_with(services(root.path()), args).await;
        assert_eq!(exit, Exit::Ok, "{args:?}: {err}");
        assert_eq!(err, "", "{args:?}");
        assert!(!out.is_empty(), "{args:?} printed nothing");
    }

    // Only runs that are not reads leave an audit entry, so a read-only
    // operation leaves none: a change attempt would have been recorded here
    // (as declined, since stdin is empty).
    assert!(
        !audit_path.exists(),
        "audit log written: {}",
        fs::read_to_string(&audit_path).unwrap_or_default()
    );
}
