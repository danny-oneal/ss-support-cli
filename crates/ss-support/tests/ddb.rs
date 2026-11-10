//! In-process end-to-end tests for the DynamoDB operations: drive
//! `ss_support::run_with` with a fake DynamoDB and assert on the exit code and
//! the captured output.

use std::path::Path;
use std::sync::Arc;

use ss_core::svc::ddb::FakeDdb;
use ss_core::svc::ddb::contract::{seed_items, table_info};
use ss_support::{Exit, Io, Services};

/// A fake holding table `tenants` with tenant `acme` at versions V0, V1, V2.
fn fake() -> FakeDdb {
    let ddb = FakeDdb::new();
    ddb.create_table(table_info("tenants"));
    for item in seed_items() {
        ddb.put("tenants", item);
    }
    ddb
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

/// Services under a fresh home that serve `ddb`.
fn with_fake(root: &Path, ddb: FakeDdb) -> Services {
    Services::isolated(root).with_ddb(Arc::new(ddb))
}

#[tokio::test]
async fn ops_list_shows_the_ddb_operations() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        Services::isolated(root.path()),
        &["ss-support", "ops", "list"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    for want in [
        "ddb.item-compare",
        "ddb.item-history",
        "ddb.gsi-consistency",
    ] {
        assert!(out.contains(want), "missing {want:?} in {out}");
    }
}

#[tokio::test]
async fn demo_and_ops_list_run_without_ddb() {
    let root = tempfile::tempdir().expect("tempdir");
    for args in [
        &["ss-support", "ops", "list"][..],
        &["ss-support", "run", "demo.hello", "--name", "x"][..],
    ] {
        let (exit, _, err) = ss_with(Services::isolated(root.path()), args).await;
        assert_eq!(exit, Exit::Ok, "{args:?}: {err}");
        assert_eq!(err, "", "{args:?}");
    }
}

#[tokio::test]
async fn item_history_through_the_cli_as_json() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "ddb.item-history",
            "--table",
            "tenants",
            "--pk",
            "acme",
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("stdout is JSON");
    let first = &json["views"][0];
    assert_eq!(first["type"], "summary", "{out}");
    assert_eq!(first["fields"]["Item"], "tenants TenantId=acme", "{out}");
    assert_eq!(first["fields"]["Versions"], "3", "{out}");
}

#[tokio::test]
async fn item_compare_through_the_cli_as_text() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "ddb.item-compare",
            "--table",
            "tenants",
            "--key-a",
            "TenantId=acme,Version=V0",
            "--key-b",
            r#"{"TenantId":"acme","Version":"V1"}"#,
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("limits.rps"), "{out}");
}

#[tokio::test]
async fn gsi_consistency_reports_a_consistent_index() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "ddb.gsi-consistency",
            "--table",
            "tenants",
            "--index",
            "TierIndex",
            "--key",
            "TenantId=acme,Version=V0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(
        out.contains("consistent — projection matches the base item"),
        "{out}"
    );
    assert!(!out.contains("DRIFT"), "{out}");
}

#[tokio::test]
async fn gsi_consistency_reports_a_stale_index() {
    let ddb = fake();
    let mut stale = seed_items().remove(0);
    stale["limits"]["rps"] = serde_json::json!(1);
    ddb.set_index_entry("tenants", "TierIndex", stale);

    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fake(root.path(), ddb),
        &[
            "ss-support",
            "run",
            "ddb.gsi-consistency",
            "--table",
            "tenants",
            "--index",
            "TierIndex",
            "--key",
            "TenantId=acme,Version=V0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("DRIFT"), "{out}");
}

#[tokio::test]
async fn a_missing_item_exits_1_with_one_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "ddb.item-compare",
            "--table",
            "tenants",
            "--key-a",
            "TenantId=acme,Version=V0",
            "--key-b",
            "TenantId=nope,Version=V0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(
        err.starts_with("ss-support run ddb.item-compare: "),
        "{err}"
    );
    assert!(err.contains("not found"), "{err}");
}

#[tokio::test]
async fn a_malformed_key_exits_2() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "ddb.item-compare",
            "--table",
            "tenants",
            "--key-a",
            "TenantId",
            "--key-b",
            "TenantId=acme,Version=V1",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(
        err.starts_with("ss-support run ddb.item-compare: "),
        "{err}"
    );
    assert!(err.contains("invalid key-a \"TenantId\""), "{err}");
    assert!(err.contains("not name=value"), "{err}");
}

#[tokio::test]
async fn an_unconfigured_ddb_exits_1() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        Services::isolated(root.path()),
        &[
            "ss-support",
            "run",
            "ddb.item-history",
            "--table",
            "t",
            "--pk",
            "p",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("DynamoDB is not configured"), "{err}");
}

#[tokio::test]
async fn a_missing_required_parameter_exits_2() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "ddb.item-compare",
            "--table",
            "tenants",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Usage, "{err}");
}
