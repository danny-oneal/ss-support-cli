//! `s3.restore-version`: makes an earlier version of an object its current
//! one again.

use anyhow::bail;
use async_trait::async_trait;

use super::{port, short_id, suggest_buckets, suggest_keys, suggest_versions};
use crate::diff::Opts;
use crate::guard::Class;
use crate::ops::compare::{Comparison, compare_bodies};
use crate::ops::{Input, Meta, OpResult, Operation, Param, Runner, View};
use crate::svc::SvcError;
use crate::svc::s3::{Version, is_current};

/// Restores an earlier version of one S3 object by copying it onto the key,
/// which adds a version: the one that was current stays in the history.
///
/// Before anything is asked the operation shows the difference between the
/// current content and the version to restore. It refuses to proceed when
/// the bucket is not keeping versions of the object, since the copy would
/// then overwrite the current content for good, and it copies nothing when
/// the object's current version is no longer the one that was shown.
#[derive(Clone, Copy, Debug, Default)]
pub struct RestoreVersion;

#[async_trait]
impl Operation for RestoreVersion {
    fn meta(&self) -> Meta {
        Meta {
            id: "s3.restore-version",
            name: "Restore object version",
            summary: "Make an earlier version of an S3 object the current one again",
            category: "S3",
            class: Class::Mutate,
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
                .required(true)
                .help("the version to make current again; a version ID from the key's history")
                .suggest(suggest_versions()),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let s3 = port(&r.deps)?;
        let (bucket, key, version) = (input.str("bucket"), input.str("key"), input.str("version"));
        let object = format!("{bucket}/{key}");

        let history = r
            .step("read the version history", |_| async {
                Ok(s3.list_versions(bucket, key).await?)
            })
            .await?;
        let Some(current) = replaced_by_restoring(&object, &history, version)? else {
            return Ok(OpResult {
                views: vec![View::summary([
                    ("Object", object.as_str()),
                    ("Version", version),
                    ("Result", "already the current version; nothing was changed"),
                ])],
            });
        };

        let (restored, existing) = r
            .step("fetch the version and the current content", |_| async {
                let restored = s3.get_version(bucket, key, version).await?;
                let existing = if current.is_delete {
                    None
                } else {
                    Some(s3.get_version(bucket, key, &current.id).await?)
                };
                Ok((restored, existing))
            })
            .await?;
        let restored_label = restored.version_label();
        let replacing = match &existing {
            Some(existing) => existing.version_label(),
            None => "a delete marker (the object is deleted now)".to_owned(),
        };
        let opts = Opts {
            ignore: r.deps.diff.ignore.clone(),
            array_match: r.deps.diff.array_match.clone(),
            ..Opts::default()
        };
        // From what is there now to what will be there.
        let change = existing.as_ref().map(|existing| {
            match compare_bodies(&existing.body, &restored.body, &opts) {
                Comparison::Json(delta) => (
                    delta.stats.to_string(),
                    View::Diff {
                        left: replacing.clone(),
                        right: restored_label.clone(),
                        delta,
                    },
                ),
                Comparison::Lines(body) => (
                    "line diff (a body is not valid JSON)".to_owned(),
                    View::Doc {
                        title: format!("Line diff: {replacing} → {restored_label}"),
                        body,
                        lang: "diff".into(),
                    },
                ),
            }
        });
        let changes = change.as_ref().map_or(
            "the restored version becomes the object's content",
            |(stats, _)| stats.as_str(),
        );
        let mut preview = vec![View::summary([
            ("Object", object.as_str()),
            ("Restore", restored_label.as_str()),
            ("Replacing", replacing.as_str()),
            ("Changes", changes),
        ])];
        preview.extend(change.iter().map(|(_, view)| view.clone()));
        r.preview(preview);
        r.set_resource(object.clone());

        let created = r
            .mutate("copy the version onto the key", |_| async {
                // The object is still what the operator was shown.
                let latest = s3
                    .list_versions(bucket, key)
                    .await?
                    .into_iter()
                    .find(|v| v.is_latest);
                if latest.map(|v| v.id).as_deref() != Some(current.id.as_str()) {
                    return Err(SvcError::changed(format!(
                        "{object}: its current version is no longer {}",
                        short_id(&current.id)
                    ))
                    .into());
                }
                Ok(s3.copy_version(bucket, key, version).await?)
            })
            .await?;

        let previous = format!("{} (kept in the history)", current.id);
        let mut views = vec![View::summary([
            ("Object", object.as_str()),
            ("Restored version", restored_label.as_str()),
            ("New current version", created.as_str()),
            ("Previous current version", previous.as_str()),
        ])];
        views.extend(change.into_iter().map(|(_, view)| view));
        Ok(OpResult { views })
    }
}

/// The entry of `history` that restoring `version` of `object` would
/// replace as the current one, or `None` when `version` already is the
/// current version and there is nothing to do.
///
/// # Errors
///
/// Fails when the history has no such version, when the version is a delete
/// marker, when it has no id of its own to be copied by, and when the
/// current entry has no id, which means the bucket would not keep it.
fn replaced_by_restoring<'a>(
    object: &str,
    history: &'a [Version],
    version: &str,
) -> anyhow::Result<Option<&'a Version>> {
    let target = match history.iter().find(|v| v.id == version) {
        Some(target) => target,
        // Without an entry of that id, the id names the current version.
        None if is_current(version) => return Ok(None),
        None => bail!("{object} has no version {version:?}"),
    };
    if target.is_delete {
        bail!(
            "version {version} of {object} is a delete marker; restoring it would delete the object"
        );
    }
    if target.is_latest {
        return Ok(None);
    }
    if is_current(version) {
        bail!(
            "version {version:?} of {object} has no id of its own (it was written while the bucket kept no versions), so it cannot be named for a copy"
        );
    }
    let Some(current) = history.iter().find(|v| v.is_latest) else {
        bail!("{object} has no current version");
    };
    if is_current(&current.id) {
        bail!(
            "{object}: the bucket is not keeping versions of this object, so restoring would overwrite its current content for good"
        );
    }
    Ok(Some(current))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::ops::s3::fixture;
    use crate::ops::{ConfirmSpec, Confirmer, FailureKind, RunFailure, find_cause};
    use crate::svc::SvcError;
    use crate::svc::s3::{FakeS3, S3Port};

    const KEY: &str = "tenants/acme.json";

    /// Answers every confirmation the same way and keeps what it was shown.
    struct Asked {
        answer: bool,
        seen: Mutex<Vec<ConfirmSpec>>,
        /// Runs while the operator is deciding.
        meanwhile: Box<dyn Fn() + Send + Sync>,
    }

    impl Asked {
        fn answering(answer: bool) -> Arc<Asked> {
            Asked::after(answer, || {})
        }

        fn after(answer: bool, meanwhile: impl Fn() + Send + Sync + 'static) -> Arc<Asked> {
            Arc::new(Asked {
                answer,
                seen: Mutex::new(Vec::new()),
                meanwhile: Box::new(meanwhile),
            })
        }

        fn times(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Confirmer for Asked {
        async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool> {
            (self.meanwhile)();
            self.seen.lock().unwrap().push(spec);
            Ok(self.answer)
        }
    }

    fn input(key: &str, version: &str) -> Input {
        [
            ("bucket", fixture::BUCKET),
            ("key", key),
            ("version", version),
        ]
        .into_iter()
        .collect()
    }

    async fn restore(
        s3: &FakeS3,
        asked: &Arc<Asked>,
        key: &str,
        version: &str,
    ) -> Result<OpResult, RunFailure> {
        Runner::new(fixture::deps(s3.clone()))
            .confirmer(asked.clone())
            .execute(&RestoreVersion, input(key, version))
            .await
    }

    async fn ids(s3: &FakeS3, key: &str) -> Vec<String> {
        s3.list_versions(fixture::BUCKET, key)
            .await
            .expect("history")
            .into_iter()
            .map(|v| v.id)
            .collect()
    }

    async fn current_body(s3: &FakeS3, key: &str) -> String {
        let object = s3
            .get_version(fixture::BUCKET, key, "")
            .await
            .expect("current version");
        String::from_utf8(object.body).expect("text")
    }

    fn summary(view: &View) -> Vec<(&str, &str)> {
        let View::Summary { fields } = view else {
            panic!("expected a summary, got {view:?}");
        };
        fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn field<'a>(view: &'a View, name: &str) -> &'a str {
        summary(view)
            .into_iter()
            .find_map(|(k, v)| (k == name).then_some(v))
            .unwrap_or_else(|| panic!("no {name} in {view:?}"))
    }

    #[tokio::test]
    async fn restoring_an_older_version_makes_its_body_current_and_keeps_the_history() {
        let s3 = fixture::fake();
        let asked = Asked::answering(true);

        let result = restore(&s3, &asked, KEY, "v1").await.expect("restored");

        let history = ids(&s3, KEY).await;
        assert_eq!(history.len(), 3, "{history:?}");
        assert_eq!(&history[1..], ["v2", "v1"]);
        assert!(current_body(&s3, KEY).await.contains(r#""tier":"gold""#));
        assert_eq!(
            field(&result.views[0], "Object"),
            "cfg-nonprod/tenants/acme.json"
        );
        assert_eq!(field(&result.views[0], "New current version"), history[0]);
        assert!(
            field(&result.views[0], "Previous current version").starts_with("v2"),
            "{:?}",
            result.views[0]
        );
        assert_eq!(asked.times(), 1);
    }

    #[tokio::test]
    async fn the_preview_is_the_diff_from_the_current_version_to_the_restored_one() {
        let s3 = fixture::fake();
        let asked = Asked::answering(true);

        restore(&s3, &asked, KEY, "v1").await.expect("restored");

        let seen = asked.seen.lock().unwrap();
        let spec = &seen[0];
        assert_eq!(spec.resource, "cfg-nonprod/tenants/acme.json");
        assert_eq!(spec.class, Class::Mutate);
        assert_eq!(field(&spec.preview[0], "Restore"), "v1 (2026-07-01 10:00)");
        assert_eq!(
            field(&spec.preview[0], "Replacing"),
            "v2 (2026-07-02 10:00)"
        );
        let View::Diff { left, right, delta } = &spec.preview[1] else {
            panic!("the second preview view is a diff: {:?}", spec.preview);
        };
        assert_eq!(
            (left.as_str(), right.as_str()),
            ("v2 (2026-07-02 10:00)", "v1 (2026-07-01 10:00)")
        );
        // From platinum back to gold, and the added rule goes again.
        assert_eq!((delta.stats.removed, delta.stats.changed), (1, 1));
    }

    #[tokio::test]
    async fn restoring_the_current_version_does_nothing_and_asks_nothing() {
        for version in ["v2", "null"] {
            let s3 = fixture::fake();
            let asked = Asked::answering(true);

            let result = restore(&s3, &asked, KEY, version)
                .await
                .expect("nothing to do");

            assert_eq!(asked.times(), 0, "{version}");
            assert_eq!(ids(&s3, KEY).await, ["v2", "v1"], "{version}");
            assert!(
                field(&result.views[0], "Result").contains("already the current version"),
                "{:?}",
                result.views[0]
            );
        }
    }

    #[tokio::test]
    async fn a_delete_marker_cannot_be_restored() {
        let s3 = fixture::fake();
        let marker = s3.delete(fixture::BUCKET, KEY);
        s3.put(fixture::BUCKET, KEY, "{}");
        let asked = Asked::answering(true);

        let failure = restore(&s3, &asked, KEY, &marker)
            .await
            .expect_err("a delete marker holds nothing to restore");

        assert_eq!(failure.kind, FailureKind::Failed);
        assert!(
            format!("{:#}", failure.error).contains("is a delete marker"),
            "{:#}",
            failure.error
        );
        assert_eq!(asked.times(), 0);
        assert_eq!(ids(&s3, KEY).await.len(), 4);
    }

    #[tokio::test]
    async fn a_bucket_that_keeps_no_versions_is_refused_before_anything_is_asked() {
        let s3 = fixture::fake();
        // Written while versioning was off: the current version has no id.
        s3.insert(
            fixture::BUCKET,
            KEY,
            "null",
            r#"{"tier":"silver"}"#,
            fixture::t0() + chrono::Duration::hours(48),
        );
        let asked = Asked::answering(true);

        let failure = restore(&s3, &asked, KEY, "v1")
            .await
            .expect_err("the current content would be lost");

        assert!(
            format!("{:#}", failure.error).contains("is not keeping versions"),
            "{:#}",
            failure.error
        );
        assert_eq!(asked.times(), 0);
        assert_eq!(ids(&s3, KEY).await, ["null", "v2", "v1"]);
    }

    #[tokio::test]
    async fn a_version_without_an_id_of_its_own_cannot_be_restored() {
        let s3 = FakeS3::new();
        s3.insert(fixture::BUCKET, KEY, "null", "{}", fixture::t0());
        s3.insert(
            fixture::BUCKET,
            KEY,
            "v2",
            r#"{"a":1}"#,
            fixture::t0() + chrono::Duration::hours(1),
        );
        let asked = Asked::answering(true);

        let failure = restore(&s3, &asked, KEY, "null")
            .await
            .expect_err("that version cannot be named");

        assert!(
            format!("{:#}", failure.error).contains("has no id of its own"),
            "{:#}",
            failure.error
        );
        assert_eq!(asked.times(), 0);
        assert_eq!(ids(&s3, KEY).await, ["v2", "null"]);
    }

    #[tokio::test]
    async fn an_object_that_is_currently_deleted_can_be_restored() {
        let s3 = fixture::fake();
        s3.delete(fixture::BUCKET, KEY);
        let asked = Asked::answering(true);

        restore(&s3, &asked, KEY, "v2").await.expect("restored");

        assert!(current_body(&s3, KEY).await.contains("platinum"));
        let seen = asked.seen.lock().unwrap();
        assert!(
            field(&seen[0].preview[0], "Replacing").contains("delete marker"),
            "{:?}",
            seen[0].preview[0]
        );
        assert_eq!(
            seen[0].preview.len(),
            1,
            "there is no content to compare with"
        );
    }

    #[tokio::test]
    async fn a_version_that_is_not_in_the_history_fails_naming_it() {
        let s3 = fixture::fake();
        let asked = Asked::answering(true);

        let failure = restore(&s3, &asked, KEY, "v9")
            .await
            .expect_err("no such version");

        assert_eq!(failure.kind, FailureKind::Failed);
        let message = format!("{:#}", failure.error);
        assert!(
            message.contains("v9") && message.contains("cfg-nonprod/tenants/acme.json"),
            "{message}"
        );
        assert_eq!(asked.times(), 0);
    }

    #[tokio::test]
    async fn a_change_after_the_preview_stops_the_restore() {
        let s3 = fixture::fake();
        let writer = s3.clone();
        let asked = Asked::after(true, move || {
            writer.put(fixture::BUCKET, KEY, r#"{"tier":"written meanwhile"}"#);
        });

        let failure = restore(&s3, &asked, KEY, "v1")
            .await
            .expect_err("the object is not what was previewed");

        let cause = find_cause::<SvcError>(&failure.error).expect("a service error");
        assert!(cause.is_changed(), "{:#}", failure.error);
        assert_eq!(failure.kind, FailureKind::Failed);
        // Nothing was copied over the other writer's version.
        assert_eq!(ids(&s3, KEY).await.len(), 3);
        assert!(current_body(&s3, KEY).await.contains("written meanwhile"));
    }

    #[tokio::test]
    async fn a_declined_restore_copies_nothing() {
        let s3 = fixture::fake();
        let asked = Asked::answering(false);

        let failure = restore(&s3, &asked, KEY, "v1").await.expect_err("declined");

        assert_eq!(failure.kind, FailureKind::Declined);
        assert_eq!(ids(&s3, KEY).await, ["v2", "v1"]);
    }

    #[tokio::test]
    async fn a_dry_run_previews_and_copies_nothing() {
        let s3 = fixture::fake();
        let asked = Asked::answering(true);

        let result = Runner::new(fixture::deps(s3.clone()))
            .confirmer(asked.clone())
            .dry_run(true)
            .execute(&RestoreVersion, input(KEY, "v1"))
            .await
            .expect("a dry run succeeds");

        assert_eq!(asked.times(), 0);
        assert_eq!(ids(&s3, KEY).await, ["v2", "v1"]);
        assert!(
            result
                .views
                .iter()
                .any(|view| matches!(view, View::Diff { .. })),
            "the dry run shows what would change: {:?}",
            result.views
        );
    }

    #[tokio::test]
    async fn yes_is_enough_to_confirm_a_restore() {
        let s3 = fixture::fake();

        Runner::new(fixture::deps(s3.clone()))
            .yes(true)
            .execute(&RestoreVersion, input(KEY, "v1"))
            .await
            .expect("restored");

        assert_eq!(ids(&s3, KEY).await.len(), 3);
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_previews_as_a_line_diff() {
        let s3 = fixture::fake();
        let asked = Asked::answering(true);

        restore(&s3, &asked, "tenants/notes.txt", "t1")
            .await
            .expect("restored");

        assert_eq!(
            current_body(&s3, "tenants/notes.txt").await,
            "hello\nworld\n"
        );
        let seen = asked.seen.lock().unwrap();
        let View::Doc { body, lang, .. } = &seen[0].preview[1] else {
            panic!(
                "the second preview view is a line diff: {:?}",
                seen[0].preview
            );
        };
        assert_eq!(lang, "diff");
        assert!(body.contains("-there") && body.contains("+world"), "{body}");
    }

    #[tokio::test]
    async fn a_missing_object_fails_without_asking() {
        let s3 = fixture::fake();
        let asked = Asked::answering(true);

        let failure = restore(&s3, &asked, "tenants/nope.json", "v1")
            .await
            .expect_err("no such object");

        let cause = find_cause::<SvcError>(&failure.error).expect("a service error");
        assert!(cause.is_not_found(), "{:#}", failure.error);
        assert_eq!(asked.times(), 0);
    }
}
