//! The S3 operations and the helpers they share: the bucket, key, and version
//! suggestion chain, and the formatting of sizes, ages, and timestamps.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use super::{Choice, Deps, Input, SuggestFn};
use crate::svc::s3::{CURRENT, S3Port};

#[cfg(test)]
pub(crate) mod fixture;
mod object_inspect;
mod version_diff;

pub use object_inspect::ObjectInspect;
pub use version_diff::VersionDiff;

/// The S3 port of `deps`, or an error when the invocation has none.
pub(crate) fn port(deps: &Deps) -> anyhow::Result<Arc<dyn S3Port>> {
    deps.s3
        .clone()
        .ok_or_else(|| anyhow::anyhow!("S3 is not configured"))
}

/// Suggests every bucket name.
pub(crate) fn suggest_buckets() -> SuggestFn {
    Arc::new(|deps: Deps, _input: Input| {
        Box::pin(async move {
            let Some(s3) = deps.s3 else {
                return Ok(Vec::new());
            };
            Ok(s3
                .list_buckets()
                .await?
                .into_iter()
                .map(|b| Choice {
                    value: b.name,
                    label: None,
                })
                .collect())
        })
    })
}

/// Suggests the prefixes and keys under the typed `key`, within `bucket`.
pub(crate) fn suggest_keys() -> SuggestFn {
    Arc::new(|deps: Deps, input: Input| {
        Box::pin(async move {
            let bucket = input.str("bucket");
            let Some(s3) = deps.s3.filter(|_| !bucket.is_empty()) else {
                return Ok(Vec::new());
            };
            let page = s3.list(bucket, input.str("key"), None).await?;
            let prefixes = page.prefixes.into_iter().map(|p| Choice {
                label: Some(format!("{p} …")),
                value: p,
            });
            let keys = page.objects.into_iter().map(|o| Choice {
                value: o.key,
                label: None,
            });
            Ok(prefixes.chain(keys).collect())
        })
    })
}

/// Suggests the versions of `key` in `bucket`, led by the current version.
pub(crate) fn suggest_versions() -> SuggestFn {
    Arc::new(|deps: Deps, input: Input| {
        Box::pin(async move {
            let (bucket, key) = (input.str("bucket"), input.str("key"));
            let Some(s3) = deps.s3.filter(|_| !bucket.is_empty() && !key.is_empty()) else {
                return Ok(Vec::new());
            };
            let versions = s3.list_versions(bucket, key).await?;
            let mut out = vec![Choice {
                value: CURRENT.to_owned(),
                label: Some("null — current version".to_owned()),
            }];
            out.extend(versions.into_iter().map(|v| {
                let mut label = format!(
                    "{} — {}, {} B",
                    short_id(&v.id),
                    timestamp(v.modified, "%Y-%m-%d %H:%M:%S"),
                    v.size
                );
                if v.is_latest {
                    label.push_str(" (latest)");
                }
                if v.is_delete {
                    label.push_str(" (delete marker)");
                }
                Choice {
                    value: v.id,
                    label: Some(label),
                }
            }));
            Ok(out)
        })
    })
}

/// `id`, cut to its first twelve characters and marked with `…` when longer.
pub(crate) fn short_id(id: &str) -> String {
    match id.char_indices().nth(12) {
        Some((cut, _)) => format!("{}…", &id[..cut]),
        None => id.to_owned(),
    }
}

/// `n` bytes in binary units: `512 B`, `1.5 KiB`, `5.0 GiB`.
pub(crate) fn human_size(n: u64) -> String {
    const UNITS: [char; 6] = ['K', 'M', 'G', 'T', 'P', 'E'];
    if n < 1024 {
        return format!("{n} B");
    }
    let (mut div, mut unit) = (1024_u64, 0);
    while unit + 1 < UNITS.len() && n / div >= 1024 {
        div *= 1024;
        unit += 1;
    }
    format!("{:.1} {}iB", n as f64 / div as f64, UNITS[unit])
}

/// How long before `now` the time `then` was, in whole minutes, hours, or
/// days; `unknown` when `then` is not known. Times after `now` count as zero.
pub(crate) fn age(then: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(then) = then else {
        return "unknown".to_owned();
    };
    let elapsed = (now - then).max(Duration::zero());
    if elapsed < Duration::hours(1) {
        format!("{}m ago", elapsed.num_minutes())
    } else if elapsed < Duration::hours(24) {
        format!("{}h ago", elapsed.num_hours())
    } else {
        format!("{}d ago", elapsed.num_days())
    }
}

