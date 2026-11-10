//! In-process end-to-end tests for the S3 operations: drive `ss_support::run_with`
//! with a fake S3 and assert on the exit code and the captured output.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ss_core::svc::s3::FakeS3;
use ss_support::{Exit, Io, Services};

const V1: &str = r#"{"tier":"gold","rules":[{"id":"r1","limit":10}],"updatedAt":"2026-07-01"}"#;
const V2: &str = r#"{"tier":"platinum","rules":[{"id":"r0","limit":5},{"id":"r1","limit":10}],"updatedAt":"2026-07-02"}"#;

fn t0() -> DateTime<Utc> {
    "2026-07-01T10:00:00Z".parse().expect("rfc3339")
}

/// A fake holding two versions of `cfg-nonprod/tenants/acme.json`.
fn fake() -> FakeS3 {
    let s3 = FakeS3::new();
    s3.insert("cfg-nonprod", "tenants/acme.json", "v1", V1, t0());
    s3.insert(
        "cfg-nonprod",
        "tenants/acme.json",
        "v2",
        V2,
        t0() + Duration::hours(24),
    );
    s3
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

/// Services under a fresh home that serve `s3`.
fn with_fake(root: &Path, s3: FakeS3) -> Services {
    Services::isolated(root).with_s3(Arc::new(s3))
}

#[tokio::test]
async fn ops_list_shows_the_s3_operations() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        Services::isolated(root.path()),
        &["ss-support", "ops", "list"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    for want in ["s3.version-diff", "s3.object-inspect", "s3.prefix-audit"] {
        assert!(out.contains(want), "missing {want:?} in {out}");
    }
}

#[tokio::test]
async fn demo_and_ops_list_run_without_s3() {
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
async fn version_diff_through_the_cli_as_json() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "s3.version-diff",
            "--bucket",
            "cfg-nonprod",
            "--key",
            "tenants/acme.json",
            "--a",
            "v1",
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("stdout is JSON");
    let first = &json["views"][0];
    assert_eq!(first["type"], "summary", "{out}");
    assert_eq!(
        first["fields"]["Object"], "cfg-nonprod/tenants/acme.json",
        "{out}"
    );
}

#[tokio::test]
async fn object_inspect_through_the_cli_as_text() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, out, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "s3.object-inspect",
            "--bucket",
            "cfg-nonprod",
            "--key",
            "tenants/acme.json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("Version history"), "{out}");
    assert!(out.contains("v2"), "{out}");
}

#[tokio::test]
async fn a_missing_key_exits_1_with_one_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        with_fake(root.path(), fake()),
        &[
            "ss-support",
            "run",
            "s3.object-inspect",
            "--bucket",
            "cfg-nonprod",
            "--key",
            "nope.json",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(
        err.starts_with("ss-support run s3.object-inspect: "),
        "{err}"
    );
    assert!(err.contains("not found"), "{err}");
}

#[tokio::test]
async fn an_unconfigured_s3_exits_1() {
    let root = tempfile::tempdir().expect("tempdir");
    let (exit, _, err) = ss_with(
        Services::isolated(root.path()),
        &["ss-support", "run", "s3.prefix-audit", "--bucket", "b"],
    )
    .await;
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("S3 is not configured"), "{err}");
}
