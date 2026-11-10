//! Guards that keep the DynamoDB operations read-only: every one is
//! classified as a read, and each runs to completion where no environment
//! allows writes and no confirmation can be answered.

use std::fs;
use std::sync::Arc;

use ss_core::guard::Class;
use ss_core::ops;
use ss_core::svc::ddb::FakeDdb;
use ss_core::svc::ddb::contract::{seed_items, table_info};
use ss_support::{Exit, Io, Services};

/// Runs `args` with `services` and empty stdin, returning the exit status
/// and the captured stdout and stderr.
async fn ss_with(services: Services, args: &[&str]) -> (Exit, String, String) {
    let cli = ss_support::parse_from(&services.registry, args.iter().copied())
        .unwrap_or_else(|e| panic!("parse {args:?}: {e}"));
    let (io, captured) = Io::capture("");
    let exit = ss_support::run_with(cli, services, io).await;
    (exit, captured.stdout(), captured.stderr())
}

/// A fake holding table `tenants` with tenant `acme` at versions V0, V1, V2.
fn seeded() -> FakeDdb {
    let ddb = FakeDdb::new();
    ddb.create_table(table_info("tenants"));
    for item in seed_items() {
        ddb.put("tenants", item);
    }
    ddb
}

#[test]
fn every_ddb_operation_is_a_read() {
    let mut ids = Vec::new();
    for op in ops::builtin().all() {
        let meta = op.meta();
        if meta.id.starts_with("ddb.") {
            assert_eq!(meta.class, Class::Read, "{} is not a read", meta.id);
            ids.push(meta.id);
        }
    }
    ids.sort_unstable();
    assert_eq!(
        ids,
        [
            "ddb.gsi-consistency",
            "ddb.item-compare",
            "ddb.item-history"
        ]
    );
}

#[tokio::test]
async fn ddb_operations_run_where_writes_are_refused() {
    let root = tempfile::tempdir().expect("tempdir");
    let runs: [&[&str]; 3] = [
        &[
            "ss-support",
            "run",
            "ddb.item-compare",
            "--table",
            "tenants",
            "--key-a",
            "TenantId=acme,Version=V0",
            "--key-b",
            "TenantId=acme,Version=V1",
        ],
        &[
            "ss-support",
            "run",
            "ddb.item-history",
            "--table",
            "tenants",
            "--pk",
            "acme",
        ],
        &[
            "ss-support",
            "run",
            "ddb.gsi-consistency",
            "--table",
            "tenants",
            "--index",
            "TierIndex",
            "--key",
            "TenantId=acme,Version=V1",
        ],
    ];

    for args in runs {
        let services = Services::isolated(root.path()).with_ddb(Arc::new(seeded()));
        let (exit, out, err) = ss_with(services, args).await;
        assert_eq!(exit, Exit::Ok, "{args:?}: {err}");
        assert_eq!(err, "", "{args:?}");
        assert!(!out.is_empty(), "{args:?} printed nothing");
    }

    // Only runs that are not reads leave an audit entry, so a read-only
    // operation leaves none: a change attempt would have been recorded here
    // (as declined, since stdin is empty).
    let audit = root.path().join(".local/state/ss-support/audit.log");
    assert!(
        !audit.exists(),
        "audit log written: {}",
        fs::read_to_string(&audit).unwrap_or_default()
    );
}
