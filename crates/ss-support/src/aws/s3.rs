//! The S3 port backed by the AWS SDK.
//!
//! SDK not-found shapes (missing bucket, key or version) become
//! [`SvcError::NotFound`]; every other SDK failure is kept whole as
//! [`SvcError::Other`].

use std::cmp::Reverse;

use anyhow::anyhow;
use async_trait::async_trait;
use aws_config::SdkConfig;
use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::DateTime as SdkDateTime;
use chrono::{DateTime, Utc};
use ss_core::svc::SvcError;
use ss_core::svc::s3::{Bucket, Object, ObjectMeta, Page, S3Port, Version, is_current};

/// The default cap on an object body read into memory: 32 MiB.
pub(crate) const MAX_BODY: u64 = 32 << 20;

/// How [`AwsS3`] reaches S3.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// An S3-compatible endpoint URL (MinIO, LocalStack). Setting one also
    /// switches to path-style addressing.
    pub endpoint: Option<String>,
    /// The largest body, in bytes, a single fetch reads into memory.
    pub max_body: u64,
    /// Keys per listing page; `None` leaves the service default.
    pub page_size: Option<i32>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            endpoint: None,
            max_body: MAX_BODY,
            page_size: None,
        }
    }
}

/// [`S3Port`] over the AWS SDK client.
#[derive(Debug, Clone)]
pub(crate) struct AwsS3 {
    client: aws_sdk_s3::Client,
    max_body: u64,
    page_size: Option<i32>,
}

impl AwsS3 {
    /// Builds the adapter from resolved SDK configuration.
    pub(crate) fn new(cfg: &SdkConfig, opts: Options) -> Self {
        let mut builder = aws_sdk_s3::config::Builder::from(cfg);
        if let Some(endpoint) = opts.endpoint {
            builder = builder.endpoint_url(endpoint).force_path_style(true);
        }
        Self {
            client: aws_sdk_s3::Client::from_conf(builder.build()),
            max_body: opts.max_body,
            page_size: opts.page_size,
        }
    }
}

#[async_trait]
impl S3Port for AwsS3 {
    async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError> {
        let out = self
            .client
            .list_buckets()
            .send()
            .await
            .map_err(|e| svc_error("buckets", e, false))?;
        let mut buckets: Vec<Bucket> = out
            .buckets()
            .iter()
            .map(|b| Bucket {
                name: b.name().unwrap_or_default().to_owned(),
                created: utc(b.creation_date()),
            })
            .collect();
        buckets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(buckets)
    }

