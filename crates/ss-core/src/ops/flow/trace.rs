//! `record.trace`: follow a DynamoDB record to the S3 config it points at and,
//! optionally, to the ConfigMap the cluster runs.

use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use serde_json::Value;

use super::{configmap_body, json_delta, kube_port, parse_vars, previous_version_id};
use super::{split_configmap_spec, split_s3_uri};
use crate::config::TraceDomain;
use crate::diff::Opts;
use crate::guard::Class;
use crate::jsonutil;
use crate::ops::compare::{Comparison, compare_bodies};
use crate::ops::ddb::{coerce, port as ddb_port, refuse_binary_key};
use crate::ops::s3::port as s3_port;
use crate::ops::{
    Choice, Deps, Input, Meta, OpResult, Operation, Param, ParamType, Runner, RunnerError,
    SuggestFn, View,
};
use crate::svc::ddb::{DdbPort, Item, Key};

/// Follows one domain record to the S3 config it points at, diffs that
/// config against its previous version and, on request, against the live
/// ConfigMap.
#[derive(Clone, Copy, Debug, Default)]
pub struct RecordTrace;

/// A bad value for the parameter `param`, as a usage error.
fn usage(
    param: &str,
    value: &str,
    why: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> anyhow::Error {
    anyhow::Error::new(RunnerError::InvalidParam {
        param: param.to_owned(),
        value: value.to_owned(),
        source: why.into(),
    })
}

/// Suggests every configured domain with its table and sort format.
fn suggest_domains() -> SuggestFn {
    Arc::new(|deps: Deps, _input: Input| {
        Box::pin(async move {
            let env = deps.env.name.as_str();
            Ok(deps
                .trace
                .domains
                .iter()
                .map(|(name, domain)| {
                    let domain = domain.clone().with_defaults();
                    let mut label = format!("{name} — {}", domain.resolve_table(env));
                    if !domain.sort_format.is_empty() {
                        label.push_str(&format!(" · sort {}", domain.sort_format));
                    }
                    Choice {
                        value: name.clone(),
                        label: Some(label),
                    }
                })
                .collect())
        })
    })
}

/// Where a trace starts: the domain with its defaults applied, the table and
/// the sort key value for this run.
struct Plan {
    name: String,
    domain: TraceDomain,
    table: String,
    sort: String,
}

/// Resolves the domain, table and sort key from the input.
///
/// # Errors
///
/// Returns [`RunnerError::InvalidParam`] for an unknown domain, a domain
/// without a table for the environment, malformed `vars`, or sort key
/// variables that are missing.
fn resolve_plan(deps: &Deps, input: &Input) -> anyhow::Result<Plan> {
    let name = input.str("domain");
    let domain = deps
        .trace
        .domains
        .get(name)
        .ok_or_else(|| {
            usage(
                "domain",
                name,
                format!(
                    r#"unknown trace domain "{name}" (configure trace.domains in config.yaml)"#
                ),
            )
        })?
        .clone()
        .with_defaults();
    let env = deps.env.name.as_str();
    let table = domain.resolve_table(env);
    if table.is_empty() {
        return Err(usage(
            "domain",
            name,
            format!(r#"domain "{name}" has no table for env "{env}""#),
        ));
    }
    let spec = input.str("vars");
    let mut vars = parse_vars(spec).map_err(|e| usage("vars", spec, e))?;
    vars.insert(domain.version_attr.clone(), input.str("version").to_owned());
    let sort = domain
        .build_sort(&vars)
        .map_err(|e| usage("vars", spec, e))?;
    Ok(Plan {
        name: name.to_owned(),
        domain,
        table,
        sort,
    })
}

/// Reads the record `id` of the planned table.
///
/// # Errors
///
/// Returns an error when the table has a binary key attribute or a call
/// fails, including when the record does not exist.
async fn fetch_record(ddb: &dyn DdbPort, plan: &Plan, id: &str) -> anyhow::Result<Item> {
    let info = ddb.describe_table(&plan.table).await?;
    refuse_binary_key(&info)?;
    let domain = &plan.domain;
    let typed = |attr: &str, value: &str| {
        coerce(
            Value::String(value.to_owned()),
            info.attr_types.get(attr).copied(),
        )
    };
    let mut key = Key::new();
    key.insert(domain.hash_attr.clone(), typed(&domain.hash_attr, id));
    if !domain.sort_attr.is_empty() && !plan.sort.is_empty() {
        key.insert(
            domain.sort_attr.clone(),
            typed(&domain.sort_attr, &plan.sort),
        );
    }
    Ok(ddb.get_item(&plan.table, &key).await?)
}

/// The S3 object a record points at.
struct Target {
    bucket: String,
    key: String,
    /// The recorded version id; empty means the current version.
    version_id: String,
}

/// Reads the object location out of `record`.
///
/// # Errors
///
/// Returns an error when the record has no object key or no bucket is known.
fn locate(record: &Item, plan: &Plan, env: &str) -> anyhow::Result<Target> {
    let domain = &plan.domain;
    let attr = &domain.s3_key_attr;
    let value = match record.get(attr) {
        Some(Value::String(s)) if !s.is_empty() => s.as_str(),
        _ => bail!(r#"record has no "{attr}" attribute — wrong domain mapping?"#),
    };
    let configured = domain.resolve_bucket(env);
    let (bucket, key) = split_s3_uri(value, &configured);
    if bucket.is_empty() {
        bail!(
            r#"no bucket: set trace.domains.{}.bucket or store s3:// URIs in "{attr}""#,
            plan.name
        );
    }
    let version_id = match record.get(&domain.s3_version_attr) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    Ok(Target {
        bucket: bucket.to_owned(),
        key: key.to_owned(),
        version_id,
    })
}

#[async_trait]
impl Operation for RecordTrace {
    fn meta(&self) -> Meta {
        Meta {
            id: "record.trace",
            name: "Trace record to config",
            summary: "Follow a domain record to its S3 config and diff against the previous version",
            category: "Cross-service",
            class: Class::Read,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::new("domain")
                .label("Domain")
                .ty(ParamType::Enum)
                .required(true)
                .help("record shape from config.yaml trace.domains (menu, …)")
                .suggest(suggest_domains()),
            Param::new("id")
                .label("Record ID")
                .required(true)
                .help("partition key value (e.g. store number for menu)"),
            Param::new("vars")
                .label("Sort key vars")
                .help("k=v pairs for the domain's sort format, e.g. locale=en-US,channel=mobile"),
            Param::new("version")
                .label("Record version")
                .default("V0")
                .help("record version label; V0 = latest"),
            Param::new("configmap")
                .label("ConfigMap")
                .help("optionally diff the live ConfigMap against the S3 config (name[:data-key])"),
            Param::new("namespace")
                .label("Namespace")
                .help("kubernetes namespace for --configmap; defaults to the domain's"),
            Param::new("ignore")
                .label("Ignore paths")
                .ty(ParamType::Json)
                .help(r#"extra ignore globs, e.g. "**.updatedAt""#),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let plan = resolve_plan(&r.deps, input)?;
        let domain = &plan.domain;
        let ddb = ddb_port(&r.deps)?;
        let s3 = s3_port(&r.deps)?;
        let id = input.str("id");

        let record = r
            .step("fetch record", |_| fetch_record(ddb.as_ref(), &plan, id))
            .await?;
        let target = locate(&record, &plan, &r.deps.env.name)?;
        let Target {
            bucket,
            key,
            version_id,
        } = &target;

        let recorded = r
            .step("fetch recorded S3 version", |_| async {
                Ok(s3.get_version(bucket, key, version_id).await?)
            })
            .await?;
        let previous = r
            .step("resolve previous version", |_| async {
                let versions = s3.list_versions(bucket, key).await?;
                match previous_version_id(&versions, &recorded.version_id) {
                    Some(id) => Ok(Some(s3.get_version(bucket, key, id).await?)),
                    None => Ok(None),
                }
            })
            .await?;
        let recorded_label = recorded.version_label();

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

        let mut fields = vec![
            ("Domain".to_owned(), plan.name.clone()),
            (
                "Record".to_owned(),
                format!("{} {}={id}", plan.table, domain.hash_attr),
            ),
        ];
        if !plan.sort.is_empty() {
            fields.push(("Sort key".to_owned(), plan.sort.clone()));
        }
        for label in &domain.labels {
            if let Some(value) = record.get(label) {
                let text = match value {
                    Value::String(s) => s.clone(),
                    other => jsonutil::compact(other),
                };
                fields.push((label.clone(), text));
            }
        }
        fields.push(("S3 object".to_owned(), format!("{bucket}/{key}")));
        fields.push(("Recorded version".to_owned(), recorded_label.clone()));

        let mut history = None;
        match &previous {
            None => fields.push((
                "Change vs previous".to_owned(),
                "none — recorded version is the oldest in history".to_owned(),
            )),
            Some(prev) => {
                let prev_label = prev.version_label();
                match compare_bodies(&prev.body, &recorded.body, &opts) {
                    Comparison::Json(delta) => {
                        fields.push(("Change vs previous".to_owned(), delta.stats.to_string()));
                        history = Some(View::Diff {
                            left: prev_label,
                            right: recorded_label.clone(),
                            delta,
                        });
                    }
                    Comparison::Lines(body) => {
                        history = Some(View::Doc {
                            title: format!("Line diff: {prev_label} → {recorded_label}"),
                            body,
                            lang: "diff".to_owned(),
                        });
                    }
                }
            }
        }

        let mut drift = None;
        let spec = input.str("configmap");
        if !spec.is_empty() {
            let k8s = kube_port(&r.deps)?;
            let namespace = [input.str("namespace"), domain.namespace.as_str()]
                .into_iter()
                .find(|ns| !ns.is_empty())
                .unwrap_or_default();
            let delta = r
                .step("diff live ConfigMap", |_| async {
                    let (data_key, body) = configmap_body(k8s.as_ref(), namespace, spec).await?;
                    json_delta(&recorded.body, &data_key, &body, &opts)
                })
                .await?;
            // A successful fetch means the spec named a ConfigMap.
            let name = split_configmap_spec(spec).map_or(spec, |(name, _)| name);
            let verdict = if delta.stats.is_empty() {
                "in sync with the recorded S3 config".to_owned()
            } else {
                format!("DRIFT vs recorded S3 config ({})", delta.stats)
            };
            fields.push((format!("ConfigMap {name}"), verdict));
            if !delta.stats.is_empty() {
                drift = Some(View::Diff {
                    left: format!("S3 {recorded_label}"),
                    right: format!("ConfigMap {name}"),
                    delta,
                });
            }
        }

        let mut views = vec![View::Summary { fields }];
        views.extend(history);
        views.extend(drift);
        Ok(OpResult { views })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::*;
    use crate::diff::Kind;
    use crate::ops::flow::fixture::{self, BUCKET, KEY, TABLE, t0};
    use crate::ops::{
        Choice, Deps, FailureKind, Input, RunFailure, Runner, RunnerError, View, find_cause,
    };
    use crate::svc::SvcError;
    use crate::svc::ddb::{AttrType, FakeDdb, Item, TableInfo};
    use crate::svc::s3::FakeS3;

    const VARS: &str = "locale=en-US,channel=mobile";

    fn input(pairs: &[(&str, &str)]) -> Input {
        pairs.iter().copied().collect()
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("domain", "menu"),
            ("id", "12345"),
            ("vars", VARS),
            ("version", "V0"),
        ]
    }

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut pairs = base();
        pairs.extend_from_slice(extra);
        pairs
    }

    async fn run(deps: Deps, pairs: &[(&str, &str)]) -> Vec<View> {
        Runner::new(deps)
            .execute(&RecordTrace, input(pairs))
            .await
            .expect("trace succeeds")
            .views
    }

    async fn fail(deps: Deps, pairs: &[(&str, &str)]) -> RunFailure {
        Runner::new(deps)
            .execute(&RecordTrace, input(pairs))
            .await
            .expect_err("trace fails")
    }

    fn summary(views: &[View]) -> BTreeMap<&str, &str> {
        let View::Summary { fields } = &views[0] else {
            panic!("first view is a summary: {views:?}");
        };
        fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn diffs(views: &[View]) -> Vec<&View> {
        views
            .iter()
            .filter(|v| matches!(v, View::Diff { .. }))
            .collect()
    }

    fn record(extra: Value) -> Item {
        let Value::Object(mut item) = json!({
            "storeNumber": "12345",
            "localeChannel": "en-US#mobile",
            "version": "V0",
            "key": KEY,
            "versionId": "ver-2",
        }) else {
            unreachable!("a JSON object literal is an object")
        };
        if let Value::Object(more) = extra {
            item.extend(more);
        }
        item
    }

    fn table_info(hash: AttrType) -> TableInfo {
        TableInfo {
            name: TABLE.to_owned(),
            hash_key: "storeNumber".to_owned(),
            sort_key: Some("localeChannel".to_owned()),
            attr_types: BTreeMap::from([
                ("storeNumber".to_owned(), hash),
                ("localeChannel".to_owned(), AttrType::S),
            ]),
            gsis: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_trace_spans_three_services() {
        let views = run(
            fixture::deps(),
            &with(&[("configmap", "menu-config:menu.json")]),
        )
        .await;
        assert_eq!(views.len(), 3, "{views:?}");
        let rows = summary(&views);
        assert_eq!(rows["Domain"], "menu");
        assert!(rows["Record"].contains("ss-menu-dev"), "{rows:?}");
        assert_eq!(rows["Record"], "ss-menu-dev storeNumber=12345");
        assert_eq!(rows["Sort key"], "en-US#mobile");
        assert_eq!(rows["storeNumber"], "12345");
        assert_eq!(rows["version"], "V0");
        assert_eq!(rows["S3 object"], "ss-config-dev/menu/12345.json");
        assert_eq!(rows["Recorded version"], "ver-2 (2026-08-01 11:00)");
        assert!(
            rows["ConfigMap menu-config"].starts_with("DRIFT vs recorded S3 config ("),
            "{rows:?}"
        );

        let View::Diff { left, right, delta } = &views[1] else {
            panic!("second view is the history diff: {views:?}");
        };
        assert_eq!(left, "ver-1 (2026-08-01 10:00)");
        assert_eq!(right, "ver-2 (2026-08-01 11:00)");
        assert_eq!(delta.stats.added, 1);
        assert_eq!(delta.stats.changed, 1);
        assert_eq!(rows["Change vs previous"], delta.stats.to_string());

        let View::Diff { left, right, delta } = &views[2] else {
            panic!("third view is the drift diff: {views:?}");
        };
        assert_eq!(left, "S3 ver-2 (2026-08-01 11:00)");
        assert_eq!(right, "ConfigMap menu-config");
        assert_eq!(delta.changes.len(), 1, "{:?}", delta.changes);
        assert_eq!(delta.changes[0].path.dotted(), "items[id=latte].price");
        assert_eq!(delta.changes[0].kind, Kind::Changed);
    }

    #[tokio::test]
    async fn a_matching_configmap_is_in_sync() {
        let fakes = fixture::fakes();
        fakes.kubectl.insert(
            "menu",
            "configmap",
            "menu-config",
            r#"{"data":{"menu.json":"{\"items\":[{\"id\":\"latte\",\"price\":6},{\"id\":\"mocha\",\"price\":7}]}"}}"#,
        );
        let views = run(
            fixture::deps_from(fakes),
            &with(&[("configmap", "menu-config")]),
        )
        .await;
        assert_eq!(diffs(&views).len(), 1, "{views:?}");
        assert_eq!(
            summary(&views)["ConfigMap menu-config"],
            "in sync with the recorded S3 config"
        );
    }

    #[tokio::test]
    async fn without_a_configmap_kubernetes_is_not_touched() {
        let deps = Deps {
            kubectl: None,
            ..fixture::deps()
        };
        let views = run(deps, &base()).await;
        assert_eq!(diffs(&views).len(), 1, "{views:?}");
        assert!(
            summary(&views).keys().all(|k| !k.starts_with("ConfigMap")),
            "{views:?}"
        );
    }

    #[tokio::test]
    async fn a_configmap_without_kubernetes_fails_clearly() {
        let deps = Deps {
            kubectl: None,
            ..fixture::deps()
        };
        let failure = fail(deps, &with(&[("configmap", "menu-config")])).await;
        assert_eq!(failure.kind, FailureKind::Failed);
        assert!(
            format!("{:#}", failure.error).contains("Kubernetes is not configured"),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn missing_sort_vars_are_a_usage_error_naming_them() {
        let pairs = [("domain", "menu"), ("id", "12345"), ("version", "V0")];
        let failure = fail(fixture::deps(), &pairs).await;
        assert_eq!(failure.kind, FailureKind::Usage);
        let message = format!("{:#}", failure.error);
        assert!(
            message.contains("locale") && message.contains("channel"),
            "{message}"
        );
        assert!(
            matches!(
                find_cause::<RunnerError>(&failure.error),
                Some(RunnerError::InvalidParam { param, .. }) if param == "vars"
            ),
            "{message}"
        );
    }

    #[tokio::test]
    async fn an_unknown_domain_is_a_usage_error() {
        let failure = fail(
            fixture::deps(),
            &[("domain", "nope"), ("id", "1"), ("version", "V0")],
        )
        .await;
        assert_eq!(failure.kind, FailureKind::Usage);
        assert!(
            format!("{:#}", failure.error).contains(
                r#"unknown trace domain "nope" (configure trace.domains in config.yaml)"#
            ),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn a_domain_without_a_table_for_the_env_is_a_usage_error() {
        let mut deps = fixture::deps();
        deps.trace
            .domains
            .get_mut("menu")
            .expect("menu domain")
            .table
            .clear();
        let failure = fail(deps, &base()).await;
        assert_eq!(failure.kind, FailureKind::Usage);
        assert!(
            format!("{:#}", failure.error).contains(r#"domain "menu" has no table for env "dev""#),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn malformed_vars_are_a_usage_error() {
        let pairs = [
            ("domain", "menu"),
            ("id", "12345"),
            ("version", "V0"),
            ("vars", "locale=en-US,oops"),
        ];
        let failure = fail(fixture::deps(), &pairs).await;
        assert_eq!(failure.kind, FailureKind::Usage);
        assert!(
            format!("{:#}", failure.error).contains(r#"vars part "oops" is not name=value"#),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn the_oldest_version_has_no_history_diff() {
        let fakes = fixture::fakes();
        fakes.ddb.put(TABLE, record(json!({"versionId": "ver-1"})));
        let views = run(fixture::deps_from(fakes), &base()).await;
        assert_eq!(views.len(), 1, "{views:?}");
        assert_eq!(
            summary(&views)["Change vs previous"],
            "none — recorded version is the oldest in history"
        );
    }

    #[tokio::test]
    async fn delete_markers_are_skipped_when_finding_the_previous_version() {
        let s3 = FakeS3::new();
        s3.insert(BUCKET, KEY, "ver-1", r#"{"a":1}"#, t0());
        s3.delete(BUCKET, KEY);
        s3.insert(BUCKET, KEY, "ver-2", r#"{"a":2}"#, t0());
        let fakes = fixture::Fakes {
            s3,
            ..fixture::fakes()
        };
        let views = run(fixture::deps_from(fakes), &base()).await;
        let View::Diff { left, right, delta } = &views[1] else {
            panic!("second view is the history diff: {views:?}");
        };
        assert_eq!(left, "ver-1 (2026-08-01 10:00)");
        assert_eq!(right, "ver-2 (2026-08-01 10:00)");
        assert_eq!(delta.changes.len(), 1);
    }

    #[tokio::test]
    async fn an_s3_uri_on_the_record_overrides_the_bucket() {
        let fakes = fixture::fakes();
        fakes
            .s3
            .insert("other-bucket", "menu/12345.json", "ver-9", "{}", t0());
        fakes.ddb.put(
            TABLE,
            record(json!({"key": "s3://other-bucket/menu/12345.json", "versionId": "ver-9"})),
        );
        let views = run(fixture::deps_from(fakes), &base()).await;
        assert_eq!(summary(&views)["S3 object"], "other-bucket/menu/12345.json");
    }

    #[tokio::test]
    async fn a_record_without_the_key_attribute_is_reported() {
        let fakes = fixture::fakes();
        let mut item = record(json!({}));
        item.remove("key");
        fakes.ddb.put(TABLE, item);
        let failure = fail(fixture::deps_from(fakes), &base()).await;
        assert_eq!(failure.kind, FailureKind::Failed);
        assert_eq!(
            format!("{:#}", failure.error),
            r#"record has no "key" attribute — wrong domain mapping?"#
        );
    }

    #[tokio::test]
    async fn no_bucket_anywhere_is_reported() {
        let mut deps = fixture::deps();
        deps.trace
            .domains
            .get_mut("menu")
            .expect("menu domain")
            .bucket
            .clear();
        let failure = fail(deps, &base()).await;
        assert_eq!(
            format!("{:#}", failure.error),
            r#"no bucket: set trace.domains.menu.bucket or store s3:// URIs in "key""#
        );
    }

    #[tokio::test]
    async fn a_numeric_partition_key_is_coerced() {
        let ddb = FakeDdb::new();
        ddb.create_table(table_info(AttrType::N));
        ddb.put(TABLE, record(json!({"storeNumber": 12345})));
        let fakes = fixture::Fakes {
            ddb,
            ..fixture::fakes()
        };
        let views = run(fixture::deps_from(fakes), &base()).await;
        assert_eq!(summary(&views)["Record"], "ss-menu-dev storeNumber=12345");
        assert_eq!(summary(&views)["storeNumber"], "12345");
    }

    #[tokio::test]
    async fn a_binary_partition_key_is_refused() {
        let ddb = FakeDdb::new();
        ddb.create_table(table_info(AttrType::B));
        let fakes = fixture::Fakes {
            ddb,
            ..fixture::fakes()
        };
        let failure = fail(fixture::deps_from(fakes), &base()).await;
        assert!(
            format!("{:#}", failure.error)
                .contains("binary key attributes are not supported: storeNumber"),
            "{:#}",
            failure.error
        );
    }

    #[tokio::test]
    async fn non_json_bodies_get_a_line_diff() {
        let s3 = FakeS3::new();
        s3.insert(BUCKET, KEY, "ver-1", "same\nold", t0());
        s3.insert(BUCKET, KEY, "ver-2", "same\nnew", t0());
        let fakes = fixture::Fakes {
            s3,
            ..fixture::fakes()
        };
        let views = run(fixture::deps_from(fakes), &base()).await;
        let View::Doc { title, body, lang } = &views[1] else {
            panic!("second view is a line diff: {views:?}");
        };
        assert_eq!(
            title,
            "Line diff: ver-1 (2026-08-01 10:00) → ver-2 (2026-08-01 10:00)"
        );
        assert_eq!(lang, "diff");
        assert!(body.contains("-old") && body.contains("+new"), "{body}");
        assert!(!summary(&views).contains_key("Change vs previous"));
    }

    #[tokio::test]
    async fn the_configmap_namespace_defaults_to_the_domain() {
        let views = run(fixture::deps(), &with(&[("configmap", "menu-config")])).await;
        assert!(summary(&views).contains_key("ConfigMap menu-config"));

        let failure = fail(
            fixture::deps(),
            &with(&[("configmap", "menu-config"), ("namespace", "other")]),
        )
        .await;
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
    async fn domain_suggestions_show_the_table_and_sort_format() {
        let param = RecordTrace
            .params()
            .into_iter()
            .find(|p| p.name == "domain")
            .expect("domain param");
        let suggest = param.suggest.expect("domain suggestions");
        let choices = suggest(fixture::deps(), Input::default())
            .await
            .expect("suggestions");
        assert_eq!(
            choices,
            [Choice {
                value: "menu".to_owned(),
                label: Some("menu — ss-menu-dev · sort {locale}#{channel}".to_owned()),
            }]
        );
    }
}
