//! The assertions every [`S3Port`] implementation must satisfy.
//!
//! [`run`] checks one fixture against a port and panics, naming the failed
//! check and the values it observed, on the first violation. It backs the
//! in-memory fake, decorators over it, and (under the `test-support` feature)
//! the tests of crates that implement the port against a real endpoint.

use std::collections::{BTreeSet, HashSet};

use super::{CURRENT, S3Port, Version, Versioning, is_current};
use crate::svc::SvcError;

#[cfg(test)]
use super::FakeS3;

/// What the suite needs to know about the store behind a port.
pub struct Fixture<'a> {
    /// The port under test.
    pub port: &'a dyn S3Port,
    /// A bucket that exists.
    pub bucket: String,
    /// A nested key (it contains a `/`) whose history is the puts in
    /// `bodies` plus the delete markers in `delete_marker_ids`. The newest
    /// entry is a put.
    pub key: String,
    /// Bodies of `key`, oldest first, at least two.
    pub bodies: Vec<String>,
    /// Version ids of `bodies`, in the same order.
    pub version_ids: Vec<String>,
    /// Ids of the delete markers in `key`'s history.
    pub delete_marker_ids: Vec<String>,
    /// A listing that spans several pages.
    pub paging: Paging<'a>,
}

/// A prefix holding more keys than fit on one page.
pub struct Paging<'a> {
    /// A port that returns at most `page_size` entries per listing.
    pub port: &'a dyn S3Port,
    /// The prefix to list.
    pub prefix: String,
    /// Every key directly under `prefix`, in listing order; more than
    /// `page_size` of them.
    pub want_keys: Vec<String>,
    /// How many entries the port returns per page.
    pub page_size: usize,
}

/// The three bodies the shared fixtures write to one key, oldest first.
pub const BODIES: [&str; 3] = [
    r#"{"tier":"gold","rules":[{"id":"r1","limit":10}]}"#,
    r#"{"tier":"platinum","rules":[{"id":"r1","limit":10}]}"#,
    r#"{"tier":"platinum","rules":[{"id":"r0","limit":5},{"id":"r1","limit":10}]}"#,
];

#[cfg(test)]
const BUCKET: &str = "ss-config";
#[cfg(test)]
const KEY: &str = "cfg/tenants/acme.json";

/// A fake seeded with put, put, delete, put on one key plus five keys under
/// `cfg/many/`. Returns the fake, the three put ids (oldest first) and the
/// delete marker id.
#[cfg(test)]
pub(crate) fn seeded_fake() -> (FakeS3, Vec<String>, String) {
    let fake = FakeS3::new();
    let first = fake.put(BUCKET, KEY, BODIES[0]);
    let second = fake.put(BUCKET, KEY, BODIES[1]);
    let marker = fake.delete(BUCKET, KEY);
    let third = fake.put(BUCKET, KEY, BODIES[2]);
    for name in ["a", "b", "c", "d", "e"] {
        fake.put(BUCKET, &format!("cfg/many/{name}.json"), "{}");
    }
    (fake, vec![first, second, third], marker)
}

/// The fixture matching [`seeded_fake`]: `port` serves the data, `paged`
/// serves the same data two entries per page.
#[cfg(test)]
pub(crate) fn fixture<'a>(
    port: &'a dyn S3Port,
    paged: &'a dyn S3Port,
    ids: Vec<String>,
    marker: String,
) -> Fixture<'a> {
    Fixture {
        port,
        bucket: BUCKET.into(),
        key: KEY.into(),
        bodies: BODIES.map(String::from).to_vec(),
        version_ids: ids,
        delete_marker_ids: vec![marker],
        paging: Paging {
            port: paged,
            prefix: "cfg/many/".into(),
            want_keys: ["a", "b", "c", "d", "e"]
                .map(|name| format!("cfg/many/{name}.json"))
                .to_vec(),
            page_size: 2,
        },
    }
}

