//! An [`S3Port`] that keeps immutable object versions on disk.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Bucket, Object, ObjectMeta, Page, S3Port, Version, Versioning, is_current};
use crate::cache::Store;
use crate::svc::SvcError;

/// Caches the bodies of explicitly requested versions.
///
/// A version id never changes what it names, so those fetches are stored and
/// served from the store afterwards. The current version is always fetched
/// from the wrapped port, and its concrete id is stored so a later request
/// for that id is a hit. Every other call goes straight to the wrapped port;
/// a copy creates a version under a new id, so nothing stored goes stale.
pub struct Cached {
    inner: Arc<dyn S3Port>,
    store: Arc<dyn Store>,
}

impl Cached {
    /// Wraps `inner`, keeping fetched versions in `store`.
    pub fn new(inner: Arc<dyn S3Port>, store: Arc<dyn Store>) -> Self {
        Self { inner, store }
    }
}

/// The first line of a stored entry.
#[derive(Serialize, Deserialize)]
struct Header {
    meta: MetaRecord,
    version_id: String,
}

#[derive(Serialize, Deserialize)]
struct MetaRecord {
    bucket: String,
    key: String,
    size: u64,
    modified: Option<DateTime<Utc>>,
    etag: String,
}

fn entry_key(bucket: &str, key: &str, version_id: &str) -> String {
    format!("s3/{bucket}/{key}@{version_id}")
}

/// A header line of JSON, a newline, then the raw body.
fn encode(object: &Object) -> Option<Vec<u8>> {
    let header = Header {
        meta: MetaRecord {
            bucket: object.meta.bucket.clone(),
            key: object.meta.key.clone(),
            size: object.meta.size,
            modified: object.meta.modified,
            etag: object.meta.etag.clone(),
        },
        version_id: object.version_id.clone(),
    };
    let mut bytes = serde_json::to_vec(&header).ok()?;
    bytes.push(b'\n');
    bytes.extend_from_slice(&object.body);
    Some(bytes)
}

/// The object stored in `bytes`; `None` when it is not a well-formed entry.
fn decode(bytes: &[u8]) -> Option<Object> {
    let split = bytes.iter().position(|&b| b == b'\n')?;
    let header: Header = serde_json::from_slice(&bytes[..split]).ok()?;
    Some(Object {
        meta: ObjectMeta {
            bucket: header.meta.bucket,
            key: header.meta.key,
            size: header.meta.size,
            modified: header.meta.modified,
            etag: header.meta.etag,
        },
        version_id: header.version_id,
        body: bytes[split + 1..].to_vec(),
    })
}

#[async_trait]
impl S3Port for Cached {
    async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError> {
        self.inner.list_buckets().await
    }

