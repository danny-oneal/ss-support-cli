//! In-process end-to-end tests for the cross-service operations: drive
//! `ss_support::run_with` with fake S3, DynamoDB, and Kubernetes ports and
//! assert on the exit code and the captured output.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use ss_core::svc::ddb::{AttrType, FakeDdb, Item, TableInfo};
use ss_core::svc::kubectl::FakeKubectl;
use ss_core::svc::s3::FakeS3;
use ss_support::{Exit, Io, Services};

const BUCKET: &str = "ss-config-dev";
const KEY: &str = "menu/12345.json";
const TABLE: &str = "ss-menu-dev";

/// A config naming the `menu` record shape.
const CONFIG: &str = "\
default_env: dev
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
        r#"{"items":[{"id":"latte","price":5}]}"#,
        t0(),
    );
    s3.insert(
        BUCKET,
        KEY,
        "ver-2",
        r#"{"items":[{"id":"latte","price":6}]}"#,
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

/// A cluster whose `menu-config` ConfigMap holds the first version's body
/// under `menu.json` and a second file, so a bare name is ambiguous.
fn kubectl() -> FakeKubectl {
    let k = FakeKubectl::new();
    k.insert(
        "menu",
        "configmap",
        "menu-config",
        r#"{"data":{"menu.json":"{\"items\":[{\"id\":\"latte\",\"price\":5}]}","other.json":"{}"}}"#,
    );
    k
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

/// Services under a home at `root` holding [`CONFIG`], serving the fakes.
fn with_fakes(root: &Path) -> Services {
    let config = root.join(".config/ss-support/config.yaml");
    fs::create_dir_all(config.parent().expect("config has a parent")).expect("mkdir");
    fs::write(&config, CONFIG).expect("write config");
    Services::isolated(root)
        .with_s3(Arc::new(s3()))
        .with_ddb(Arc::new(ddb()))
        .with_kubectl(Arc::new(kubectl()))
}

/// Runs `config.drift` against `configmap` with the fakes.
async fn drift(configmap: &str) -> (Exit, String, String) {
    let root = tempfile::tempdir().expect("tempdir");
    ss_with(
        with_fakes(root.path()),
        &[
            "ss-support",
            "run",
            "config.drift",
            "--bucket",
            BUCKET,
            "--key",
            KEY,
            "--configmap",
            configmap,
            "--namespace",
            "menu",
        ],
    )
    .await
}

/// Asserts that `err` is exactly one line.
fn assert_one_line(err: &str) {
    assert_eq!(err.trim_end().lines().count(), 1, "{err}");
}

#[tokio::test]
async fn ops_list_shows_the_cross_service_operations() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        Services::isolated(root.path()),
        &["ss-support", "ops", "list"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    for want in ["config.drift", "record.trace"] {
        assert!(out.contains(want), "missing {want:?} in {out}");
    }
}

#[tokio::test]
async fn config_drift_through_the_cli_as_json() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fakes(root.path()),
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
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    let json: Value = serde_json::from_str(&out).expect("stdout is JSON");
    let first = &json["views"][0];
    assert_eq!(first["type"], "summary", "{out}");
    let result = first["fields"]["Result"].as_str().expect("a result string");
    assert!(result.starts_with("DRIFT ("), "{out}");
    assert_eq!(first["fields"]["ConfigMap"], "menu/menu-config", "{out}");
}

#[tokio::test]
async fn record_trace_through_the_cli_as_text() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fakes(root.path()),
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
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("Recorded version"), "{out}");
    assert!(out.contains("ver-2"), "{out}");
    assert_eq!(err, "");
}

#[tokio::test]
async fn a_missing_configmap_exits_1_with_one_line() {
    let (exit, _, err) = drift("absent:menu.json").await;
    assert_eq!(exit, Exit::Failed, "{err}");
    assert_one_line(&err);
    assert!(
        err.starts_with("ss-support run config.drift: diff live ConfigMap: "),
        "{err}"
    );
    assert!(err.contains("not found"), "{err}");
}

#[tokio::test]
async fn an_ambiguous_configmap_exits_2() {
    let (exit, _, err) = drift("menu-config").await;
    assert_eq!(exit, Exit::Usage, "{err}");
    assert_one_line(&err);
    assert!(err.contains("menu.json"), "{err}");
    assert!(err.contains("other.json"), "{err}");
}

#[tokio::test]
async fn an_unknown_domain_exits_2() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        with_fakes(root.path()),
        &[
            "ss-support",
            "run",
            "record.trace",
            "--domain",
            "nope",
            "--id",
            "12345",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Usage, "{err}");
    assert_one_line(&err);
}

#[tokio::test]
async fn an_unconfigured_kubectl_exits_1() {
    let root = tempfile::tempdir().expect("tempdir");
    let services = Services::isolated(root.path()).with_s3(Arc::new(s3()));
    let (exit, _, err) = ss_with(
        services,
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
        ],
    )
    .await;
    assert_eq!(exit, Exit::Failed, "{err}");
    assert!(err.contains("Kubernetes is not configured"), "{err}");
    let lines: Vec<&str> = err.lines().collect();
    let [line] = lines.as_slice() else {
        panic!(
            "expected exactly one stderr line, got {}: {err}",
            lines.len()
        );
    };
    assert!(line.starts_with("ss-support run config.drift: "), "{err}");
}

#[tokio::test]
async fn a_missing_required_parameter_exits_2() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        with_fakes(root.path()),
        &["ss-support", "run", "config.drift", "--bucket", BUCKET],
    )
    .await;
    assert_eq!(exit, Exit::Usage, "{err}");
    assert_one_line(&err);
}