/// `t` rendered with the chrono format `fmt`, or `-` when unknown.
pub(crate) fn timestamp(t: Option<DateTime<Utc>>, fmt: &str) -> String {
    t.map_or_else(|| "-".to_owned(), |t| t.format(fmt).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_truncates_past_twelve() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("abcdefghijkl"), "abcdefghijkl");
        assert_eq!(short_id("abcdefghijklm"), "abcdefghijkl…");
    }

    #[test]
    fn human_size_uses_binary_units() {
        for (n, want) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1 << 20, "1.0 MiB"),
            (5 << 30, "5.0 GiB"),
        ] {
            assert_eq!(human_size(n), want);
        }
    }

    #[test]
    fn age_is_minutes_hours_or_days() {
        let now = fixture::t0();
        for (ago, want) in [
            (Duration::minutes(30), "30m ago"),
            (Duration::seconds(3599), "59m ago"),
            (Duration::hours(1), "1h ago"),
            (Duration::hours(23), "23h ago"),
            (Duration::hours(24), "1d ago"),
            (Duration::hours(49), "2d ago"),
            (Duration::minutes(-5), "0m ago"),
        ] {
            assert_eq!(age(Some(now - ago), now), want);
        }
        assert_eq!(age(None, now), "unknown");
    }

    #[test]
    fn timestamp_formats_or_dashes() {
        assert_eq!(
            timestamp(Some(fixture::t0()), "%Y-%m-%d %H:%M:%S"),
            "2026-07-01 10:00:00"
        );
        assert_eq!(timestamp(None, "%Y-%m-%d"), "-");
    }

    #[test]
    fn port_requires_an_s3_dependency() {
        let err = port(&Deps::default()).err().expect("no port configured");
        assert_eq!(err.to_string(), "S3 is not configured");
        assert!(port(&fixture::deps(fixture::fake())).is_ok());
    }

    fn call(
        f: SuggestFn,
        d: &Deps,
        pairs: &[(&str, &str)],
    ) -> crate::ops::BoxFuture<anyhow::Result<Vec<Choice>>> {
        f(d.clone(), pairs.iter().copied().collect())
    }

    fn plain(value: &str) -> Choice {
        Choice {
            value: value.into(),
            label: None,
        }
    }

    #[tokio::test]
    async fn suggestions_chain_bucket_key_version() -> anyhow::Result<()> {
        let d = fixture::deps(fixture::fake());
        assert_eq!(
            call(suggest_buckets(), &d, &[]).await?,
            [plain("cfg-nonprod")]
        );
        assert_eq!(
            call(suggest_keys(), &d, &[("bucket", "cfg-nonprod")]).await?,
            [Choice {
                value: "tenants/".into(),
                label: Some("tenants/ …".into())
            }]
        );
        let keys = call(
            suggest_keys(),
            &d,
            &[("bucket", "cfg-nonprod"), ("key", "tenants/")],
        )
        .await?;
        assert_eq!(
            keys,
            [plain("tenants/acme.json"), plain("tenants/notes.txt")]
        );
        let versions = call(
            suggest_versions(),
            &d,
            &[("bucket", "cfg-nonprod"), ("key", "tenants/acme.json")],
        )
        .await?;
        let v2_len = r#"{"tier":"platinum","rules":[{"id":"r0","limit":5},{"id":"r1","limit":10}],"updatedAt":"2026-07-02"}"#.len();
        let v1_len =
            r#"{"tier":"gold","rules":[{"id":"r1","limit":10}],"updatedAt":"2026-07-01"}"#.len();
        assert_eq!(
            versions,
            [
                Choice {
                    value: "null".into(),
                    label: Some("null — current version".into())
                },
                Choice {
                    value: "v2".into(),
                    label: Some(format!("v2 — 2026-07-02 10:00:00, {v2_len} B (latest)"))
                },
                Choice {
                    value: "v1".into(),
                    label: Some(format!("v1 — 2026-07-01 10:00:00, {v1_len} B"))
                },
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn version_labels_flag_delete_markers() -> anyhow::Result<()> {
        let s3 = fixture::fake();
        let marker = s3.delete(fixture::BUCKET, "tenants/acme.json");
        let d = fixture::deps(s3);
        let versions = call(
            suggest_versions(),
            &d,
            &[("bucket", "cfg-nonprod"), ("key", "tenants/acme.json")],
        )
        .await?;
        assert_eq!(versions[1].value, marker);
        let label = versions[1].label.as_deref().expect("labelled");
        assert!(label.ends_with(", 0 B (latest) (delete marker)"), "{label}");
        Ok(())
    }

    #[tokio::test]
    async fn suggestions_are_empty_without_a_port_or_a_parent_value() -> anyhow::Result<()> {
        let none = Deps::default();
        let some = fixture::deps(fixture::fake());
        let pairs: &[(&str, &str)] = &[("bucket", "cfg-nonprod"), ("key", "tenants/acme.json")];
        for make in [suggest_buckets, suggest_keys, suggest_versions] {
            assert!(call(make(), &none, pairs).await?.is_empty());
        }
        assert!(call(suggest_keys(), &some, &[]).await?.is_empty());
        assert!(
            call(suggest_versions(), &some, &[("bucket", "cfg-nonprod")])
                .await?
                .is_empty()
        );
        assert!(
            call(suggest_versions(), &some, &[("key", "tenants/acme.json")])
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn suggestion_errors_propagate() {
        let d = fixture::deps(fixture::fake());
        assert!(
            call(suggest_keys(), &d, &[("bucket", "missing")])
                .await
                .is_err()
        );
        assert!(
            call(
                suggest_versions(),
                &d,
                &[("bucket", "missing"), ("key", "k")]
            )
            .await
            .is_err()
        );
    }
}