/// `key` up to and including its last `/`; empty when it has none.
fn parent(key: &str) -> &str {
    key.rfind('/').map_or("", |slash| &key[..=slash])
}

/// The value of a call that must succeed.
fn must<T>(check: &str, call: &str, result: Result<T, SvcError>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => panic!("{check}: {call} failed: {err}"),
    }
}

/// Requires that a call failed with a not-found error.
fn must_be_not_found<T: std::fmt::Debug>(call: &str, result: Result<T, SvcError>) {
    match result {
        Ok(value) => panic!("not found: {call} succeeded with {value:?}"),
        Err(err) => assert!(
            err.is_not_found(),
            "not found: {call} failed with a different error: {err} ({err:?})"
        ),
    }
}

/// Runs every check against `fx`, in a fixed order, panicking on the first
/// violation.
pub async fn run(fx: &Fixture<'_>) {
    assert!(
        fx.bodies.len() >= 2 && fx.bodies.len() == fx.version_ids.len(),
        "fixture: need at least two bodies with one version id each, got {} bodies and {} ids",
        fx.bodies.len(),
        fx.version_ids.len()
    );
    list_buckets(fx).await;
    versioning(fx).await;
    list(fx).await;
    pagination(fx).await;
    list_versions(fx).await;
    get_version(fx).await;
    head(fx).await;
    not_found(fx).await;
}

/// Runs the checks of the port's one write against `fx`. They change the
/// history of the fixture's key, so call this after [`run`] or on a fixture
/// of its own.
pub async fn run_writes(fx: &Fixture<'_>) {
    let (port, bucket, key) = (fx.port, fx.bucket.as_str(), fx.key.as_str());
    let history = || async {
        must(
            "copy_version",
            &format!("list_versions({bucket:?}, {key:?})"),
            port.list_versions(bucket, key).await,
        )
    };
    let before = history().await;
    let (source, source_body) = match (fx.version_ids.first(), fx.bodies.first()) {
        (Some(id), Some(body)) => (id.as_str(), body.as_str()),
        _ => panic!("fixture: need at least one body and its version id"),
    };

    let created = must(
        "copy_version",
        &format!("copy_version({bucket:?}, {key:?}, {source:?})"),
        port.copy_version(bucket, key, source).await,
    );
    assert!(
        !is_current(&created),
        "copy_version: a bucket that keeps versions returns the id of the new version, got {created:?}"
    );
    assert!(
        before.iter().all(|v| v.id != created),
        "copy_version: the returned id {created:?} already named a version"
    );

    let after = history().await;
    assert_eq!(
        after.len(),
        before.len() + 1,
        "copy_version: the history has one more entry; before {:?}, after {:?}",
        before.iter().map(|v| &v.id).collect::<Vec<_>>(),
        after.iter().map(|v| &v.id).collect::<Vec<_>>()
    );
    let latest: Vec<&Version> = after.iter().filter(|v| v.is_latest).collect();
    assert!(
        matches!(latest.as_slice(), [v] if v.id == created && !v.is_delete),
        "copy_version: the new version {created:?} is the one latest entry, got {latest:?}"
    );
    assert_eq!(
        after.first().map(|v| v.id.as_str()),
        Some(created.as_str()),
        "copy_version: the new version is listed first"
    );

    let current = must(
        "copy_version",
        &format!("get_version({bucket:?}, {key:?}, current)"),
        port.get_version(bucket, key, CURRENT).await,
    );
    assert_eq!(
        (
            String::from_utf8_lossy(&current.body).as_ref(),
            current.version_id.as_str()
        ),
        (source_body, created.as_str()),
        "copy_version: the current version holds the copied body under the new id"
    );
    for (id, body) in fx.version_ids.iter().zip(&fx.bodies) {
        let kept = must(
            "copy_version",
            &format!("get_version({bucket:?}, {key:?}, {id:?})"),
            port.get_version(bucket, key, id).await,
        );
        assert_eq!(
            String::from_utf8_lossy(&kept.body),
            body.as_str(),
            "copy_version: version {id:?} still holds its body"
        );
    }

    let missing = format!("{key}.does-not-exist");
    let nope = format!("{bucket}-does-not-exist");
    must_be_not_found(
        &format!("copy_version({bucket:?}, {key:?}, an unknown version id)"),
        port.copy_version(bucket, key, "definitely-not-a-version-id")
            .await,
    );
    must_be_not_found(
        &format!("copy_version({bucket:?}, {missing:?}, {source:?})"),
        port.copy_version(bucket, &missing, source).await,
    );
    must_be_not_found(
        &format!("copy_version({nope:?}, {key:?}, {source:?})"),
        port.copy_version(&nope, key, source).await,
    );
    let refused = ["", CURRENT]
        .into_iter()
        .chain(fx.delete_marker_ids.iter().map(String::as_str));
    for id in refused {
        if let Ok(created) = port.copy_version(bucket, key, id).await {
            panic!("copy_version: copying {id:?} must be refused, but it created {created:?}");
        }
    }
    assert_eq!(
        history().await.len(),
        after.len(),
        "copy_version: a refused or failed copy writes nothing"
    );
}

