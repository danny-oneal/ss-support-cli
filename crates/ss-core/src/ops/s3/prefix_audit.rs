//! `s3.prefix-audit`: object count, size, age spread, and the largest objects
//! under a prefix.

use async_trait::async_trait;
use chrono::Utc;

use super::{age, human_size, port, suggest_buckets, suggest_keys, timestamp};
use crate::guard::Class;
use crate::ops::{Input, Meta, OpResult, Operation, Param, ParamType, Runner, View};
use crate::svc::SvcError;
use crate::svc::s3::{ObjectMeta, S3Port};

/// Top-N size used when `top` is not positive.
const DEFAULT_TOP: usize = 10;

/// Summarizes what lives under a prefix: how many objects, how large, how
/// old, and which are the largest.
#[derive(Clone, Copy, Debug, Default)]
pub struct PrefixAudit;

/// The objects found under a prefix.
struct Scan {
    objects: Vec<ObjectMeta>,
    /// The scan stopped because the object limit was reached.
    truncated: bool,
}

/// Walks `prefix` and every prefix below it depth first. For each page, its
/// objects are taken, then each of its prefixes is scanned completely, then
/// the next page is read. Scanning stops as soon as `limit` objects have been
/// collected, without descending any further.
async fn scan(
    s3: &dyn S3Port,
    bucket: &str,
    prefix: &str,
    limit: Option<usize>,
) -> Result<Scan, SvcError> {
    let mut objects = Vec::new();
    // Pending listings; the last entry is read next.
    let mut pending = vec![(prefix.to_owned(), None::<String>)];
    while let Some((prefix, token)) = pending.pop() {
        let page = s3.list(bucket, &prefix, token.as_deref()).await?;
        objects.extend(page.objects);
        if limit.is_some_and(|limit| objects.len() >= limit) {
            return Ok(Scan {
                objects,
                truncated: true,
            });
        }
        if let Some(next) = page.next {
            pending.push((prefix, Some(next)));
        }
        pending.extend(page.prefixes.into_iter().rev().map(|p| (p, None)));
    }
    Ok(Scan {
        objects,
        truncated: false,
    })
}