    async fn list(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<Page, SvcError> {
        self.inner.list(bucket, prefix, token).await
    }

    async fn versioning(&self, bucket: &str) -> Result<Versioning, SvcError> {
        self.inner.versioning(bucket).await
    }

    async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError> {
        self.inner.list_versions(bucket, key).await
    }

    async fn get_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Object, SvcError> {
        if !is_current(version_id)
            && let Some(object) = self
                .store
                .get(&entry_key(bucket, key, version_id))
                .and_then(|bytes| decode(&bytes))
        {
            return Ok(object);
        }

        let object = self.inner.get_version(bucket, key, version_id).await?;
        if !is_current(&object.version_id)
            && let Some(bytes) = encode(&object)
        {
            // A failed write only costs a refetch next time.
            let _ = self
                .store
                .put(&entry_key(bucket, key, &object.version_id), &bytes);
        }
        Ok(object)
    }

    async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError> {
        self.inner.head(bucket, key).await
    }

    async fn copy_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<String, SvcError> {
        self.inner.copy_version(bucket, key, version_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};

    use super::super::contract::{fixture, run, run_writes, seeded_fake};
    use super::super::lazy::Lazy;
    use super::super::{Bucket, FakeS3, Object, ObjectMeta, Page, S3Port, Version, Versioning};
    use super::*;
    use crate::cache::{Dir, Store};
    use crate::svc::SvcError;

    /// Counts `get_version` calls made to the wrapped fake.
    struct Counting {
        inner: FakeS3,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl S3Port for Counting {
        async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError> {
            self.inner.list_buckets().await
        }
        async fn list(
            &self,
            bucket: &str,
            prefix: &str,
            token: Option<&str>,
        ) -> Result<Page, SvcError> {
            self.inner.list(bucket, prefix, token).await
        }
        async fn versioning(&self, bucket: &str) -> Result<Versioning, SvcError> {
            self.inner.versioning(bucket).await
        }
        async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError> {
            self.inner.list_versions(bucket, key).await
        }
        async fn get_version(
            &self,
            bucket: &str,
            key: &str,
            version_id: &str,
        ) -> Result<Object, SvcError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.get_version(bucket, key, version_id).await
        }
        async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError> {
            self.inner.head(bucket, key).await
        }
        async fn copy_version(
            &self,
            bucket: &str,
            key: &str,
            version_id: &str,
        ) -> Result<String, SvcError> {
            self.inner.copy_version(bucket, key, version_id).await
        }
    }

    struct Rig {
        fake: FakeS3,
        calls: Arc<AtomicUsize>,
        store: Arc<Dir>,
        cached: Cached,
        _dir: tempfile::TempDir,
    }

    impl Rig {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("rfc3339")
    }

    fn rig() -> Rig {
        let fake = FakeS3::new();
        fake.insert(
            "b",
            "k",
            "v1",
            r#"{"a":1}"#,
            at("2026-07-01T10:00:00.123456789Z"),
        );
        fake.insert("b", "k", "v2", r#"{"a":2}"#, at("2026-07-02T10:00:00Z"));
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Dir::new(dir.path()));
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Counting {
            inner: fake.clone(),
            calls: Arc::clone(&calls),
        };
        let cached = Cached::new(Arc::new(inner), store.clone());
        Rig {
            fake,
            calls,
            store,
            cached,
            _dir: dir,
        }
    }

    #[tokio::test]
    async fn explicit_versions_are_fetched_once() {
        let rig = rig();
        let want = rig.fake.get_version("b", "k", "v1").await.expect("fake");
        for _ in 0..3 {
            let got = rig.cached.get_version("b", "k", "v1").await.expect("get");
            assert_eq!(got, want);
        }
        assert_eq!(rig.calls(), 1);
    }

    #[tokio::test]
    async fn current_is_never_served_from_cache_but_seeds_its_concrete_id() {
        let rig = rig();
        for _ in 0..2 {
            let got = rig
                .cached
                .get_version("b", "k", "null")
                .await
                .expect("get current");
            assert_eq!(got.version_id, "v2");
        }
        assert_eq!(rig.calls(), 2);
        let got = rig
            .cached
            .get_version("b", "k", "v2")
            .await
            .expect("get v2");
        assert_eq!(got.body, br#"{"a":2}"#);
        assert_eq!(rig.calls(), 2);
    }

    #[tokio::test]
    async fn errors_pass_through() {
        let rig = rig();
        let err = rig
            .cached
            .get_version("b", "nope", "v1")
            .await
            .expect_err("missing key");
        assert!(err.is_not_found(), "{err:?}");
    }

    #[tokio::test]
    async fn corrupt_entry_falls_through_and_is_rewritten() {
        let rig = rig();
        rig.store.put("s3/b/k@v1", b"not json\nbody").expect("put");
        let want = rig.fake.get_version("b", "k", "v1").await.expect("fake");

        assert_eq!(
            rig.cached.get_version("b", "k", "v1").await.expect("get"),
            want
        );
        assert_eq!(rig.calls(), 1);
        assert_eq!(
            rig.cached.get_version("b", "k", "v1").await.expect("get"),
            want
        );
        assert_eq!(rig.calls(), 1);
    }

    #[tokio::test]
    async fn entry_without_a_newline_is_a_miss() {
        let rig = rig();
        rig.store.put("s3/b/k@v1", b"{}").expect("put");
        rig.cached.get_version("b", "k", "v1").await.expect("get");
        assert_eq!(rig.calls(), 1);
    }

    #[tokio::test]
    async fn bodies_with_newlines_and_binary_bytes_round_trip() {
        let rig = rig();
        let body = b"line one\nline two\n\x00\xff\n".to_vec();
        rig.fake
            .insert("b", "bin", "v1", body.clone(), at("2026-07-03T00:00:00Z"));
        rig.cached
            .get_version("b", "bin", "v1")
            .await
            .expect("miss");
        let hit = rig.cached.get_version("b", "bin", "v1").await.expect("hit");
        assert_eq!(hit.body, body);
        assert_eq!(rig.calls(), 1);
    }

    struct BrokenStore;

    impl Store for BrokenStore {
        fn get(&self, _key: &str) -> Option<Vec<u8>> {
            None
        }
        fn put(&self, _key: &str, _val: &[u8]) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[tokio::test]
    async fn failed_cache_write_does_not_fail_the_fetch() {
        let rig = rig();
        let cached = Cached::new(
            Arc::new(Counting {
                inner: rig.fake.clone(),
                calls: Arc::clone(&rig.calls),
            }),
            Arc::new(BrokenStore),
        );
        let got = cached.get_version("b", "k", "v1").await.expect("get");
        assert_eq!(got.body, br#"{"a":1}"#);
    }

    #[tokio::test]
    async fn cached_lazy_satisfies_the_contract() {
        let (fake, ids, marker) = seeded_fake();
        let dir = tempfile::tempdir().expect("tempdir");
        let store: Arc<dyn Store> = Arc::new(Dir::new(dir.path()));
        let over = |port: FakeS3| {
            let lazy = Lazy::new(move || {
                let port: Arc<dyn S3Port> = Arc::new(port.clone());
                Box::pin(async move { Ok(port) })
            });
            Cached::new(Arc::new(lazy), Arc::clone(&store))
        };
        let cached = over(fake.clone());
        let paged = over(fake.paged(2));
        run(&fixture(&cached, &paged, ids, marker)).await;
    }

    #[tokio::test]
    async fn cached_lazy_satisfies_the_write_contract() {
        let (fake, ids, marker) = seeded_fake();
        let dir = tempfile::tempdir().expect("tempdir");
        let store: Arc<dyn Store> = Arc::new(Dir::new(dir.path()));
        let over = |port: FakeS3| {
            let lazy = Lazy::new(move || {
                let port: Arc<dyn S3Port> = Arc::new(port.clone());
                Box::pin(async move { Ok(port) })
            });
            Cached::new(Arc::new(lazy), Arc::clone(&store))
        };
        let cached = over(fake.clone());
        let paged = over(fake.paged(2));
        let fx = fixture(&cached, &paged, ids, marker);
        // The reads fill the store before the write.
        run(&fx).await;

        run_writes(&fx).await;
    }

    #[tokio::test]
    async fn a_copy_goes_to_the_wrapped_port_and_the_new_version_is_fetched_from_it() {
        let r = rig();
        r.cached.get_version("b", "k", "v1").await.expect("fill");
        let before = r.calls();

        let created = r.cached.copy_version("b", "k", "v1").await.expect("copy");

        assert_eq!(
            r.fake.list_versions("b", "k").await.expect("history")[0].id,
            created
        );
        let current = r.cached.get_version("b", "k", "").await.expect("current");
        assert_eq!(
            (current.body.as_slice(), current.version_id.as_str()),
            (&br#"{"a":1}"#[..], created.as_str())
        );
        assert_eq!(
            r.calls(),
            before + 1,
            "the current version is fetched, not served from the store"
        );
        // What was stored for the copied version is still valid.
        r.cached.get_version("b", "k", "v1").await.expect("stored");
        assert_eq!(r.calls(), before + 1);
    }
}