fn newest_body<'a>(fx: &'a Fixture<'_>) -> &'a str {
    fx.bodies.last().expect("fixture has bodies")
}

async fn list_buckets(fx: &Fixture<'_>) {
    let buckets = must("list_buckets", "list_buckets", fx.port.list_buckets().await);
    assert!(
        buckets.iter().any(|b| b.name == fx.bucket),
        "list_buckets: {:?} is not among {:?}",
        fx.bucket,
        buckets.iter().map(|b| &b.name).collect::<Vec<_>>()
    );
}

async fn versioning(fx: &Fixture<'_>) {
    let state = must(
        "versioning",
        &format!("versioning({:?})", fx.bucket),
        fx.port.versioning(&fx.bucket).await,
    );
    assert_eq!(
        state,
        Versioning::Enabled,
        "versioning: the fixture bucket keeps versions"
    );
    let nope = format!("{}-does-not-exist", fx.bucket);
    must_be_not_found(
        &format!("versioning({nope:?})"),
        fx.port.versioning(&nope).await,
    );
}

async fn list(fx: &Fixture<'_>) {
    let dir = parent(&fx.key);
    let page = must(
        "list",
        &format!("list({:?}, {dir:?})", fx.bucket),
        fx.port.list(&fx.bucket, dir, None).await,
    );
    let Some(found) = page.objects.iter().find(|o| o.key == fx.key) else {
        panic!(
            "list: {:?} is missing under {dir:?}; got keys {:?}",
            fx.key,
            page.objects.iter().map(|o| &o.key).collect::<Vec<_>>()
        );
    };
    let want_size = newest_body(fx).len() as u64;
    assert_eq!(
        found.size, want_size,
        "list: size of {:?} under {dir:?} should be that of the newest body",
        fx.key
    );
    assert_eq!(
        found.bucket, fx.bucket,
        "list: bucket of {:?} under {dir:?}",
        fx.key
    );

    let root = must(
        "list",
        &format!("list({:?}, \"\")", fx.bucket),
        fx.port.list(&fx.bucket, "", None).await,
    );
    assert!(
        !root.prefixes.is_empty(),
        "list: nothing was rolled up into prefixes at the root; objects {:?}",
        root.objects.iter().map(|o| &o.key).collect::<Vec<_>>()
    );
    assert!(
        root.objects.iter().all(|o| o.key != fx.key),
        "list: nested key {:?} was listed at the root instead of being rolled up into {:?}",
        fx.key,
        root.prefixes
    );
}

