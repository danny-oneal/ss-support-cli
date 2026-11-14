//! An in-memory [`S3Port`] for tests.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{Bucket, Object, ObjectMeta, Page, S3Port, Version, Versioning, is_current};
use crate::svc::SvcError;

/// One entry in a key's history.
#[derive(Debug, Clone)]
enum Entry {
    Object(Object),
    DeleteMarker {
        id: String,
        modified: Option<DateTime<Utc>>,
    },
}

/// A listing row: an object or a rolled-up prefix.
enum Row {
    Object(ObjectMeta),
    Prefix,
}

#[derive(Debug, Default)]
struct State {
    /// bucket -> key -> history, oldest first.
    buckets: BTreeMap<String, BTreeMap<String, Vec<Entry>>>,
    /// Buckets that do not keep versions the usual way; every other bucket
    /// has versioning enabled.
    versioning: BTreeMap<String, Versioning>,
    /// Shared by [`FakeS3::put`], [`FakeS3::delete`], and copies for
    /// generated ids.
    counter: u64,
}

/// An in-memory, versioned S3. Clones share the same data.
#[derive(Debug, Clone, Default)]
pub struct FakeS3 {
    state: Arc<Mutex<State>>,
    page_size: usize,
}

impl FakeS3 {
    /// An empty fake whose listings are not paged.
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle onto the same data whose [`S3Port::list`] returns at most
    /// `page_size` entries (objects and prefixes together) per page.
    pub fn paged(&self, page_size: usize) -> FakeS3 {
        FakeS3 {
            state: Arc::clone(&self.state),
            page_size,
        }
    }

    /// Creates an empty bucket; does nothing when it already exists.
    pub fn create_bucket(&self, bucket: &str) {
        self.lock().buckets.entry(bucket.to_owned()).or_default();
    }

    /// Sets what [`S3Port::versioning`] reports for `bucket`. Nothing else
    /// changes: the fake goes on keeping every version it is given.
    pub fn set_versioning(&self, bucket: &str, versioning: Versioning) {
        self.lock().versioning.insert(bucket.to_owned(), versioning);
    }

    /// Appends a version as the newest, creating the bucket when needed. The
    /// etag is `"{version_id}-etag"`.
    pub fn insert(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        body: impl Into<Vec<u8>>,
        modified: DateTime<Utc>,
    ) {
        let body = body.into();
        let object = Object {
            meta: ObjectMeta {
                bucket: bucket.to_owned(),
                key: key.to_owned(),
                size: body.len() as u64,
                modified: Some(modified),
                etag: format!("{version_id}-etag"),
            },
            version_id: version_id.to_owned(),
            body,
        };
        self.push(bucket, key, Entry::Object(object));
    }

    /// Appends a version stamped now with a generated id (`put-{n}`) and
    /// returns that id.
    pub fn put(&self, bucket: &str, key: &str, body: impl Into<Vec<u8>>) -> String {
        let id = self.next_id("put");
        self.insert(bucket, key, &id, body, Utc::now());
        id
    }

    /// Appends a delete marker stamped now with a generated id (`del-{n}`)
    /// and returns that id.
    pub fn delete(&self, bucket: &str, key: &str) -> String {
        let id = self.next_id("del");
        let marker = Entry::DeleteMarker {
            id: id.clone(),
            modified: Some(Utc::now()),
        };
        self.push(bucket, key, marker);
        id
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_id(&self, kind: &str) -> String {
        let mut state = self.lock();
        state.counter += 1;
        format!("{kind}-{}", state.counter)
    }

    fn push(&self, bucket: &str, key: &str, entry: Entry) {
        self.lock()
            .buckets
            .entry(bucket.to_owned())
            .or_default()
            .entry(key.to_owned())
            .or_default()
            .push(entry);
    }
}

fn history_of<'a>(state: &'a State, bucket: &str, key: &str) -> Result<&'a Vec<Entry>, SvcError> {
    state
        .buckets
        .get(bucket)
        .and_then(|keys| keys.get(key))
        .filter(|history| !history.is_empty())
        .ok_or_else(|| SvcError::not_found(format!("{bucket}/{key}")))
}

#[async_trait]
impl S3Port for FakeS3 {
    async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError> {
        Ok(self
            .lock()
            .buckets
            .keys()
            .map(|name| Bucket {
                name: name.clone(),
                created: None,
            })
            .collect())
    }

