//! Helpers shared by the operations that follow a record across DynamoDB, S3
//! and Kubernetes.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, bail};

use super::{Deps, RunnerError};
use crate::diff::{self, Delta, Opts};
use crate::jsonutil;
use crate::svc::kubectl::{KubectlPort, configmap_data};
use crate::svc::s3::{Version, is_current};

mod drift;
#[cfg(test)]
pub(crate) mod fixture;

mod trace;

pub use drift::ConfigDrift;
pub use trace::RecordTrace;

/// The Kubernetes port of `deps`, or an error when the invocation has none.
///
/// # Errors
///
/// Returns an error when `deps` carries no Kubernetes port.
pub(crate) fn kube_port(deps: &Deps) -> anyhow::Result<Arc<dyn KubectlPort>> {
    deps.kubectl
        .clone()
        .ok_or_else(|| anyhow!("Kubernetes is not configured"))
}

/// Splits a ConfigMap spec, `name` or `name:key`, at the first colon.
///
/// # Errors
///
/// Returns an error when the name is empty.
pub(crate) fn split_configmap_spec(spec: &str) -> anyhow::Result<(&str, Option<&str>)> {
    let (name, key) = match spec.split_once(':') {
        Some((name, key)) => (name, Some(key)),
        None => (spec, None),
    };
    if name.is_empty() {
        bail!("configmap name is empty in {spec:?}");
    }
    Ok((name, key))
}

/// Parses `name=value` pairs separated by commas. Each part splits on its
/// first `=` and both sides are trimmed; a blank `spec` is an empty map.
///
/// # Errors
///
/// Returns an error naming the first part that is not `name=value` or has an
/// empty name.
pub(crate) fn parse_vars(spec: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut vars = BTreeMap::new();
    if spec.trim().is_empty() {
        return Ok(vars);
    }
    for part in spec.split(',') {
        let (name, value) = part
            .split_once('=')
            .map(|(name, value)| (name.trim(), value.trim()))
            .filter(|(name, _)| !name.is_empty())
            .ok_or_else(|| anyhow!("vars part {:?} is not name=value", part.trim()))?;
        vars.insert(name.to_owned(), value.to_owned());
    }
    Ok(vars)
}

/// Splits an object location into its bucket and key. An `s3://bucket/key`
/// URI names both (the key is empty when only a bucket is given); any other
/// value is a key in `configured_bucket`.
pub(crate) fn split_s3_uri<'a>(value: &'a str, configured_bucket: &'a str) -> (&'a str, &'a str) {
    match value.strip_prefix("s3://") {
        Some(rest) => rest.split_once('/').unwrap_or((rest, "")),
        None => (configured_bucket, value),
    }
}

/// The id of the version older than `current` in `versions`, which arrive
/// newest first. `current` may be the current-version alias, which names the
/// entry flagged latest. Delete markers are skipped. `None` when `current` is
/// absent or nothing older holds content.
pub(crate) fn previous_version_id<'a>(versions: &'a [Version], current: &str) -> Option<&'a str> {
    let at = if is_current(current) {
        versions.iter().position(|v| v.is_latest)
    } else {
        versions.iter().position(|v| v.id == current)
    }?;
    versions[at + 1..]
        .iter()
        .find(|v| !v.is_delete)
        .map(|v| v.id.as_str())
}

/// A bad value for the ConfigMap parameter, as a usage error.
fn invalid_configmap(spec: &str, why: String) -> anyhow::Error {
    anyhow::Error::new(RunnerError::InvalidParam {
        param: "configmap".to_owned(),
        value: spec.to_owned(),
        source: why.into(),
    })
}