async fn pagination(fx: &Fixture<'_>) {
    let Paging {
        port,
        prefix,
        want_keys,
        page_size,
    } = &fx.paging;
    assert!(
        want_keys.len() > *page_size,
        "fixture: pagination needs more than {page_size} keys, got {}",
        want_keys.len()
    );
    let max_calls = want_keys.len() + 2;
    let mut calls = 0;
    let mut token: Option<String> = None;
    let mut seen = HashSet::new();
    let mut drained = Vec::new();
    loop {
        calls += 1;
        assert!(
            calls <= max_calls,
            "pagination: did not terminate within {max_calls} calls; drained {drained:?}"
        );
        let page = must(
            "pagination",
            &format!("list({:?}, {prefix:?}, {token:?})", fx.bucket),
            port.list(&fx.bucket, prefix, token.as_deref()).await,
        );
        for object in page.objects {
            assert!(
                seen.insert(object.key.clone()),
                "pagination: {:?} appeared on two pages; drained {drained:?}",
                object.key
            );
            drained.push(object.key);
        }
        match page.next {
            None => break,
            Some(next) => {
                assert!(
                    token.as_deref() != Some(next.as_str()),
                    "pagination: next token {next:?} repeats the token just sent"
                );
                token = Some(next);
            }
        }
    }
    assert!(
        calls >= 2,
        "pagination: {} keys came back in {calls} call with page size {page_size}",
        drained.len()
    );
    assert_eq!(
        &drained, want_keys,
        "pagination: drained keys differ from the expected keys"
    );

    match port
        .list(&fx.bucket, prefix, Some("zzzz-not-a-real-token"))
        .await
    {
        Err(_) => {}
        Ok(page) if page.objects.first().map(|o| &o.key) == want_keys.first() => {
            panic!("pagination: an unknown token restarted the listing");
        }
        Ok(page) => panic!("pagination: an unknown token was accepted, returning {page:?}"),
    }
}

async fn list_versions(fx: &Fixture<'_>) {
    let versions = must(
        "list_versions",
        &format!("list_versions({:?}, {:?})", fx.bucket, fx.key),
        fx.port.list_versions(&fx.bucket, &fx.key).await,
    );
    let ids = |keep: fn(&Version) -> bool| -> Vec<&str> {
        versions
            .iter()
            .filter(|v| keep(v))
            .map(|v| v.id.as_str())
            .collect()
    };

    let want_len = fx.bodies.len() + fx.delete_marker_ids.len();
    assert_eq!(
        versions.len(),
        want_len,
        "list_versions: entry count (bodies plus delete markers); got ids {:?}",
        ids(|_| true)
    );

    let latest = ids(|v| v.is_latest);
    assert_eq!(
        latest,
        [fx.version_ids.last().expect("fixture has ids").as_str()],
        "list_versions: the single latest entry should be the newest put"
    );

    let markers: BTreeSet<&str> = ids(|v| v.is_delete).into_iter().collect();
    let want_markers: BTreeSet<&str> = fx.delete_marker_ids.iter().map(String::as_str).collect();
    assert_eq!(markers, want_markers, "list_versions: delete marker ids");

    let puts = ids(|v| !v.is_delete);
    let want_puts: Vec<&str> = fx.version_ids.iter().rev().map(String::as_str).collect();
    assert_eq!(puts, want_puts, "list_versions: put ids, newest first");

    for pair in versions.windows(2) {
        if let (Some(newer), Some(older)) = (pair[0].modified, pair[1].modified) {
            assert!(
                newer >= older,
                "list_versions: {} ({newer}) is listed before the newer {} ({older})",
                pair[0].id,
                pair[1].id
            );
        }
    }
}