    async fn versioning(&self, bucket: &str) -> Result<Versioning, SvcError> {
        let state = self.lock();
        if !state.buckets.contains_key(bucket) {
            return Err(SvcError::not_found(bucket));
        }
        Ok(state
            .versioning
            .get(bucket)
            .copied()
            .unwrap_or(Versioning::Enabled))
    }

    async fn list(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<Page, SvcError> {
        let state = self.lock();
        let keys = state
            .buckets
            .get(bucket)
            .ok_or_else(|| SvcError::not_found(bucket))?;

        let mut rows: BTreeMap<String, Row> = BTreeMap::new();
        for (key, history) in keys {
            let Some(rest) = key.strip_prefix(prefix) else {
                continue;
            };
            let Some(Entry::Object(newest)) = history.last() else {
                continue;
            };
            match rest.find('/') {
                Some(slash) => {
                    let rolled = format!("{prefix}{}", &rest[..=slash]);
                    rows.insert(rolled, Row::Prefix);
                }
                None => {
                    let mut meta = newest.meta.clone();
                    meta.bucket = bucket.to_owned();
                    meta.key = key.clone();
                    meta.size = newest.body.len() as u64;
                    rows.insert(key.clone(), Row::Object(meta));
                }
            }
        }

        let lower = match token {
            Some(token) if rows.contains_key(token) => Bound::Excluded(token.to_owned()),
            Some(token) => {
                return Err(SvcError::Other(anyhow::anyhow!(
                    "continuation token {token:?} was not issued by this listing"
                )));
            }
            None => Bound::Unbounded,
        };
        let mut remaining = rows.range((lower, Bound::Unbounded));
        let limit = if self.page_size > 0 {
            self.page_size
        } else {
            usize::MAX
        };

        let mut page = Page::default();
        let mut last = None;
        for (name, row) in remaining.by_ref().take(limit) {
            match row {
                Row::Object(meta) => page.objects.push(meta.clone()),
                Row::Prefix => page.prefixes.push(name.clone()),
            }
            last = Some(name.clone());
        }
        if remaining.next().is_some() {
            page.next = last;
        }
        Ok(page)
    }

    async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError> {
        let state = self.lock();
        let history = history_of(&state, bucket, key)?;
        let newest = history.len() - 1;
        Ok(history
            .iter()
            .enumerate()
            .rev()
            .map(|(i, entry)| match entry {
                Entry::Object(object) => Version {
                    id: object.version_id.clone(),
                    modified: object.meta.modified,
                    size: object.body.len() as u64,
                    etag: object.meta.etag.clone(),
                    is_latest: i == newest,
                    is_delete: false,
                },
                Entry::DeleteMarker { id, modified } => Version {
                    id: id.clone(),
                    modified: *modified,
                    size: 0,
                    etag: String::new(),
                    is_latest: i == newest,
                    is_delete: true,
                },
            })
            .collect())
    }

    async fn get_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Object, SvcError> {
        let state = self.lock();
        let history = history_of(&state, bucket, key)?;
        let entry = if is_current(version_id) {
            history.last()
        } else {
            history.iter().find(|entry| match entry {
                Entry::Object(object) => object.version_id == version_id,
                Entry::DeleteMarker { id, .. } => id == version_id,
            })
        };
        match entry {
            Some(Entry::Object(object)) => Ok(object.clone()),
            Some(Entry::DeleteMarker { .. }) if is_current(version_id) => {
                Err(SvcError::not_found(format!("{bucket}/{key}")))
            }
            Some(Entry::DeleteMarker { .. }) => Err(SvcError::Other(anyhow::anyhow!(
                "{bucket}/{key}@{version_id} is a delete marker"
            ))),
            None => Err(SvcError::not_found(format!("{bucket}/{key}@{version_id}"))),
        }
    }

    async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError> {
        let object = self.get_version(bucket, key, super::CURRENT).await?;
        Ok(ObjectMeta {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            size: object.body.len() as u64,
            ..object.meta
        })
    }

    /// The copy is stamped now and gets a generated id (`copy-{n}`).
    async fn copy_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<String, SvcError> {
        if is_current(version_id) {
            return Err(SvcError::Other(anyhow::anyhow!(
                "{bucket}/{key}: the version to copy must be named by its own id"
            )));
        }
        let mut state = self.lock();
        let source = history_of(&state, bucket, key)?
            .iter()
            .find(|entry| match entry {
                Entry::Object(object) => object.version_id == version_id,
                Entry::DeleteMarker { id, .. } => id == version_id,
            })
            .cloned();
        let mut object = match source {
            Some(Entry::Object(object)) => object,
            Some(Entry::DeleteMarker { .. }) => {
                return Err(SvcError::Other(anyhow::anyhow!(
                    "{bucket}/{key}@{version_id} is a delete marker"
                )));
            }
            None => return Err(SvcError::not_found(format!("{bucket}/{key}@{version_id}"))),
        };
        state.counter += 1;
        let id = format!("copy-{}", state.counter);
        object.version_id.clone_from(&id);
        object.meta.modified = Some(Utc::now());
        if let Some(history) = state
            .buckets
            .get_mut(bucket)
            .and_then(|keys| keys.get_mut(key))
        {
            history.push(Entry::Object(object));
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(page: &Page) -> Vec<&str> {
        page.objects.iter().map(|o| o.key.as_str()).collect()
    }

    #[tokio::test]
    async fn list_rolls_up_nested_keys_into_prefixes() {
        let s3 = FakeS3::new();
        for k in ["a.json", "dir/b.json", "dir/sub/c.json", "dir/sub/d.json"] {
            s3.put("b", k, "{}");
        }
        let root = s3.list("b", "", None).await.expect("list");
        assert_eq!(keys(&root), ["a.json"]);
        assert_eq!(root.prefixes, ["dir/"]);
        let dir = s3.list("b", "dir/", None).await.expect("list");
        assert_eq!(keys(&dir), ["dir/b.json"]);
        assert_eq!(dir.prefixes, ["dir/sub/"]);
    }

    #[tokio::test]
    async fn paged_listing_shares_data_and_counts_prefixes_toward_the_page() {
        let s3 = FakeS3::new();
        for k in ["p/1", "p/2", "p/sub/3"] {
            s3.put("b", k, "x");
        }
        let paged = s3.paged(2);
        let first = paged.list("b", "p/", None).await.expect("page 1");
        assert_eq!(
            (keys(&first), first.prefixes.clone(), first.next.as_deref()),
            (vec!["p/1", "p/2"], vec![], Some("p/2"))
        );
        let second = paged
            .list("b", "p/", first.next.as_deref())
            .await
            .expect("page 2");
        assert_eq!(
            (keys(&second), second.prefixes.clone(), second.next.clone()),
            (vec![], vec!["p/sub/".to_owned()], None)
        );
    }

    #[tokio::test]
    async fn unknown_buckets_and_tokens_are_reported() {
        let s3 = FakeS3::new();
        s3.create_bucket("z");
        s3.put("a", "k", "x");
        let names: Vec<_> = s3
            .list_buckets()
            .await
            .expect("buckets")
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(names, ["a", "z"]);
        assert!(
            s3.list("missing", "", None)
                .await
                .expect_err("no bucket")
                .is_not_found()
        );
        let err = s3
            .list("a", "", Some("nope"))
            .await
            .expect_err("foreign token");
        assert!(!err.is_not_found());
        assert!(err.to_string().contains("was not issued"), "{err}");
    }

    #[tokio::test]
    async fn explicit_versions_resolve_and_head_reports_the_current_one() {
        let s3 = FakeS3::new();
        let t = "2026-07-01T10:00:00Z".parse().expect("rfc3339");
        s3.insert("b", "k", "v1", "one", t);
        s3.insert("b", "k", "v2", "three", t);
        let v1 = s3.get_version("b", "k", "v1").await.expect("v1");
        assert_eq!((v1.body, v1.meta.etag), (b"one".to_vec(), "v1-etag".into()));
        let missing = s3.get_version("b", "k", "v9").await.expect_err("absent");
        assert_eq!(missing.to_string(), "not found: b/k@v9");
        let head = s3.head("b", "k").await.expect("head");
        assert_eq!((head.size, head.etag.as_str()), (5, "v2-etag"));
    }

    #[tokio::test]
    async fn a_deleted_key_leaves_history_but_not_the_listing() {
        let s3 = FakeS3::new();
        let v1 = s3.put("b", "k", "one");
        let marker = s3.delete("b", "k");
        assert!(keys(&s3.list("b", "", None).await.expect("list")).is_empty());
        assert!(
            s3.get_version("b", "k", "null")
                .await
                .expect_err("current is gone")
                .is_not_found()
        );
        assert!(s3.head("b", "k").await.expect_err("head").is_not_found());
        let err = s3
            .get_version("b", "k", &marker)
            .await
            .expect_err("marker has no body");
        assert!(!err.is_not_found(), "{err}");
        assert_eq!(
            s3.get_version("b", "k", &v1)
                .await
                .expect("old version")
                .body,
            b"one"
        );
        let history = s3.list_versions("b", "k").await.expect("history");
        assert_eq!(
            history
                .iter()
                .map(|v| (v.id.as_str(), v.is_latest, v.is_delete))
                .collect::<Vec<_>>(),
            [(marker.as_str(), true, true), (v1.as_str(), false, false)]
        );
    }

    /// A key written three times: `put-1`, `put-2`, `put-3`.
    fn three_versions() -> FakeS3 {
        let s3 = FakeS3::new();
        for body in ["one", "two", "three"] {
            s3.put("b", "k", body);
        }
        s3
    }

    #[tokio::test]
    async fn copy_version_adds_a_new_current_version_with_the_same_body() {
        let s3 = three_versions();

        let created = s3.copy_version("b", "k", "put-1").await.expect("copy");

        let history = s3.list_versions("b", "k").await.expect("history");
        assert_eq!(
            history.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            [created.as_str(), "put-3", "put-2", "put-1"]
        );
        assert!(history[0].is_latest && !history[1].is_latest);
        let current = s3.get_version("b", "k", "").await.expect("current");
        assert_eq!(
            (current.body, current.version_id),
            (b"one".to_vec(), created)
        );
        // The version that was current is still there.
        assert_eq!(
            s3.get_version("b", "k", "put-3")
                .await
                .expect("previous")
                .body,
            b"three"
        );
    }

    #[tokio::test]
    async fn copy_version_of_something_missing_is_not_found() {
        let s3 = three_versions();

        for (bucket, key, version) in [
            ("b", "k", "put-9"),
            ("b", "nope", "put-1"),
            ("x", "k", "put-1"),
        ] {
            let err = s3
                .copy_version(bucket, key, version)
                .await
                .expect_err("nothing to copy");
            assert!(err.is_not_found(), "{bucket}/{key}@{version}: {err}");
        }
        assert_eq!(s3.list_versions("b", "k").await.expect("history").len(), 3);
    }

    #[tokio::test]
    async fn copy_version_refuses_a_delete_marker_and_a_version_without_its_own_id() {
        let s3 = three_versions();
        let marker = s3.delete("b", "k");

        for version in [marker.as_str(), "", "null"] {
            let err = s3
                .copy_version("b", "k", version)
                .await
                .expect_err("not something that can be copied");
            assert!(!err.is_not_found(), "{version:?}: {err}");
        }
        assert_eq!(s3.list_versions("b", "k").await.expect("history").len(), 4);
    }

    #[tokio::test]
    async fn copy_version_over_a_delete_marker_makes_the_object_listed_again() {
        let s3 = three_versions();
        s3.delete("b", "k");
        assert!(
            s3.list("b", "", None)
                .await
                .expect("list")
                .objects
                .is_empty()
        );

        s3.copy_version("b", "k", "put-2").await.expect("copy");

        let page = s3.list("b", "", None).await.expect("list");
        assert_eq!(keys(&page), ["k"]);
        assert_eq!(
            s3.get_version("b", "k", "").await.expect("current").body,
            b"two"
        );
    }

    #[tokio::test]
    async fn a_bucket_keeps_versions_unless_told_otherwise() {
        let s3 = three_versions();
        assert_eq!(
            s3.versioning("b").await.expect("bucket"),
            Versioning::Enabled
        );

        s3.set_versioning("b", Versioning::Suspended);
        assert_eq!(
            s3.versioning("b").await.expect("bucket"),
            Versioning::Suspended
        );

        let err = s3.versioning("nope").await.expect_err("no such bucket");
        assert!(err.is_not_found(), "{err}");
    }
}
