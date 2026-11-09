//! Shared test data for the S3 operations: one bucket with a JSON object and
//! a text object, each with two versions.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::config::{DiffDefaults, Environment};
use crate::ops::Deps;
use crate::svc::s3::FakeS3;

/// The bucket every fixture object lives in.
pub(crate) const BUCKET: &str = "cfg-nonprod";

/// The time of the oldest fixture version.
pub(crate) fn t0() -> DateTime<Utc> {
    "2026-07-01T10:00:00Z".parse().expect("valid timestamp")
}

/// A fake holding `tenants/acme.json` (`v1`, `v2`) and `tenants/notes.txt`
/// (`t1`, `t2`) in [`BUCKET`].
pub(crate) fn fake() -> FakeS3 {
    let s3 = FakeS3::new();
    s3.insert(
        BUCKET,
        "tenants/acme.json",
        "v1",
        r#"{"tier":"gold","rules":[{"id":"r1","limit":10}],"updatedAt":"2026-07-01"}"#,
        t0(),
    );
    s3.insert(
        BUCKET,
        "tenants/acme.json",
        "v2",
        r#"{"tier":"platinum","rules":[{"id":"r0","limit":5},{"id":"r1","limit":10}],"updatedAt":"2026-07-02"}"#,
        t0() + Duration::hours(24),
    );
    s3.insert(BUCKET, "tenants/notes.txt", "t1", "hello\nworld\n", t0());
    s3.insert(
        BUCKET,
        "tenants/notes.txt",
        "t2",
        "hello\nthere\n",
        t0() + Duration::hours(1),
    );
    s3
}

/// Dependencies for the `dev` environment backed by `s3`, with the diff
/// defaults the operations are tested against.
pub(crate) fn deps(s3: FakeS3) -> Deps {
    Deps {
        env: Environment {
            name: "dev".into(),
            ..Environment::default()
        },
        diff: DiffDefaults {
            ignore: vec!["**.updatedAt".into()],
            array_match: BTreeMap::from([("$.rules[*]".to_owned(), "id".to_owned())]),
        },
        s3: Some(Arc::new(s3)),
        ..Deps::default()
    }
}