#[async_trait]
impl Operation for PrefixAudit {
    fn meta(&self) -> Meta {
        Meta {
            id: "s3.prefix-audit",
            name: "Audit prefix",
            summary: "Object count, size, age spread, and largest objects under a prefix",
            category: "S3",
            class: Class::Read,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::new("bucket")
                .label("Bucket")
                .required(true)
                .suggest(suggest_buckets()),
            Param::new("prefix")
                .label("Prefix")
                .help("empty = whole bucket")
                .suggest(suggest_keys()),
            Param::new("max")
                .label("Max objects")
                .ty(ParamType::Int)
                .default("10000")
                .help("stop scanning after this many objects"),
            Param::new("top")
                .label("Top N largest")
                .ty(ParamType::Int)
                .default("10"),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let s3 = port(&r.deps)?;
        let (bucket, prefix) = (input.str("bucket"), input.str("prefix"));
        let max = input.int("max");
        let limit = usize::try_from(max).ok().filter(|&n| n > 0);
        let top = usize::try_from(input.int("top"))
            .ok()
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_TOP);

        let Scan { objects, truncated } = r
            .step("scan prefix", |_| {
                let s3 = s3.clone();
                async move { Ok(scan(&*s3, bucket, prefix, limit).await?) }
            })
            .await?;

        let location = format!("{bucket}/{prefix}");
        if objects.is_empty() {
            return Ok(OpResult {
                views: vec![View::summary([
                    ("Prefix", location),
                    ("Objects", "0".to_owned()),
                ])],
            });
        }

        let total: u64 = objects.iter().map(|o| o.size).sum();
        let oldest = objects
            .iter()
            .reduce(|best, o| if o.modified < best.modified { o } else { best })
            .expect("objects is not empty");
        let newest = objects
            .iter()
            .reduce(|best, o| if o.modified > best.modified { o } else { best })
            .expect("objects is not empty");
        let now = Utc::now();
        let dated = |o: &ObjectMeta| format!("{} ({})", o.key, age(o.modified, now));

        let mut count = objects.len().to_string();
        if truncated {
            count.push_str(&format!(" (stopped at --max {max})"));
        }
        let summary = View::summary([
            ("Prefix", location),
            ("Objects", count),
            ("Total size", human_size(total)),
            ("Newest", dated(newest)),
            ("Oldest", dated(oldest)),
        ]);

        let mut largest: Vec<&ObjectMeta> = objects.iter().collect();
        largest.sort_by_key(|o| std::cmp::Reverse(o.size));
        let rows: Vec<Vec<String>> = largest
            .into_iter()
            .take(top)
            .map(|o| {
                vec![
                    o.key.clone(),
                    human_size(o.size),
                    timestamp(o.modified, "%Y-%m-%d %H:%M"),
                ]
            })
            .collect();
        let table = View::Table {
            title: format!("Top {} by size", rows.len()),
            cols: ["KEY", "SIZE", "MODIFIED"].map(String::from).to_vec(),
            rows,
        };
        Ok(OpResult {
            views: vec![summary, table],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::s3::{fixture, human_size};
    use crate::ops::{Deps, Input, Runner, View};
    use crate::svc::s3::FakeS3;
    use chrono::Duration;

    fn input(pairs: &[(&str, &str)]) -> Input {
        pairs.iter().copied().collect()
    }

    /// `logs/000.json` .. `logs/{n-1}.json`, one hour apart, listed in pages
    /// of `page_size` entries (unpaged when zero).
    fn logs(n: i64, page_size: usize) -> Deps {
        let fake = fixture::fake();
        for i in 0..n {
            fake.insert(
                fixture::BUCKET,
                &format!("logs/{i:03}.json"),
                "v1",
                format!(r#"{{"n":{i}}}"#),
                fixture::t0() + Duration::hours(i),
            );
        }
        if page_size > 0 {
            fixture::deps(fake.paged(page_size))
        } else {
            fixture::deps(fake)
        }
    }

    async fn run(deps: Deps, pairs: &[(&str, &str)]) -> Vec<View> {
        Runner::new(deps)
            .execute(&PrefixAudit, input(pairs))
            .await
            .expect("audit succeeds")
            .views
    }

    fn field(views: &[View], name: &str) -> String {
        let View::Summary { fields } = &views[0] else {
            panic!("first view is a summary");
        };
        fields
            .iter()
            .find(|(k, _)| k == name)
            .unwrap_or_else(|| panic!("no {name} field in {fields:?}"))
            .1
            .clone()
    }

    #[tokio::test]
    async fn audits_the_fixture_prefix() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[("bucket", fixture::BUCKET), ("prefix", "tenants/")],
        )
        .await;
        assert_eq!(field(&views, "Objects"), "2");
    }

    #[tokio::test]
    async fn drains_every_page() {
        let views = run(
            logs(7, 2),
            &[("bucket", fixture::BUCKET), ("prefix", "logs/")],
        )
        .await;
        assert_eq!(field(&views, "Objects"), "7");
    }

    #[tokio::test]
    async fn page_size_does_not_change_the_answer() {
        let args = [("bucket", fixture::BUCKET), ("prefix", "logs/")];
        let unpaged = run(logs(6, 0), &args).await;
        for page in [1, 2, 3, 5, 6, 7, 100] {
            let views = run(logs(6, page), &args).await;
            assert_eq!(field(&views, "Objects"), "6", "page size {page}");
            assert_eq!(
                field(&views, "Total size"),
                field(&unpaged, "Total size"),
                "page size {page}"
            );
        }
    }

    #[tokio::test]
    async fn max_stops_within_one_page() {
        let views = run(
            logs(20, 3),
            &[
                ("bucket", fixture::BUCKET),
                ("prefix", "logs/"),
                ("max", "5"),
            ],
        )
        .await;
        let objects = field(&views, "Objects");
        let count: usize = objects
            .strip_suffix(" (stopped at --max 5)")
            .unwrap_or_else(|| panic!("not marked as stopped: {objects}"))
            .parse()
            .expect("a count");
        assert!((5..=7).contains(&count), "{objects}");
    }

    #[tokio::test]
    async fn descends_into_nested_prefixes() {
        let fake = FakeS3::new();
        for key in ["logs/a.json", "logs/2026/b.json", "logs/2026/07/c.json"] {
            fake.insert(fixture::BUCKET, key, "v1", "{}", fixture::t0());
        }
        let views = run(
            fixture::deps(fake),
            &[("bucket", fixture::BUCKET), ("prefix", "logs/")],
        )
        .await;
        assert_eq!(field(&views, "Objects"), "3");
    }

    #[tokio::test]
    async fn max_stops_before_descending_into_prefixes() {
        let fake = FakeS3::new();
        for key in ["p/1", "p/2"] {
            fake.insert(fixture::BUCKET, key, "v1", "x", fixture::t0());
        }
        for i in 3..=9 {
            let key = format!("p/sub/{i}");
            fake.insert(fixture::BUCKET, &key, "v1", "x", fixture::t0());
        }
        let views = run(
            fixture::deps(fake.paged(3)),
            &[("bucket", fixture::BUCKET), ("prefix", "p/"), ("max", "2")],
        )
        .await;
        assert_eq!(field(&views, "Objects"), "2 (stopped at --max 2)");
    }

    #[tokio::test]
    async fn summary_and_top_table() {
        let fake = FakeS3::new();
        for (i, size) in [10_usize, 20, 30].into_iter().enumerate() {
            fake.insert(
                fixture::BUCKET,
                &format!("logs/{i:03}.json"),
                "v1",
                "x".repeat(size),
                fixture::t0() + Duration::hours(i as i64),
            );
        }
        let views = run(
            fixture::deps(fake),
            &[
                ("bucket", fixture::BUCKET),
                ("prefix", "logs/"),
                ("top", "2"),
            ],
        )
        .await;
        assert_eq!(views.len(), 2);
        let View::Summary { fields } = &views[0] else {
            panic!("first view is a summary");
        };
        let keys: Vec<_> = fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["Prefix", "Objects", "Total size", "Newest", "Oldest"]
        );
        assert_eq!(fields[0].1, "cfg-nonprod/logs/");
        assert_eq!(fields[1].1, "3");
        assert_eq!(fields[2].1, human_size(60));
        assert!(
            fields[3].1.starts_with("logs/002.json ("),
            "{}",
            fields[3].1
        );
        assert!(fields[3].1.ends_with(" ago)"), "{}", fields[3].1);
        assert!(
            fields[4].1.starts_with("logs/000.json ("),
            "{}",
            fields[4].1
        );
        let View::Table { title, cols, rows } = &views[1] else {
            panic!("second view is a table");
        };
        assert_eq!(title, "Top 2 by size");
        assert_eq!(cols, &["KEY", "SIZE", "MODIFIED"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "logs/002.json");
        assert_eq!(rows[0][1], human_size(30));
        assert_eq!(rows[0][2], "2026-07-01 12:00");
        assert_eq!(rows[1][0], "logs/001.json");
    }

    #[tokio::test]
    async fn empty_prefix_reports_zero() {
        let views = run(
            fixture::deps(fixture::fake()),
            &[("bucket", fixture::BUCKET), ("prefix", "none/")],
        )
        .await;
        assert_eq!(
            views,
            [View::summary([
                ("Prefix", "cfg-nonprod/none/"),
                ("Objects", "0")
            ])]
        );
    }

    #[tokio::test]
    async fn non_positive_top_means_ten() {
        let views = run(
            logs(12, 0),
            &[
                ("bucket", fixture::BUCKET),
                ("prefix", "logs/"),
                ("top", "0"),
            ],
        )
        .await;
        let View::Table { title, rows, .. } = &views[1] else {
            panic!("second view is a table");
        };
        assert_eq!(title, "Top 10 by size");
        assert_eq!(rows.len(), 10);
    }
}