/// Fetches the ConfigMap named by `spec` (`name` or `name:key`) from
/// `namespace` and returns the data key used with that key's body. Without a
/// `:key` the ConfigMap must hold exactly one data key.
///
/// # Errors
///
/// Returns [`RunnerError::InvalidParam`] when the ConfigMap has several data
/// keys and none was named, the named key is absent, `spec` has no name, or
/// the name starts with `-`; the port's error when the fetch fails (including
/// not found); and an error when the ConfigMap holds no data.
pub(crate) async fn configmap_body(
    k8s: &dyn KubectlPort,
    namespace: &str,
    spec: &str,
) -> anyhow::Result<(String, String)> {
    let (name, wanted) =
        split_configmap_spec(spec).map_err(|e| invalid_configmap(spec, e.to_string()))?;
    if name.starts_with('-') {
        return Err(invalid_configmap(
            spec,
            format!("configmap name {name:?} cannot start with \"-\""),
        ));
    }
    let raw = k8s.get_json(namespace, "configmap", name).await?;
    let mut data = configmap_data(&raw)?;
    if data.is_empty() {
        bail!("configmap {name} has no data");
    }
    let keys = data
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let count = data.len();
    let entry = match wanted {
        Some(key) => data.remove_entry(key).ok_or_else(|| {
            invalid_configmap(
                spec,
                format!("configmap {name} has no data key {key:?} (have: {keys})"),
            )
        })?,
        None => data.pop_first().filter(|_| count == 1).ok_or_else(|| {
            invalid_configmap(
                spec,
                format!(
                    "configmap {name} has {count} data keys ({keys}) — specify one as name:key"
                ),
            )
        })?,
    };
    Ok(entry)
}

