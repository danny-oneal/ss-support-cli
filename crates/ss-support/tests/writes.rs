//! In-process end-to-end tests for the two operations that change state,
//! `s3.restore-version` and `ddb.item-edit`: the environment allowlist, the
//! confirmation, `--yes`, `--dry-run`, the audit entry, and the exit codes.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use ss_core::guard::Class;
use ss_core::ops;
use ss_core::svc::ddb::contract::{seed_items, table_info};
use ss_core::svc::ddb::{DdbPort, FakeDdb, Key};
use ss_core::svc::s3::{FakeS3, S3Port};
use ss_support::{Exit, Io, Services};

const BUCKET: &str = "cfg-nonprod";
const KEY: &str = "tenants/acme.json";
const V1: &str = r#"{"tier":"gold","rules":[{"id":"r1","limit":10}]}"#;
const V2: &str = r#"{"tier":"platinum","rules":[{"id":"r1","limit":10}]}"#;

const RESTORE: [&str; 11] = [
    "ss-support",
    "run",
    "s3.restore-version",
    "--env",
    "dev",
    "--bucket",
    BUCKET,
    "--key",
    KEY,
    "--version",
    "v1",
];

const EDIT: [&str; 11] = [
    "ss-support",
    "run",
    "ddb.item-edit",
    "--env",
    "dev",
    "--table",
    "tenants",
    "--key",
    "TenantId=acme,Version=V1",
    "--set",
    r#"{"Tier":"silver"}"#,
];

fn t0() -> DateTime<Utc> {
    "2026-07-01T10:00:00Z".parse().expect("rfc3339")
}

/// A bucket holding two versions of one object, `v1` then `v2`.
fn s3() -> FakeS3 {
    let s3 = FakeS3::new();
    s3.insert(BUCKET, KEY, "v1", V1, t0());
    s3.insert(BUCKET, KEY, "v2", V2, t0() + Duration::hours(24));
    s3
}

/// Table `tenants` with tenant `acme` at versions V0, V1 (gold), V2.
fn ddb() -> FakeDdb {
    let ddb = FakeDdb::new();
    ddb.create_table(table_info("tenants"));
    for item in seed_items() {
        ddb.put("tenants", item);
    }
    ddb
}

/// Puts `dev` on the allowlist of the home under `root`.
fn allow_dev(root: &Path) {
    let path = root.join(".config/ss-support/config.yaml");
    fs::create_dir_all(path.parent().expect("has a parent")).expect("create dirs");
    fs::write(path, "environments:\n  dev: {}\n").expect("write config");
}

fn audit(root: &Path) -> String {
    fs::read_to_string(root.join(".local/state/ss-support/audit.log")).unwrap_or_default()
}

/// Runs `args` with `services`, answering any question with `stdin`, and
/// returns the exit status and the captured stdout and stderr.
async fn ss_with(services: Services, args: &[&str], stdin: &str) -> (Exit, String, String) {
    let cli = ss_support::parse_from(&services.registry, args.iter().copied())
        .unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
    let (io, captured) = Io::capture(stdin);
    let exit = ss_support::run_with(cli, services, io).await;
    (exit, captured.stdout(), captured.stderr())
}

async fn version_ids(s3: &FakeS3) -> Vec<String> {
    s3.list_versions(BUCKET, KEY)
        .await
        .expect("history")
        .into_iter()
        .map(|v| v.id)
        .collect()
}

async fn current(s3: &FakeS3) -> String {
    let object = s3.get_version(BUCKET, KEY, "").await.expect("current");
    String::from_utf8(object.body).expect("text")
}

fn tenant_key() -> Key {
    let Value::Object(key) = json!({"TenantId": "acme", "Version": "V1"}) else {
        unreachable!("a JSON object literal is an object")
    };
    key
}

async fn tier(ddb: &FakeDdb) -> Value {
    ddb.get_item("tenants", &tenant_key())
        .await
        .expect("the item")["Tier"]
        .clone()
}

#[test]
fn the_two_write_operations_are_the_only_operations_that_are_not_reads() {
    let writes: Vec<(&str, Class)> = ops::builtin()
        .all()
        .iter()
        .map(|op| op.meta())
        .filter(|meta| meta.class != Class::Read)
        .map(|meta| (meta.id, meta.class))
        .collect();

    assert_eq!(
        writes,
        [
            ("ddb.item-edit", Class::Destructive),
            ("s3.restore-version", Class::Mutate)
        ]
    );
}

