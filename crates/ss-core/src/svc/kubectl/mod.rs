//! The read-only Kubernetes port and the parsers for what it returns.
//!
//! The port exposes only `get` and `logs`; nothing here can change cluster
//! state.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::SvcError;

#[cfg(any(test, feature = "test-support"))]
mod fake;

#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeKubectl;

/// How many trailing log lines [`KubectlPort::logs`] returns before following.
pub const LOG_TAIL: u32 = 200;

/// Capacity of the channel a log stream is delivered on.
pub const LOG_BUFFER: usize = 64;

/// Read-only access to a Kubernetes cluster.
#[async_trait]
pub trait KubectlPort: Send + Sync {
    /// Runs `kubectl get <resource> <name> -o json` in `namespace` and returns
    /// the raw JSON. An empty `namespace` means the context's default.
    ///
    /// # Errors
    ///
    /// [`SvcError::NotFound`] when the resource does not exist; otherwise any
    /// failure to run kubectl or reach the cluster.
    async fn get_json(
        &self,
        namespace: &str,
        resource: &str,
        name: &str,
    ) -> Result<Vec<u8>, SvcError>;

    /// Runs `kubectl get <resource> -o json` in `namespace` and returns the
    /// raw JSON list.
    ///
    /// # Errors
    ///
    /// [`SvcError::NotFound`] when the resource type or namespace does not
    /// exist; otherwise any failure to run kubectl or reach the cluster.
    async fn list_json(&self, namespace: &str, resource: &str) -> Result<Vec<u8>, SvcError>;

    /// Streams a pod's log: the last [`LOG_TAIL`] lines, then new lines while
    /// `follow` is set. The channel closes when the stream ends; dropping the
    /// receiver stops it.
    ///
    /// # Errors
    ///
    /// Any failure to start the stream.
    async fn logs(
        &self,
        namespace: &str,
        pod: &str,
        follow: bool,
    ) -> Result<tokio::sync::mpsc::Receiver<String>, SvcError>;
}

/// A pod as shown in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pod {
    /// Pod name.
    pub name: String,
    /// Lifecycle phase, e.g. `Running`.
    pub phase: String,
    /// Ready containers over total containers, e.g. `1/2`.
    pub ready: String,
    /// Restarts summed over all containers.
    pub restarts: u32,
    /// When the pod was created, if the cluster reported it.
    pub created: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct PodList {
    #[serde(default)]
    items: Vec<RawPod>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPod {
    #[serde(default)]
    metadata: PodMeta,
    #[serde(default)]
    status: PodStatus,
}

#[derive(Debug, Default, Deserialize)]
struct PodMeta {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "creationTimestamp")]
    created: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Deserialize)]
struct PodStatus {
    #[serde(default)]
    phase: String,
    #[serde(default, rename = "containerStatuses")]
    containers: Vec<ContainerStatus>,
}

#[derive(Debug, Default, Deserialize)]
struct ContainerStatus {
    #[serde(default)]
    ready: bool,
    #[serde(default, rename = "restartCount")]
    restarts: u32,
}

/// Parses the JSON of a `kubectl get pods -o json` listing.
///
/// # Errors
///
/// [`SvcError::Other`] (`parse pods: …`) when `raw` is not a valid pod list.
pub fn parse_pods(raw: &[u8]) -> Result<Vec<Pod>, SvcError> {
    let list: PodList = serde_json::from_slice(raw)
        .map_err(|e| SvcError::Other(anyhow::anyhow!("parse pods: {e}")))?;
    Ok(list
        .items
        .into_iter()
        .map(|p| {
            let ready = p.status.containers.iter().filter(|c| c.ready).count();
            Pod {
                name: p.metadata.name,
                phase: p.status.phase,
                ready: format!("{ready}/{}", p.status.containers.len()),
                restarts: p.status.containers.iter().map(|c| c.restarts).sum(),
                created: p.metadata.created,
            }
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct ConfigMap {
    #[serde(default)]
    data: BTreeMap<String, String>,
}

/// Extracts the `data` map from the JSON of a ConfigMap; a ConfigMap without
/// `data` yields an empty map.
///
/// # Errors
///
/// [`SvcError::Other`] (`parse configmap: …`) when `raw` is not a valid
/// ConfigMap.
pub fn configmap_data(raw: &[u8]) -> Result<BTreeMap<String, String>, SvcError> {
    let cm: ConfigMap = serde_json::from_slice(raw)
        .map_err(|e| SvcError::Other(anyhow::anyhow!("parse configmap: {e}")))?;
    Ok(cm.data)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    const PODS: &str = r#"{"items":[
        {"metadata":{"name":"api-7f9","creationTimestamp":"2026-08-01T00:00:00Z"},
         "status":{"phase":"Running","containerStatuses":[
            {"ready":true,"restartCount":2},{"ready":false,"restartCount":0}]}},
        {"metadata":{"name":"worker-1"},"status":{"phase":"Pending"}}
    ]}"#;

    #[test]
    fn parse_pods_sums_restarts_counts_ready_and_keeps_the_creation_time() {
        let pods = parse_pods(PODS.as_bytes()).unwrap();
        assert_eq!(
            pods,
            vec![
                Pod {
                    name: "api-7f9".into(),
                    phase: "Running".into(),
                    ready: "1/2".into(),
                    restarts: 2,
                    created: Some(Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap()),
                },
                Pod {
                    name: "worker-1".into(),
                    phase: "Pending".into(),
                    ready: "0/0".into(),
                    restarts: 0,
                    created: None,
                },
            ]
        );
    }

    #[test]
    fn parse_pods_rejects_malformed_json() {
        let err = parse_pods(b"{").unwrap_err();
        assert!(!err.is_not_found());
        assert!(err.to_string().starts_with("parse pods: "), "{err}");
    }

    #[test]
    fn configmap_data_returns_the_data_map() {
        let raw = br#"{"data":{"menu.json":"{\"a\":1}","other":"x"}}"#;
        let data = configmap_data(raw).unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data["menu.json"], r#"{"a":1}"#);
        assert_eq!(data["other"], "x");
    }

    #[test]
    fn configmap_without_data_is_empty() {
        assert!(configmap_data(br#"{"metadata":{}}"#).unwrap().is_empty());
    }

    #[test]
    fn configmap_rejects_malformed_json() {
        let err = configmap_data(b"[").unwrap_err();
        assert!(err.to_string().starts_with("parse configmap: "), "{err}");
    }
}