/// Compares an S3 object body (left) with a ConfigMap data value (right)
/// structurally under `opts`.
///
/// # Errors
///
/// Returns an error naming the side that is not valid JSON.
pub(crate) fn json_delta(
    s3_body: &[u8],
    data_key: &str,
    body: &str,
    opts: &Opts,
) -> anyhow::Result<Delta> {
    let left = jsonutil::parse(s3_body).map_err(|e| anyhow!("S3 config is not JSON: {e}"))?;
    let right = jsonutil::parse(body.as_bytes())
        .map_err(|e| anyhow!("configmap data {data_key:?} is not JSON: {e}"))?;
    Ok(diff::json(&left, &right, opts))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::diff::Opts;
    use crate::ops::{Deps, RunnerError, find_cause};
    use crate::svc::SvcError;
    use crate::svc::s3::Version;

    #[test]
    fn split_configmap_spec_cases() {
        assert_eq!(
            split_configmap_spec("menu-config").unwrap(),
            ("menu-config", None)
        );
        assert_eq!(
            split_configmap_spec("menu-config:menu.json").unwrap(),
            ("menu-config", Some("menu.json"))
        );
        assert_eq!(split_configmap_spec("a:b:c").unwrap(), ("a", Some("b:c")));
        assert!(split_configmap_spec(":x").is_err());
        assert!(split_configmap_spec("").is_err());
    }

    #[test]
    fn parse_vars_is_strict() {
        assert!(parse_vars("").unwrap().is_empty());
        assert!(parse_vars("  ").unwrap().is_empty());
        assert_eq!(
            parse_vars(" locale = en-US , channel=mobile ").unwrap(),
            BTreeMap::from([
                ("channel".to_owned(), "mobile".to_owned()),
                ("locale".to_owned(), "en-US".to_owned()),
            ])
        );
        assert_eq!(
            parse_vars("a=b=c").unwrap(),
            BTreeMap::from([("a".to_owned(), "b=c".to_owned())])
        );
        let err = parse_vars("locale=en-US,oops").unwrap_err();
        assert!(
            format!("{err:#}").contains(r#"vars part "oops" is not name=value"#),
            "{err:#}"
        );
    }

    #[test]
    fn split_s3_uri_cases() {
        assert_eq!(
            split_s3_uri("s3://other-bucket/some/key.json", "cfg"),
            ("other-bucket", "some/key.json")
        );
        assert_eq!(
            split_s3_uri("plain/key.json", "cfg"),
            ("cfg", "plain/key.json")
        );
        assert_eq!(split_s3_uri("s3://only", "cfg"), ("only", ""));
    }

    fn version(id: &str, is_latest: bool, is_delete: bool) -> Version {
        Version {
            id: id.to_owned(),
            is_latest,
            is_delete,
            ..Version::default()
        }
    }

    #[test]
    fn previous_version_skips_delete_markers() {
        let versions = [
            version("v3", true, false),
            version("del", false, true),
            version("v2", false, false),
            version("v1", false, false),
        ];
        assert_eq!(previous_version_id(&versions, "v3"), Some("v2"));
        assert_eq!(previous_version_id(&versions, "null"), Some("v2"));
        assert_eq!(previous_version_id(&versions, "v1"), None);
        assert_eq!(previous_version_id(&versions, "zzz"), None);
    }

    #[tokio::test]
    async fn configmap_body_picks_the_only_key() {
        let deps = fixture::deps();
        let k8s = kube_port(&deps).unwrap();
        let (key, body) = configmap_body(k8s.as_ref(), "menu", "menu-config")
            .await
            .unwrap();
        assert_eq!(key, "menu.json");
        assert!(body.contains("mocha"), "{body}");
    }

    #[tokio::test]
    async fn configmap_body_with_several_keys_needs_a_choice() {
        let fakes = fixture::fakes();
        fakes.kubectl.insert(
            "menu",
            "configmap",
            "menu-config",
            r#"{"data":{"menu.json":"{}","other":"x"}}"#,
        );
        let err = configmap_body(&fakes.kubectl, "menu", "menu-config")
            .await
            .unwrap_err();
        let Some(RunnerError::InvalidParam { param, value, .. }) = find_cause::<RunnerError>(&err)
        else {
            panic!("an ambiguous ConfigMap is a usage error: {err:#}");
        };
        assert_eq!(param, "configmap");
        assert_eq!(value, "menu-config");
        assert!(format!("{err:#}").contains("menu.json, other"), "{err:#}");
    }

    #[tokio::test]
    async fn configmap_body_with_an_empty_name_is_a_usage_error() {
        let deps = fixture::deps();
        let k8s = kube_port(&deps).unwrap();
        for spec in [":x", ""] {
            let err = configmap_body(k8s.as_ref(), "menu", spec)
                .await
                .unwrap_err();
            let Some(RunnerError::InvalidParam { param, value, .. }) =
                find_cause::<RunnerError>(&err)
            else {
                panic!("an empty ConfigMap name is a usage error: {err:#}");
            };
            assert_eq!(param, "configmap");
            assert_eq!(value, spec);
        }
    }

    #[tokio::test]
    async fn configmap_body_rejects_a_name_that_looks_like_a_flag() {
        let deps = fixture::deps();
        let k8s = kube_port(&deps).unwrap();
        let spec = "--context=prod:menu.json";
        let err = configmap_body(k8s.as_ref(), "menu", spec)
            .await
            .unwrap_err();
        let Some(RunnerError::InvalidParam { param, value, .. }) = find_cause::<RunnerError>(&err)
        else {
            panic!("a ConfigMap name starting with \"-\" is a usage error: {err:#}");
        };
        assert_eq!(param, "configmap");
        assert_eq!(value, spec);
        assert!(
            format!("{err:#}").contains(r#"configmap name "--context=prod" cannot start with "-""#),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn configmap_body_names_an_unknown_key() {
        let deps = fixture::deps();
        let k8s = kube_port(&deps).unwrap();
        let err = configmap_body(k8s.as_ref(), "menu", "menu-config:nope")
            .await
            .unwrap_err();
        assert!(
            matches!(
                find_cause::<RunnerError>(&err),
                Some(RunnerError::InvalidParam { .. })
            ),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains(r#"no data key "nope" (have: menu.json)"#),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn configmap_body_reports_a_missing_configmap_as_not_found() {
        let deps = fixture::deps();
        let k8s = kube_port(&deps).unwrap();
        let err = configmap_body(k8s.as_ref(), "menu", "absent")
            .await
            .unwrap_err();
        assert!(
            find_cause::<SvcError>(&err).is_some_and(SvcError::is_not_found),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn configmap_without_data_is_an_error() {
        let fakes = fixture::fakes();
        fakes
            .kubectl
            .insert("menu", "configmap", "empty", r#"{"metadata":{}}"#);
        let err = configmap_body(&fakes.kubectl, "menu", "empty")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("configmap empty has no data"),
            "{err:#}"
        );
    }

    #[test]
    fn kube_port_requires_a_configured_port() {
        let err = kube_port(&Deps::default()).err().expect("no port");
        assert!(
            format!("{err:#}").contains("Kubernetes is not configured"),
            "{err:#}"
        );
    }

    #[test]
    fn json_delta_names_the_side_that_is_not_json() {
        let opts = Opts::default();
        let err = json_delta(b"x", "menu.json", "{}", &opts).unwrap_err();
        assert!(
            format!("{err:#}").contains("S3 config is not JSON"),
            "{err:#}"
        );
        let err = json_delta(b"{}", "menu.json", "x", &opts).unwrap_err();
        assert!(
            format!("{err:#}").contains(r#"configmap data "menu.json" is not JSON"#),
            "{err:#}"
        );
    }

    #[test]
    fn json_delta_compares_both_sides() {
        let delta = json_delta(br#"{"a":1}"#, "k", r#"{"a":2}"#, &Opts::default()).unwrap();
        assert_eq!(delta.stats.total(), 1);
    }
}
