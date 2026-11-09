//! `s3.object-inspect`: metadata, version history, and a body preview for one
//! object.

use async_trait::async_trait;

use super::{port, short_id, suggest_buckets, suggest_keys, suggest_versions, timestamp};
use crate::guard::Class;
use crate::jsonutil;
use crate::ops::{Input, Meta, OpResult, Operation, Param, Runner, View};
use crate::svc::s3::Version;

/// The most body bytes shown for a body that is not JSON.
const PREVIEW_BYTES: usize = 4096;

/// Shows one object's metadata, its full version history, and a preview of
/// the body of one version.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjectInspect;

#[async_trait]
impl Operation for ObjectInspect {
    fn meta(&self) -> Meta {
        Meta {
            id: "s3.object-inspect",
            name: "Inspect object",
            summary: "Metadata, version history, and body preview for one object",
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
            Param::new("key")
                .label("Key")
                .required(true)
                .suggest(suggest_keys()),
            Param::new("version")
                .label("Version")
                .default("null")
                .help(r#"version to preview; "null" = current"#)
                .suggest(suggest_versions()),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let s3 = port(&r.deps)?;
        let (bucket, key) = (input.str("bucket"), input.str("key"));

        let versions = r
            .step("list versions", |_| {
                let s3 = s3.clone();
                async move { Ok(s3.list_versions(bucket, key).await?) }
            })
            .await?;
        let object = r
            .step("fetch body", |_| async {
                Ok(s3.get_version(bucket, key, input.str("version")).await?)
            })
            .await?;

        let summary = View::summary([
            ("Object", format!("{bucket}/{key}")),
            ("Version", object.version_label()),
            ("Size", format!("{} B", object.meta.size)),
            (
                "Modified",
                timestamp(object.meta.modified, "%Y-%m-%d %H:%M:%S %Z"),
            ),
            ("ETag", object.meta.etag.clone()),
            ("Versions", versions.len().to_string()),
        ]);
        let history = View::Table {
            title: "Version history".into(),
            cols: ["ID", "MODIFIED", "SIZE", ""].map(String::from).to_vec(),
            rows: versions.iter().map(history_row).collect(),
        };
        Ok(OpResult {
            views: vec![summary, history, body_preview(&object.body)],
        })
    }
}

/// One row of the version history table.
fn history_row(v: &Version) -> Vec<String> {
    let kind = if v.is_delete {
        "delete marker"
    } else if v.is_latest {
        "latest"
    } else {
        ""
    };
    vec![
        short_id(&v.id),
        timestamp(v.modified, "%Y-%m-%d %H:%M:%S"),
        v.size.to_string(),
        kind.to_owned(),
    ]
}

/// JSON bodies pretty-printed; any other text cut to [`PREVIEW_BYTES`] at a
/// character boundary; anything that is not text as a placeholder.
fn body_preview(body: &[u8]) -> View {
    if let Ok(value) = jsonutil::parse(body) {
        return View::Doc {
            title: "Body".into(),
            body: jsonutil::pretty(&value),
            lang: "json".into(),
        };
    }
    let head = &body[..body.len().min(PREVIEW_BYTES)];
    let text = match std::str::from_utf8(head) {
        Ok(text) => Some(text),
        // Only the cut, not the content, split the final character.
        Err(e) if e.error_len().is_none() && head.len() < body.len() => {
            std::str::from_utf8(&head[..e.valid_up_to()]).ok()
        }
        Err(_) => None,
    };
    match text {
        Some(text) => View::Doc {
            title: format!("Body (first {} bytes)", text.len()),
            body: text.to_owned(),
            lang: String::new(),
        },
        None => View::Doc {
            title: "Body".into(),
            body: format!("(binary, {} bytes)", body.len()),
            lang: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::s3::fixture;
    use crate::ops::{Deps, Input, Runner};
    use crate::svc::s3::FakeS3;

    const KEY: &str = "tenants/acme.json";
    const V1: &str = r#"{"tier":"gold","rules":[{"id":"r1","limit":10}],"updatedAt":"2026-07-01"}"#;
    const V2: &str = r#"{"tier":"platinum","rules":[{"id":"r0","limit":5},{"id":"r1","limit":10}],"updatedAt":"2026-07-02"}"#;

    fn input(pairs: &[(&str, &str)]) -> Input {
        pairs.iter().copied().collect()
    }

    async fn run(deps: Deps, key: &str) -> Vec<View> {
        Runner::new(deps)
            .execute(
                &ObjectInspect,
                input(&[("bucket", fixture::BUCKET), ("key", key)]),
            )
            .await
            .expect("inspect succeeds")
            .views
    }

    fn doc(title: &str, body: &str, lang: &str) -> View {
        View::Doc {
            title: title.into(),
            body: body.into(),
            lang: lang.into(),
        }
    }

    async fn body_of(body: impl Into<Vec<u8>>) -> View {
        let s3 = FakeS3::new();
        s3.insert(fixture::BUCKET, "blob", "b1", body, fixture::t0());
        let mut views = run(fixture::deps(s3), "blob").await;
        assert_eq!(views.len(), 3);
        views.remove(2)
    }

    #[tokio::test]
    async fn inspects_metadata_history_and_json_body() {
        let views = run(fixture::deps(fixture::fake()), KEY).await;
        assert_eq!(views.len(), 3);
        assert_eq!(
            views[0],
            View::summary([
                ("Object", "cfg-nonprod/tenants/acme.json".to_owned()),
                ("Version", "v2 (2026-07-02 10:00)".to_owned()),
                ("Size", format!("{} B", V2.len())),
                ("Modified", "2026-07-02 10:00:00 UTC".to_owned()),
                ("ETag", "v2-etag".to_owned()),
                ("Versions", "2".to_owned()),
            ])
        );
        let row = |id: &str, modified: &str, body: &str, kind: &str| {
            vec![
                id.into(),
                modified.into(),
                body.len().to_string(),
                kind.into(),
            ]
        };
        assert_eq!(
            views[1],
            View::Table {
                title: "Version history".into(),
                cols: ["ID", "MODIFIED", "SIZE", ""].map(String::from).to_vec(),
                rows: vec![
                    row("v2", "2026-07-02 10:00:00", V2, "latest"),
                    row("v1", "2026-07-01 10:00:00", V1, ""),
                ],
            }
        );
        let View::Doc { title, body, lang } = &views[2] else {
            panic!("third view is a doc");
        };
        assert_eq!((title.as_str(), lang.as_str()), ("Body", "json"));
        assert_eq!(
            *body,
            jsonutil::pretty(&jsonutil::parse(V2.as_bytes()).expect("valid json"))
        );
        assert!(body.contains("\"tier\": \"platinum\""), "{body}");
    }

    #[tokio::test]
    async fn the_version_param_selects_the_body_to_preview() {
        let views = Runner::new(fixture::deps(fixture::fake()))
            .execute(
                &ObjectInspect,
                input(&[("bucket", fixture::BUCKET), ("key", KEY), ("version", "v1")]),
            )
            .await
            .expect("inspect succeeds")
            .views;
        let View::Doc { body, .. } = &views[2] else {
            panic!("third view is a doc");
        };
        assert!(body.contains("\"tier\": \"gold\""), "{body}");
        let View::Summary { fields } = &views[0] else {
            panic!("first view is a summary");
        };
        assert_eq!(fields[1].1, "v1 (2026-07-01 10:00)");
        assert_eq!(fields[5].1, "2");
    }

    #[tokio::test]
    async fn text_body_preview() {
        let views = run(fixture::deps(fixture::fake()), "tenants/notes.txt").await;
        assert_eq!(views[2], doc("Body (first 12 bytes)", "hello\nthere\n", ""));
    }

    #[tokio::test]
    async fn long_text_is_cut_at_4096_bytes() {
        let View::Doc { title, body, lang } = body_of("a".repeat(5000)).await else {
            panic!("third view is a doc");
        };
        assert_eq!(title, "Body (first 4096 bytes)");
        assert_eq!(body.len(), 4096);
        assert_eq!(lang, "");
    }

    #[tokio::test]
    async fn a_cut_through_a_character_keeps_whole_characters() {
        let body = format!("{}é{}", "a".repeat(4095), "tail");
        let View::Doc { title, body, .. } = body_of(body).await else {
            panic!("third view is a doc");
        };
        assert_eq!(title, "Body (first 4095 bytes)");
        assert_eq!(body, "a".repeat(4095));
    }

    #[tokio::test]
    async fn binary_body_is_a_placeholder() {
        assert_eq!(
            body_of([0xff, 0xfe, 0x00]).await,
            doc("Body", "(binary, 3 bytes)", "")
        );
    }

    #[tokio::test]
    async fn a_body_ending_in_a_broken_character_is_binary() {
        assert_eq!(
            body_of([b'a', b'b', 0xc3]).await,
            doc("Body", "(binary, 3 bytes)", "")
        );
    }

    #[tokio::test]
    async fn history_marks_latest_and_delete_markers() {
        let s3 = FakeS3::new();
        s3.put(fixture::BUCKET, "k", "one");
        s3.delete(fixture::BUCKET, "k");
        s3.put(fixture::BUCKET, "k", "three");
        let views = run(fixture::deps(s3), "k").await;
        let View::Table { rows, .. } = &views[1] else {
            panic!("second view is a table");
        };
        let kinds: Vec<_> = rows.iter().map(|row| row[3].as_str()).collect();
        assert_eq!(kinds, ["latest", "delete marker", ""]);
    }

    #[tokio::test]
    async fn a_missing_object_fails_in_the_first_step() {
        let failure = Runner::new(fixture::deps(fixture::fake()))
            .execute(
                &ObjectInspect,
                input(&[("bucket", fixture::BUCKET), ("key", "nope")]),
            )
            .await
            .expect_err("unknown key fails");
        assert!(
            format!("{:#}", failure.error).starts_with("list versions: "),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn without_s3_the_run_fails_clearly() {
        let failure = Runner::new(Deps::default())
            .execute(
                &ObjectInspect,
                input(&[("bucket", fixture::BUCKET), ("key", KEY)]),
            )
            .await
            .expect_err("no port fails");
        assert!(
            format!("{:#}", failure.error).contains("S3 is not configured"),
            "{:#}",
            failure.error
        );
    }
}
