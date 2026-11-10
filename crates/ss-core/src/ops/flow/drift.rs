//! `config.drift`: whether the cluster runs the configuration stored in S3.

use async_trait::async_trait;

use super::{configmap_body, json_delta, kube_port, split_configmap_spec};
use crate::diff::Opts;
use crate::guard::Class;
use crate::ops::s3::{port, suggest_buckets, suggest_keys, suggest_versions};
use crate::ops::{Input, Meta, OpResult, Operation, Param, ParamType, Runner, View};

/// Diffs an S3 config object against the ConfigMap deployed in the cluster
/// and reports whether they agree.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConfigDrift;

#[async_trait]
impl Operation for ConfigDrift {
    fn meta(&self) -> Meta {
        Meta {
            id: "config.drift",
            name: "Config drift check",
            summary: "Diff an S3 config object against a live Kubernetes ConfigMap",
            category: "Cross-service",
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
                .label("S3 version")
                .default("null")
                .help(r#"version to compare; "null" = current"#)
                .suggest(suggest_versions()),
            Param::new("configmap")
                .label("ConfigMap")
                .required(true)
                .help("name, or name:data-key when the map holds several files"),
            Param::new("namespace")
                .label("Namespace")
                .default("default"),
            Param::new("ignore")
                .label("Ignore paths")
                .ty(ParamType::Json)
                .help(r#"extra ignore globs, e.g. "**.updatedAt""#),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let s3 = port(&r.deps)?;
        let k8s = kube_port(&r.deps)?;
        let (bucket, key) = (input.str("bucket"), input.str("key"));
        let (namespace, spec) = (input.str("namespace"), input.str("configmap"));

        let object = r
            .step("fetch S3 config", |_| async {
                Ok(s3.get_version(bucket, key, input.str("version")).await?)
            })
            .await?;
        let version_label = object.version_label();

        let opts = Opts {
            ignore: r
                .deps
                .diff
                .ignore
                .iter()
                .cloned()
                .chain(input.globs("ignore"))
                .collect(),
            array_match: r.deps.diff.array_match.clone(),
            ..Opts::default()
        };
        let delta = r
            .step("diff live ConfigMap", |_| async {
                let (data_key, body) = configmap_body(k8s.as_ref(), namespace, spec).await?;
                json_delta(&object.body, &data_key, &body, &opts)
            })
            .await?;

        // A successful fetch means the spec named a ConfigMap.
        let name = split_configmap_spec(spec).map_or(spec, |(name, _)| name);
        let in_sync = delta.stats.is_empty();
        let result = if in_sync {
            "in sync — the cluster is running this config".to_owned()
        } else {
            format!("DRIFT ({})", delta.stats)
        };
        let mut views = vec![View::summary([
            ("S3 object", format!("{bucket}/{key} @ {version_label}")),
            ("ConfigMap", format!("{namespace}/{name}")),
            ("Result", result),
        ])];
        if !in_sync {
            views.push(View::Diff {
                left: format!("S3 {version_label}"),
                right: format!("ConfigMap {name}"),
                delta,
            });
        }
        Ok(OpResult { views })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Kind;
    use crate::ops::flow::fixture::{self, BUCKET, CONFIGMAP, KEY};
    use crate::ops::{Deps, FailureKind, Input, RunFailure, Runner, RunnerError, View, find_cause};
    use crate::svc::SvcError;

    fn input(pairs: &[(&str, &str)]) -> Input {
        pairs.iter().copied().collect()
    }

    fn base(configmap: &str) -> Vec<(&str, &str)> {
        vec![
            ("bucket", BUCKET),
            ("key", KEY),
            ("configmap", configmap),
            ("namespace", "menu"),
        ]
    }

    async fn run(deps: Deps, pairs: &[(&str, &str)]) -> Vec<View> {
        Runner::new(deps)
            .execute(&ConfigDrift, input(pairs))
            .await
            .expect("drift check succeeds")
            .views
    }

    async fn fail(deps: Deps, pairs: &[(&str, &str)]) -> RunFailure {
        Runner::new(deps)
            .execute(&ConfigDrift, input(pairs))
            .await
            .expect_err("drift check fails")
    }

    #[tokio::test]
    async fn a_stale_configmap_is_reported_as_drift() {
        let views = run(fixture::deps(), &base("menu-config:menu.json")).await;
        assert_eq!(views.len(), 2);
        let View::Summary { fields } = &views[0] else {
            panic!("first view is a summary: {views:?}");
        };
        let rows: std::collections::BTreeMap<_, _> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            rows["S3 object"],
            "ss-config-dev/menu/12345.json @ ver-2 (2026-08-01 11:00)"
        );
        assert_eq!(rows["ConfigMap"], "menu/menu-config");
        assert!(rows["Result"].starts_with("DRIFT ("), "{rows:?}");
        let View::Diff { left, right, delta } = &views[1] else {
            panic!("second view is a diff");
        };
        assert_eq!(left, "S3 ver-2 (2026-08-01 11:00)");
        assert_eq!(right, "ConfigMap menu-config");
        assert_eq!(delta.changes.len(), 1, "{:?}", delta.changes);
        assert_eq!(delta.changes[0].path.dotted(), "items[id=latte].price");
    }

    #[tokio::test]
    async fn a_matching_configmap_is_in_sync() {
        let fakes = fixture::fakes();
        fakes.kubectl.insert(
            "menu",
            "configmap",
            CONFIGMAP,
            r#"{"data":{"menu.json":"{\"items\":[{\"id\":\"latte\",\"price\":6},{\"id\":\"mocha\",\"price\":7}],\"updatedAt\":\"zzz\"}"}}"#,
        );
        let views = run(fixture::deps_from(fakes), &base(CONFIGMAP)).await;
        assert_eq!(views.len(), 1);
        let View::Summary { fields } = &views[0] else {
            panic!("a summary: {views:?}");
        };
        assert!(
            fields
                .iter()
                .any(|(k, v)| k == "Result" && v == "in sync — the cluster is running this config"),
            "{fields:?}"
        );
    }

    #[tokio::test]
    async fn an_older_version_can_be_compared() {
        let mut pairs = base(CONFIGMAP);
        pairs.push(("version", "ver-1"));
        let views = run(fixture::deps(), &pairs).await;
        let View::Diff { left, delta, .. } = &views[1] else {
            panic!("second view is a diff: {views:?}");
        };
        assert_eq!(left, "S3 ver-1 (2026-08-01 10:00)");
        assert_eq!(delta.changes.len(), 1, "{:?}", delta.changes);
        assert_eq!(delta.changes[0].kind, Kind::Added);
        assert_eq!(delta.changes[0].path.dotted(), "items[id=mocha]");
    }

    #[tokio::test]
    async fn several_data_keys_without_a_choice_is_a_usage_error() {
        let fakes = fixture::fakes();
        fakes.kubectl.insert(
            "menu",
            "configmap",
            CONFIGMAP,
            r#"{"data":{"menu.json":"{}","other":"x"}}"#,
        );
        let failure = fail(fixture::deps_from(fakes), &base(CONFIGMAP)).await;
        assert_eq!(failure.kind, FailureKind::Usage);
        assert!(
            matches!(
                find_cause::<RunnerError>(&failure.error),
                Some(RunnerError::InvalidParam { .. })
            ),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn a_missing_configmap_fails_as_not_found() {
        let failure = fail(fixture::deps(), &base("absent")).await;
        assert!(
            find_cause::<SvcError>(&failure.error).is_some_and(SvcError::is_not_found),
            "{:#}",
            failure.error
        );
        assert!(
            format!("{:#}", failure.error).starts_with("diff live ConfigMap: "),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn a_non_json_s3_body_is_reported() {
        let fakes = fixture::fakes();
        fakes
            .s3
            .insert(BUCKET, KEY, "ver-3", "plain text", fixture::t0());
        let failure = fail(fixture::deps_from(fakes), &base(CONFIGMAP)).await;
        assert!(
            format!("{:#}", failure.error).contains("S3 config is not JSON"),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn without_kubernetes_the_run_fails_clearly() {
        let deps = Deps {
            kubectl: None,
            ..fixture::deps()
        };
        let failure = fail(deps, &base(CONFIGMAP)).await;
        assert!(
            format!("{:#}", failure.error).contains("Kubernetes is not configured"),
            "{:#}",
            failure.error
        );
    }
}
