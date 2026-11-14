//! The S3 port and the types it exchanges.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::SvcError;

mod cached;
#[cfg(any(test, feature = "test-support"))]
pub mod contract;
#[cfg(any(test, feature = "test-support"))]
mod fake;
mod lazy;

pub use cached::Cached;
pub use lazy::Lazy;

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeS3;

/// The version id that names an object's current version.
pub const CURRENT: &str = "null";

/// Whether `version_id` names the current version: empty or [`CURRENT`].
pub fn is_current(version_id: &str) -> bool {
    version_id.is_empty() || version_id == CURRENT
}

/// A bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bucket {
    /// Bucket name.
    pub name: String,
    /// Creation time, when known.
    pub created: Option<DateTime<Utc>>,
}

/// Metadata of one object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Bucket holding the object.
    pub bucket: String,
    /// Object key.
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// Last modification time, when known.
    pub modified: Option<DateTime<Utc>>,
    /// Entity tag.
    pub etag: String,
}

/// Whether a bucket keeps the versions of its objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Versioning {
    /// Every write adds a version.
    Enabled,
    /// Versions written earlier are kept, but a write now takes the place
    /// of the version that has no id.
    Suspended,
    /// The bucket has never kept versions.
    Off,
}

impl std::fmt::Display for Versioning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Enabled => "enabled",
            Self::Suspended => "suspended",
            Self::Off => "off",
        })
    }
}

/// One page of a prefix listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    /// Objects directly under the prefix.
    pub objects: Vec<ObjectMeta>,
    /// Common prefixes ("folders") under the prefix, each ending in `/`.
    pub prefixes: Vec<String>,
    /// Continuation token for the next page; `None` on the last page.
    pub next: Option<String>,
}

/// One entry in an object's version history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Version {
    /// Version id.
    pub id: String,
    /// Last modification time, when known.
    pub modified: Option<DateTime<Utc>>,
    /// Size in bytes; zero for delete markers.
    pub size: u64,
    /// Entity tag; empty for delete markers.
    pub etag: String,
    /// Whether this is the newest entry in the history.
    pub is_latest: bool,
    /// Whether this entry is a delete marker rather than content.
    pub is_delete: bool,
}

/// An object fetched with its body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Object {
    /// The object's metadata.
    pub meta: ObjectMeta,
    /// The version that was fetched.
    pub version_id: String,
    /// The object content.
    pub body: Vec<u8>,
}

impl Object {
    /// A short human label for the fetched version: `current` for the current
    /// version, otherwise the first eight characters of the id, followed by
    /// the modification time (`YYYY-MM-DD HH:MM`, UTC) when known.
    pub fn version_label(&self) -> String {
        if is_current(&self.version_id) {
            return "current".to_owned();
        }
        let short: String = self.version_id.chars().take(8).collect();
        match self.meta.modified {
            Some(modified) => format!("{short} ({})", modified.format("%Y-%m-%d %H:%M")),
            None => short,
        }
    }
}

/// Access to S3: reads, and one write, which makes an earlier version of an
/// object its current one.
#[async_trait]
pub trait S3Port: Send + Sync {
    /// Every bucket visible to the caller.
    async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError>;

    /// Whether `bucket` keeps the versions of its objects.
    ///
    /// # Errors
    ///
    /// [`SvcError::NotFound`] when the bucket does not exist.
    async fn versioning(&self, bucket: &str) -> Result<Versioning, SvcError>;

    /// One page of the keys and common prefixes under `prefix`, resuming
    /// after `token` when given.
    async fn list(&self, bucket: &str, prefix: &str, token: Option<&str>)
    -> Result<Page, SvcError>;

    /// The version history of `key`, newest first.
    async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError>;

    /// The object at `key` in the given version; an empty id or [`CURRENT`]
    /// selects the current version.
    async fn get_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Object, SvcError>;

    /// The metadata of the current version of `key`.
    async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError>;

    /// Makes the version `version_id` of `key` the current one by copying
    /// it onto the same key on the server; its content, content type, and
    /// user metadata are copied with it. Returns the id of the version the
    /// copy created, or [`CURRENT`] when the bucket does not keep versions.
    ///
    /// # Errors
    ///
    /// [`SvcError::NotFound`] when the bucket, key, or version does not
    /// exist. An empty id, [`CURRENT`], and the id of a delete marker are
    /// refused: only a version that holds content and is named by its own
    /// id can be copied.
    async fn copy_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<String, SvcError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("rfc3339")
    }

    #[test]
    fn version_label_names_current_short_ids_and_dates() {
        let obj = |id: &str, modified| Object {
            version_id: id.into(),
            meta: ObjectMeta {
                modified,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(obj("", None).version_label(), "current");
        assert_eq!(
            obj("null", Some(at("2026-07-01T10:00:00Z"))).version_label(),
            "current"
        );
        assert_eq!(obj("abc", None).version_label(), "abc");
        assert_eq!(obj("abcdefghijk", None).version_label(), "abcdefgh");
        assert_eq!(
            obj("abcdefghijk", Some(at("2026-07-01T10:00:59Z"))).version_label(),
            "abcdefgh (2026-07-01 10:00)"
        );
    }

    #[test]
    fn not_found_is_recognised_and_keeps_its_source() {
        let e = SvcError::not_found_from("b/k", std::io::Error::other("NoSuchKey"));
        assert!(e.is_not_found());
        assert_eq!(e.to_string(), "not found: b/k");
        assert_eq!(
            std::error::Error::source(&e)
                .map(ToString::to_string)
                .as_deref(),
            Some("NoSuchKey")
        );
        assert!(!SvcError::from(anyhow::anyhow!("boom")).is_not_found());
    }
}