async fn get_version(fx: &Fixture<'_>) {
    for (id, body) in fx.version_ids.iter().zip(&fx.bodies) {
        let object = must(
            "get_version",
            &format!("get_version({:?}, {:?}, {id:?})", fx.bucket, fx.key),
            fx.port.get_version(&fx.bucket, &fx.key, id).await,
        );
        assert_eq!(
            String::from_utf8_lossy(&object.body),
            body.as_str(),
            "get_version: body of version {id:?}"
        );
        assert_eq!(
            &object.version_id, id,
            "get_version: the returned version id"
        );
    }

    for selector in ["", "null"] {
        assert!(
            is_current(selector),
            "get_version: {selector:?} must name the current version"
        );
        let object = must(
            "get_version",
            &format!("get_version({:?}, {:?}, {selector:?})", fx.bucket, fx.key),
            fx.port.get_version(&fx.bucket, &fx.key, selector).await,
        );
        assert_eq!(
            String::from_utf8_lossy(&object.body),
            newest_body(fx),
            "get_version: body for selector {selector:?} should be the newest"
        );
        assert!(
            !is_current(&object.version_id),
            "get_version: selector {selector:?} returned the non-concrete version id {:?}",
            object.version_id
        );
    }

    for marker in &fx.delete_marker_ids {
        if let Ok(object) = fx.port.get_version(&fx.bucket, &fx.key, marker).await {
            panic!(
                "get_version: delete marker {marker:?} returned an object with version id {:?}",
                object.version_id
            );
        }
    }
}

async fn head(fx: &Fixture<'_>) {
    let meta = must(
        "head",
        &format!("head({:?}, {:?})", fx.bucket, fx.key),
        fx.port.head(&fx.bucket, &fx.key).await,
    );
    assert_eq!(
        meta.size,
        newest_body(fx).len() as u64,
        "head: size should be that of the newest body"
    );
    assert_eq!(meta.bucket, fx.bucket, "head: bucket");
    assert_eq!(meta.key, fx.key, "head: key");
}

async fn not_found(fx: &Fixture<'_>) {
    let port = fx.port;
    let bucket = fx.bucket.as_str();
    let missing = "does/not/exist.json";
    let nope = format!("{bucket}-nope");

    must_be_not_found(
        &format!("head({bucket:?}, {missing:?})"),
        port.head(bucket, missing).await,
    );
    must_be_not_found(
        &format!("get_version({bucket:?}, {missing:?}, \"\")"),
        port.get_version(bucket, missing, "").await,
    );
    must_be_not_found(
        &format!("list_versions({bucket:?}, {missing:?})"),
        port.list_versions(bucket, missing).await,
    );
    must_be_not_found(
        &format!(
            "get_version({bucket:?}, {:?}, an unknown version id)",
            fx.key
        ),
        port.get_version(bucket, &fx.key, "definitely-not-a-version-id")
            .await,
    );
    must_be_not_found(
        &format!("list({nope:?}, \"\")"),
        port.list(&nope, "", None).await,
    );
    must_be_not_found(
        &format!("head({nope:?}, {:?})", fx.key),
        port.head(&nope, &fx.key).await,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_satisfies_the_contract() {
        let (fake, ids, marker) = seeded_fake();
        let paged = fake.paged(2);
        run(&fixture(&fake, &paged, ids, marker)).await;
    }

    #[tokio::test]
    #[should_panic(expected = "list_versions: delete marker ids")]
    async fn a_wrong_marker_id_is_reported_by_name() {
        let (fake, ids, _) = seeded_fake();
        let paged = fake.paged(2);
        run(&fixture(&fake, &paged, ids, "del-999".into())).await;
    }

    #[tokio::test]
    #[should_panic(expected = "pagination: 5 keys came back in 1 call")]
    async fn a_listing_that_ignores_the_page_size_is_reported() {
        let (fake, ids, marker) = seeded_fake();
        run(&fixture(&fake, &fake, ids, marker)).await;
    }

    #[tokio::test]
    async fn fake_satisfies_the_write_contract() {
        let (fake, ids, marker) = seeded_fake();
        let paged = fake.paged(2);
        let fx = fixture(&fake, &paged, ids, marker);
        run(&fx).await;
        run_writes(&fx).await;
    }
}