    async fn list(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<Page, SvcError> {
        let out = self
            .client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(prefix)
            .delimiter("/")
            .set_continuation_token(token.map(str::to_owned))
            .set_max_keys(self.page_size)
            .send()
            .await
            .map_err(|e| svc_error(bucket, e, false))?;
        let objects = out
            .contents()
            .iter()
            .map(|o| ObjectMeta {
                bucket: bucket.to_owned(),
                key: o.key().unwrap_or_default().to_owned(),
                size: size(o.size()),
                modified: utc(o.last_modified()),
                etag: o.e_tag().unwrap_or_default().to_owned(),
            })
            .collect();
        let prefixes = out
            .common_prefixes()
            .iter()
            .filter_map(|p| p.prefix().map(str::to_owned))
            .collect();
        let next = if out.is_truncated().unwrap_or(false) {
            out.next_continuation_token().map(str::to_owned)
        } else {
            None
        };
        Ok(Page {
            objects,
            prefixes,
            next,
        })
    }

    async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError> {
        let what = format!("{bucket}/{key}");
        let mut versions = Vec::new();
        let mut key_marker = None;
        let mut version_id_marker = None;
        loop {
            let out = self
                .client
                .list_object_versions()
                .bucket(bucket)
                .prefix(key)
                .set_key_marker(key_marker)
                .set_version_id_marker(version_id_marker)
                .send()
                .await
                .map_err(|e| svc_error(&what, e, false))?;
            // The prefix also matches sibling keys that extend `key`.
            versions.extend(
                out.versions()
                    .iter()
                    .filter(|v| v.key() == Some(key))
                    .map(|v| Version {
                        id: v.version_id().unwrap_or_default().to_owned(),
                        modified: utc(v.last_modified()),
                        size: size(v.size()),
                        etag: v.e_tag().unwrap_or_default().to_owned(),
                        is_latest: v.is_latest().unwrap_or(false),
                        is_delete: false,
                    }),
            );
            versions.extend(
                out.delete_markers()
                    .iter()
                    .filter(|m| m.key() == Some(key))
                    .map(|m| Version {
                        id: m.version_id().unwrap_or_default().to_owned(),
                        modified: utc(m.last_modified()),
                        size: 0,
                        etag: String::new(),
                        is_latest: m.is_latest().unwrap_or(false),
                        is_delete: true,
                    }),
            );
            let saw_other_key = out
                .versions()
                .iter()
                .map(|v| v.key())
                .chain(out.delete_markers().iter().map(|m| m.key()))
                .any(|k| k != Some(key));
            let Some((next_key, next_version)) = next_markers(
                key,
                out.is_truncated().unwrap_or(false),
                saw_other_key,
                out.next_key_marker(),
                out.next_version_id_marker(),
            ) else {
                break;
            };
            key_marker = next_key;
            version_id_marker = next_version;
        }
        if versions.is_empty() {
            return Err(SvcError::not_found(what));
        }
        versions.sort_by_key(|v| Reverse(v.modified));
        Ok(versions)
    }

    async fn get_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Object, SvcError> {
        let requested = (!is_current(version_id)).then(|| version_id.to_owned());
        let what = match &requested {
            Some(id) => format!("{bucket}/{key}@{id}"),
            None => format!("{bucket}/{key}"),
        };
        let had_version_id = requested.is_some();
        let mut out = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .set_version_id(requested)
            .send()
            .await
            .map_err(|e| svc_error(&what, e, had_version_id))?;

        let max = self.max_body;
        if let Some(n) = out.content_length().filter(|&n| size(Some(n)) > max) {
            return Err(SvcError::Other(anyhow!(
                "{bucket}/{key}: object is {n} bytes, exceeds the {max} byte limit"
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = out.body.next().await {
            let chunk = chunk.map_err(|e| {
                anyhow::Error::from(e).context(format!("{bucket}/{key}: read body"))
            })?;
            if (body.len() + chunk.len()) as u64 > max {
                return Err(SvcError::Other(anyhow!(
                    "{bucket}/{key}: body exceeds the {max} byte limit"
                )));
            }
            body.extend_from_slice(&chunk);
        }

        let version_id = match out.version_id() {
            Some(id) if !id.is_empty() => id.to_owned(),
            _ => version_id.to_owned(),
        };
        Ok(Object {
            meta: ObjectMeta {
                bucket: bucket.to_owned(),
                key: key.to_owned(),
                size: body.len() as u64,
                modified: utc(out.last_modified()),
                etag: out.e_tag().unwrap_or_default().to_owned(),
            },
            version_id,
            body,
        })
    }

    async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError> {
        let what = format!("{bucket}/{key}");
        let out = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| svc_error(&what, e, false))?;
        Ok(ObjectMeta {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            size: size(out.content_length()),
            modified: utc(out.last_modified()),
            etag: out.e_tag().unwrap_or_default().to_owned(),
        })
    }
}

/// Converts an SDK failure into a [`SvcError`], naming `what` when it is a
/// not-found. `had_version_id` says whether the request named an explicit
/// version.
fn svc_error<E>(what: &str, err: SdkError<E, HttpResponse>, had_version_id: bool) -> SvcError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let status = err.raw_response().map(|r| r.status().as_u16());
    if is_not_found(err.code(), err.message(), status, had_version_id) {
        SvcError::not_found_from(what, err)
    } else {
        SvcError::Other(anyhow::Error::from(err))
    }
}

/// Whether an S3 error means the bucket, key or version does not exist.
///
/// S3 answers a malformed version id with `InvalidArgument` ("Invalid
/// version id specified"); that counts as not-found only when the request
/// named a version, since the same answer for a request without one points
/// at an unversioned bucket instead.
fn is_not_found(
    code: Option<&str>,
    message: Option<&str>,
    status: Option<u16>,
    had_version_id: bool,
) -> bool {
    if matches!(
        code,
        Some("NoSuchKey" | "NoSuchBucket" | "NoSuchVersion" | "NotFound")
    ) || status == Some(404)
    {
        return true;
    }
    had_version_id
        && code == Some("InvalidArgument")
        && message.is_some_and(|m| m.to_lowercase().contains("version id"))
}

/// The key and version-id markers for the next `ListObjectVersions` page of
/// `key`, or `None` when the walk is over.
///
/// Listings with `key` as the prefix are sorted by key, and `key` sorts
/// before every sibling it prefixes, so all of its entries come first. The
/// walk ends on the last page, once a page holds an entry for another key, or
/// when the next page would not resume within `key` — including a truncated
/// page that names no next key.
fn next_markers(
    key: &str,
    truncated: bool,
    saw_other_key: bool,
    next_key: Option<&str>,
    next_version: Option<&str>,
) -> Option<(Option<String>, Option<String>)> {
    if !truncated || saw_other_key || next_key != Some(key) {
        return None;
    }
    Some((Some(key.to_owned()), next_version.map(str::to_owned)))
}

/// An SDK timestamp in UTC.
fn utc(t: Option<&SdkDateTime>) -> Option<DateTime<Utc>> {
    t.and_then(|t| DateTime::from_timestamp(t.secs(), t.subsec_nanos()))
}

/// An SDK size in bytes; absent or negative counts as zero.
fn size(n: Option<i64>) -> u64 {
    n.and_then(|n| u64::try_from(n).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_not_found_shapes() {
        let cases = [
            (Some("NoSuchKey"), None, Some(404), false, true),
            (Some("NoSuchBucket"), None, Some(404), false, true),
            (Some("NoSuchVersion"), None, Some(404), true, true),
            (Some("NotFound"), None, Some(404), false, true),
            (None, None, Some(404), false, true),
            (Some("AccessDenied"), None, Some(403), false, false),
            (
                Some("InvalidArgument"),
                Some("Invalid version id specified"),
                Some(400),
                true,
                true,
            ),
            (
                Some("InvalidArgument"),
                Some("Invalid version id specified"),
                Some(400),
                false,
                false,
            ),
            (
                Some("InvalidArgument"),
                Some("Invalid continuation token"),
                Some(400),
                true,
                false,
            ),
        ];
        for (code, msg, status, had, want) in cases {
            assert_eq!(
                is_not_found(code, msg, status, had),
                want,
                "{code:?} {msg:?} {status:?} {had}"
            );
        }
    }

    #[test]
    fn version_paging_stops_once_the_key_is_exhausted() {
        const KEY: &str = "cfg/a.json";
        let markers = |k: &str, v: &str| Some((Some(k.to_owned()), Some(v.to_owned())));
        // More versions of the key itself: keep going.
        assert_eq!(
            next_markers(KEY, true, false, Some(KEY), Some("v9")),
            markers(KEY, "v9")
        );
        // The last page.
        assert_eq!(next_markers(KEY, false, false, Some(KEY), Some("v9")), None);
        // A sibling key showed up on this page: the key's entries are done.
        assert_eq!(next_markers(KEY, true, true, Some(KEY), Some("v9")), None);
        // The next page starts at a sibling key.
        assert_eq!(
            next_markers(KEY, true, false, Some("cfg/a.json.bak"), Some("v1")),
            None
        );
        // Truncated but without markers: no way to make progress.
        assert_eq!(next_markers(KEY, true, false, None, None), None);
    }
}

/// Runs against any versioned S3-compatible endpoint named by
/// `SS_SUPPORT_S3_TEST_ENDPOINT` (e.g. MinIO at `http://127.0.0.1:9000`); skipped
/// when it is unset. Credentials come from `SS_SUPPORT_S3_TEST_KEY` and
/// `SS_SUPPORT_S3_TEST_SECRET`, both defaulting to `minioadmin`.
#[cfg(test)]
mod minio {
    use std::time::{SystemTime, UNIX_EPOCH};

    use aws_config::{BehaviorVersion, Region, SdkConfig};
    use aws_sdk_s3::config::Credentials;
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
    use ss_core::svc::s3::S3Port;
    use ss_core::svc::s3::contract::{self, BODIES, Fixture, Paging};

    use super::{AwsS3, Options};

    const KEY: &str = "cfg/tenants/acme.json";
    const PAGE_PREFIX: &str = "cfg/many/";
    const PAGE_NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];

    /// A fresh versioned bucket holding the contract fixture.
    struct Seeded {
        cfg: SdkConfig,
        endpoint: String,
        bucket: String,
        /// Version ids of the three puts on [`KEY`], oldest first.
        ids: Vec<String>,
        /// Version id of the delete marker on [`KEY`].
        marker: String,
    }

    impl Seeded {
        fn port(&self, opts: Options) -> AwsS3 {
            AwsS3::new(
                &self.cfg,
                Options {
                    endpoint: Some(self.endpoint.clone()),
                    ..opts
                },
            )
        }
    }

    fn env_or(name: &str, default: &str) -> String {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_owned())
    }

    /// Creates and seeds the bucket, or returns `None` when no endpoint is
    /// configured.
    async fn seed() -> Option<Seeded> {
        let Some(endpoint) = std::env::var("SS_SUPPORT_S3_TEST_ENDPOINT")
            .ok()
            .filter(|v| !v.is_empty())
        else {
            eprintln!("skipping: SS_SUPPORT_S3_TEST_ENDPOINT is not set");
            return None;
        };
        let creds = Credentials::new(
            env_or("SS_SUPPORT_S3_TEST_KEY", "minioadmin"),
            env_or("SS_SUPPORT_S3_TEST_SECRET", "minioadmin"),
            None,
            None,
            "static",
        );
        let cfg = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(creds)
            .load()
            .await;
        let raw = aws_sdk_s3::Client::from_conf(
            aws_sdk_s3::config::Builder::from(&cfg)
                .endpoint_url(&endpoint)
                .force_path_style(true)
                .build(),
        );

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let bucket = format!("ss-it-{nanos}");
        raw.create_bucket()
            .bucket(&bucket)
            .send()
            .await
            .expect("create bucket");
        raw.put_bucket_versioning()
            .bucket(&bucket)
            .versioning_configuration(
                VersioningConfiguration::builder()
                    .status(BucketVersioningStatus::Enabled)
                    .build(),
            )
            .send()
            .await
            .expect("enable versioning");

        let put = |key: String, body: &'static str| {
            raw.put_object()
                .bucket(&bucket)
                .key(key)
                .body(ByteStream::from_static(body.as_bytes()))
                .send()
        };
        let put_id = |out: aws_sdk_s3::operation::put_object::PutObjectOutput| {
            out.version_id()
                .expect("put returns a version id")
                .to_owned()
        };

        let first = put_id(put(KEY.into(), BODIES[0]).await.expect("put first"));
        let second = put_id(put(KEY.into(), BODIES[1]).await.expect("put second"));
        let deleted = raw
            .delete_object()
            .bucket(&bucket)
            .key(KEY)
            .send()
            .await
            .expect("delete");
        let marker = deleted
            .version_id()
            .expect("delete returns the marker's version id")
            .to_owned();
        let third = put_id(put(KEY.into(), BODIES[2]).await.expect("put third"));
        for name in PAGE_NAMES {
            put(format!("{PAGE_PREFIX}{name}.json"), "{}")
                .await
                .expect("put paging key");
        }

        Some(Seeded {
            cfg,
            endpoint,
            bucket,
            ids: vec![first, second, third],
            marker,
        })
    }

    #[tokio::test]
    async fn contract_holds_against_s3() {
        let Some(s) = seed().await else { return };
        let port = s.port(Options::default());
        let paged = s.port(Options {
            page_size: Some(2),
            ..Options::default()
        });
        contract::run(&Fixture {
            port: &port,
            bucket: s.bucket.clone(),
            key: KEY.into(),
            bodies: BODIES.map(String::from).to_vec(),
            version_ids: s.ids.clone(),
            delete_marker_ids: vec![s.marker.clone()],
            paging: Paging {
                port: &paged,
                prefix: PAGE_PREFIX.into(),
                want_keys: PAGE_NAMES
                    .map(|name| format!("{PAGE_PREFIX}{name}.json"))
                    .to_vec(),
                page_size: 2,
            },
        })
        .await;
    }

    #[tokio::test]
    async fn max_body_is_enforced() {
        let Some(s) = seed().await else { return };
        let port = s.port(Options {
            max_body: 10,
            ..Options::default()
        });
        let err = port
            .get_version(&s.bucket, KEY, "")
            .await
            .expect_err("a body over the limit is refused");
        assert!(
            err.to_string().contains("exceeds the 10 byte limit"),
            "{err}"
        );
    }
}