#[tokio::test]
async fn outside_the_allowlist_a_restore_is_refused_and_s3_is_never_reached() {
    let root = tempfile::tempdir().expect("tempdir");
    let built = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&built);
    let lazy = ss_core::svc::s3::Lazy::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
        let port: Arc<dyn S3Port> = Arc::new(s3());
        Box::pin(async move { Ok(port) })
    });
    let services = Services::isolated(root.path()).with_s3(Arc::new(lazy));
    let args = [&RESTORE[..], &["--yes"]].concat();

    let (exit, out, err) = ss_with(services, &args, "y\n").await;

    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        err,
        "ss-support run s3.restore-version: environment \"dev\" is not in the configured allowlist\n"
    );
    assert_eq!(out, "");
    assert_eq!(built.load(Ordering::SeqCst), 0, "S3 was reached");
    assert!(audit(root.path()).contains(r#""outcome":"denied""#));
}

#[tokio::test]
async fn outside_the_allowlist_an_edit_is_refused_and_dynamodb_is_never_reached() {
    let root = tempfile::tempdir().expect("tempdir");
    let built = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&built);
    let lazy = ss_core::svc::ddb::Lazy::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
        let port: Arc<dyn DdbPort> = Arc::new(ddb());
        Box::pin(async move { Ok(port) })
    });
    let services = Services::isolated(root.path()).with_ddb(Arc::new(lazy));

    let (exit, out, err) = ss_with(services, &EDIT, "tenants\n").await;

    assert_eq!(exit, Exit::Failed);
    assert_eq!(
        err,
        "ss-support run ddb.item-edit: environment \"dev\" is not in the configured allowlist\n"
    );
    assert_eq!(out, "");
    assert_eq!(built.load(Ordering::SeqCst), 0, "DynamoDB was reached");
    assert!(audit(root.path()).contains(r#""outcome":"denied""#));
}

#[tokio::test]
async fn a_restore_with_yes_copies_the_version_and_is_audited() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let s3 = s3();
    let services = Services::isolated(root.path()).with_s3(Arc::new(s3.clone()));
    let args = [&RESTORE[..], &["--yes"]].concat();

    let (exit, out, err) = ss_with(services, &args, "").await;

    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(err, "", "nothing was asked");
    assert!(out.contains("New current version"), "{out}");
    let ids = version_ids(&s3).await;
    assert_eq!(&ids[1..], ["v2", "v1"], "{ids:?}");
    assert_eq!(current(&s3).await, V1);
    let audit = audit(root.path());
    assert_eq!(audit.lines().count(), 1, "{audit}");
    assert!(
        audit.contains(r#""op":"s3.restore-version""#) && audit.contains(r#""outcome":"ok""#),
        "{audit}"
    );
}

#[tokio::test]
async fn a_restore_shows_the_change_asks_and_proceeds_on_y() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let s3 = s3();
    let services = Services::isolated(root.path()).with_s3(Arc::new(s3.clone()));

    let (exit, _, err) = ss_with(services, &RESTORE, "y\n").await;

    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(
        err.contains("About to run s3.restore-version"),
        "the change is shown before the question: {err}"
    );
    assert!(err.contains("platinum") && err.contains("gold"), "{err}");
    assert!(
        err.ends_with("Proceed with s3.restore-version? [y/N] "),
        "{err}"
    );
    assert_eq!(current(&s3).await, V1);
}

#[tokio::test]
async fn a_declined_restore_exits_1_and_copies_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let s3 = s3();
    let services = Services::isolated(root.path()).with_s3(Arc::new(s3.clone()));

    let (exit, _, err) = ss_with(services, &RESTORE, "n\n").await;

    assert_eq!(exit, Exit::Failed);
    assert!(
        err.ends_with("ss-support run s3.restore-version: operation not confirmed\n"),
        "{err}"
    );
    assert_eq!(version_ids(&s3).await, ["v2", "v1"]);
    assert!(audit(root.path()).contains(r#""outcome":"declined""#));
}

#[tokio::test]
async fn an_edit_needs_the_table_name_typed_and_yes_does_not_replace_it() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let ddb = ddb();
    let args = [&EDIT[..], &["--yes"]].concat();

    for answer in ["", "y\n", "yes\n", "Tenants\n"] {
        let services = Services::isolated(root.path()).with_ddb(Arc::new(ddb.clone()));
        let (exit, _, err) = ss_with(services, &args, answer).await;

        assert_eq!(exit, Exit::Failed, "{answer:?}");
        assert!(
            err.contains(r#"This is DESTRUCTIVE. Type "tenants" to confirm: "#),
            "{err}"
        );
        assert!(err.contains("operation not confirmed"), "{err}");
        assert_eq!(tier(&ddb).await, "gold", "{answer:?}");
    }
}

#[tokio::test]
async fn an_edit_with_the_table_name_typed_updates_the_item_and_is_audited() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let ddb = ddb();
    let services = Services::isolated(root.path()).with_ddb(Arc::new(ddb.clone()));

    let (exit, out, err) = ss_with(services, &EDIT, "tenants\n").await;

    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(err.contains("About to run ddb.item-edit"), "{err}");
    assert!(err.contains("gold") && err.contains("silver"), "{err}");
    assert!(out.contains("updated"), "{out}");
    assert_eq!(tier(&ddb).await, "silver");
    let item = ddb
        .get_item("tenants", &tenant_key())
        .await
        .expect("the item");
    assert_eq!(item["limits"], json!({"rps": 100, "burst": 200}));
    let audit = audit(root.path());
    assert!(
        audit.contains(r#""op":"ddb.item-edit""#) && audit.contains(r#""outcome":"ok""#),
        "{audit}"
    );
}

#[tokio::test]
async fn a_dry_run_shows_what_would_change_and_changes_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let (s3, ddb) = (s3(), ddb());

    let services = Services::isolated(root.path()).with_s3(Arc::new(s3.clone()));
    let args = [&RESTORE[..], &["--dry-run"]].concat();
    let (exit, out, err) = ss_with(services, &args, "").await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(err, "", "a dry run asks nothing");
    assert!(
        out.contains("dry-run: stopped before the first mutating step"),
        "{out}"
    );
    assert!(out.contains("platinum") && out.contains("gold"), "{out}");
    assert_eq!(version_ids(&s3).await, ["v2", "v1"]);

    let services = Services::isolated(root.path()).with_ddb(Arc::new(ddb.clone()));
    let args = [&EDIT[..], &["--dry-run"]].concat();
    let (exit, out, err) = ss_with(services, &args, "").await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert_eq!(err, "");
    assert!(out.contains("gold") && out.contains("silver"), "{out}");
    assert_eq!(tier(&ddb).await, "gold");
}

#[tokio::test]
async fn an_edit_that_cannot_be_made_is_a_usage_error() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let ddb = ddb();
    let base = &EDIT[..9];

    for (extra, want) in [
        (&["--set", r#"{"Version":"V9"}"#][..], "key attribute"),
        (&["--remove", "TenantId"][..], "key attribute"),
        (&["--set", "Tier=silver"][..], "not valid JSON"),
        (&["--set", "[1]"][..], "JSON object"),
        (&[][..], "nothing to set or remove"),
    ] {
        let services = Services::isolated(root.path()).with_ddb(Arc::new(ddb.clone()));
        let args = [base, extra].concat();
        let (exit, out, err) = ss_with(services, &args, "tenants\n").await;

        assert_eq!(exit, Exit::Usage, "{extra:?}: {err}");
        assert!(
            err.starts_with("ss-support run ddb.item-edit: invalid ") && err.contains(want),
            "{extra:?}: {err}"
        );
        assert_eq!(err.lines().count(), 1, "{err}");
        assert_eq!(out, "");
        assert_eq!(tier(&ddb).await, "gold");
    }
}

#[tokio::test]
async fn a_restore_that_cannot_be_made_exits_1_with_one_line() {
    let root = tempfile::tempdir().expect("tempdir");
    allow_dev(root.path());
    let s3 = s3();
    let marker = s3.delete(BUCKET, KEY);
    s3.put(BUCKET, KEY, "{}");

    for (version, want) in [
        (marker.as_str(), "is a delete marker"),
        ("v9", "has no version \"v9\""),
    ] {
        let services = Services::isolated(root.path()).with_s3(Arc::new(s3.clone()));
        let args = [&RESTORE[..9], &["--version", version, "--yes"]].concat();
        let (exit, out, err) = ss_with(services, &args, "").await;

        assert_eq!(exit, Exit::Failed, "{version}");
        assert!(err.contains(want), "{err}");
        assert_eq!(err.lines().count(), 1, "{err}");
        assert_eq!(out, "");
    }
    assert_eq!(version_ids(&s3).await.len(), 4);
}

#[tokio::test]
async fn the_write_operations_are_listed_and_described() {
    let root = tempfile::tempdir().expect("tempdir");

    let (exit, out, err) = ss_with(
        Services::isolated(root.path()),
        &["ss-support", "ops", "list"],
        "",
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    for want in ["s3.restore-version", "ddb.item-edit"] {
        assert!(out.contains(want), "missing {want:?} in {out}");
    }

    let (exit, out, err) = ss_with(
        Services::isolated(root.path()),
        &["ss-support", "ops", "describe", "ddb.item-edit"],
        "",
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    for want in ["destructive", "--table", "--key", "--set", "--remove"] {
        assert!(out.contains(want), "missing {want:?} in {out}");
    }
}
