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

/// The most bytes one line of a log stream holds: 16 KiB. A longer line
/// arrives as several lines, so a log with no line break in it cannot grow
/// memory without bound.
pub const LOG_LINE_MAX: usize = 16 << 10;

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
    /// Any failure to run kubectl or reach the cluster. A namespace that does
    /// not exist is not a failure: kubectl lists it as empty, so a caller
    /// that needs to tell "empty" from "no such namespace" asks for the
    /// namespace itself with [`KubectlPort::get_json`].
    async fn list_json(&self, namespace: &str, resource: &str) -> Result<Vec<u8>, SvcError>;

    /// Streams a pod's log: the last [`LOG_TAIL`] lines, then new lines while
    /// `follow` is set. No line is longer than [`LOG_LINE_MAX`] bytes; a
    /// longer one arrives in pieces. The channel closes when the stream
    /// ends; dropping the receiver stops it.
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
    /// What the pod is doing, as `kubectl get pods` words it: `Terminating`
    /// while it is being deleted, a waiting container's reason such as
    /// `CrashLoopBackOff` (prefixed `Init:` for an init container), the
    /// reason its containers finished with when none is running, and
    /// otherwise the phase.
    pub status: String,
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
    #[serde(default, rename = "deletionTimestamp")]
    deletion: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct PodStatus {
    #[serde(default)]
    phase: String,
    #[serde(default, rename = "containerStatuses")]
    containers: Vec<ContainerStatus>,
    #[serde(default, rename = "initContainerStatuses")]
    init_containers: Vec<ContainerStatus>,
}

#[derive(Debug, Default, Deserialize)]
struct ContainerStatus {
    #[serde(default)]
    ready: bool,
    #[serde(default, rename = "restartCount")]
    restarts: u32,
    #[serde(default)]
    state: ContainerState,
}

/// The state of a container; exactly one member is set once it has one.
#[derive(Debug, Default, Deserialize)]
struct ContainerState {
    #[serde(default)]
    waiting: Option<Reason>,
    #[serde(default)]
    running: Option<serde_json::Value>,
    #[serde(default)]
    terminated: Option<Reason>,
}

/// Why a container is waiting or how it finished.
#[derive(Debug, Default, Deserialize)]
struct Reason {
    #[serde(default)]
    reason: String,
}

fn waiting_reason(container: &ContainerStatus) -> Option<&str> {
    let reason = container.state.waiting.as_ref()?.reason.as_str();
    (!reason.is_empty()).then_some(reason)
}

fn finished_reason(container: &ContainerStatus) -> Option<&str> {
    let reason = container.state.terminated.as_ref()?.reason.as_str();
    (!reason.is_empty()).then_some(reason)
}

/// What the pod is doing, in the words `kubectl get pods` uses.
fn status_of(pod: &RawPod) -> String {
    if pod.metadata.deletion.is_some() {
        return "Terminating".to_owned();
    }
    if let Some(reason) = pod.status.init_containers.iter().find_map(waiting_reason) {
        return format!("Init:{reason}");
    }
    if let Some(reason) = pod.status.containers.iter().find_map(waiting_reason) {
        return reason.to_owned();
    }
    let running = pod
        .status
        .containers
        .iter()
        .any(|container| container.state.running.is_some());
    if !running && let Some(reason) = pod.status.containers.iter().find_map(finished_reason) {
        return reason.to_owned();
    }
    pod.status.phase.clone()
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
            let status = status_of(&p);
            Pod {
                name: p.metadata.name,
                phase: p.status.phase,
                status,
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
                    status: "Running".into(),
                    ready: "1/2".into(),
                    restarts: 2,
                    created: Some(Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap()),
                },
                Pod {
                    name: "worker-1".into(),
                    phase: "Pending".into(),
                    status: "Pending".into(),
                    ready: "0/0".into(),
                    restarts: 0,
                    created: None,
                },
            ]
        );
    }

    /// The status of the one pod in a listing whose `metadata` and `status`
    /// are the given JSON objects.
    fn status_of(metadata: &str, status: &str) -> String {
        let raw = format!(r#"{{"items":[{{"metadata":{metadata},"status":{status}}}]}}"#);
        let pods = parse_pods(raw.as_bytes()).unwrap();
        pods[0].status.clone()
    }

    #[test]
    fn a_waiting_container_gives_the_pod_its_status() {
        let status = status_of(
            r#"{"name":"worker"}"#,
            r#"{"phase":"Running","containerStatuses":[
                {"ready":false,"restartCount":41,
                 "state":{"waiting":{"reason":"CrashLoopBackOff"}}}]}"#,
        );

        assert_eq!(status, "CrashLoopBackOff");
    }

    #[test]
    fn a_finished_container_gives_its_reason_when_nothing_is_running() {
        let failed = status_of(
            r#"{"name":"job"}"#,
            r#"{"phase":"Failed","containerStatuses":[
                {"state":{"terminated":{"reason":"OOMKilled","exitCode":137}}}]}"#,
        );
        let done = status_of(
            r#"{"name":"job"}"#,
            r#"{"phase":"Succeeded","containerStatuses":[
                {"state":{"terminated":{"reason":"Completed","exitCode":0}}}]}"#,
        );

        assert_eq!((failed.as_str(), done.as_str()), ("OOMKilled", "Completed"));
    }

    #[test]
    fn a_running_container_beside_a_finished_one_stays_running() {
        let status = status_of(
            r#"{"name":"api"}"#,
            r#"{"phase":"Running","containerStatuses":[
                {"ready":true,"state":{"running":{"startedAt":"2026-08-01T00:00:00Z"}}},
                {"state":{"terminated":{"reason":"Completed"}}}]}"#,
        );

        assert_eq!(status, "Running");
    }

    #[test]
    fn a_waiting_init_container_shows_as_init() {
        let status = status_of(
            r#"{"name":"api"}"#,
            r#"{"phase":"Pending",
                "initContainerStatuses":[{"state":{"waiting":{"reason":"ImagePullBackOff"}}}],
                "containerStatuses":[{"state":{"waiting":{"reason":"PodInitializing"}}}]}"#,
        );

        assert_eq!(status, "Init:ImagePullBackOff");
    }

    #[test]
    fn a_pod_being_deleted_is_terminating() {
        let status = status_of(
            r#"{"name":"api","deletionTimestamp":"2026-08-01T00:00:00Z"}"#,
            r#"{"phase":"Running","containerStatuses":[{"ready":true,"state":{"running":{}}}]}"#,
        );

        assert_eq!(status, "Terminating");
    }

    #[test]
    fn a_pod_without_container_states_shows_its_phase() {
        assert_eq!(
            status_of(r#"{"name":"api"}"#, r#"{"phase":"Unknown"}"#),
            "Unknown"
        );
        assert_eq!(status_of(r#"{"name":"api"}"#, "{}"), "");
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

    /// A port with exactly the three read methods and nothing else.
    struct ReadOnly;

    #[async_trait]
    impl KubectlPort for ReadOnly {
        async fn get_json(
            &self,
            _namespace: &str,
            _resource: &str,
            _name: &str,
        ) -> Result<Vec<u8>, SvcError> {
            Ok(Vec::new())
        }

        async fn list_json(&self, _namespace: &str, _resource: &str) -> Result<Vec<u8>, SvcError> {
            Ok(Vec::new())
        }

        async fn logs(
            &self,
            _namespace: &str,
            _pod: &str,
            _follow: bool,
        ) -> Result<tokio::sync::mpsc::Receiver<String>, SvcError> {
            Err(SvcError::Other(anyhow::anyhow!("read-only fixture")))
        }
    }

    #[test]
    fn the_port_has_no_write_method() {
        // Compiles only while the trait has no required method beyond the
        // three reads above.
        let _port: &dyn KubectlPort = &ReadOnly;

        let production = include_str!("mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("split yields at least one part");
        assert!(
            production.contains("trait KubectlPort"),
            "the production half of the port module must hold the trait"
        );
        // Anchors: the scan covers the whole trait and stops where the test
        // module starts, so a misplaced marker cannot make it vacuous.
        for read in [
            "fn get_json",
            "fn list_json",
            "fn logs",
            "fn configmap_data",
        ] {
            assert!(
                production.contains(read),
                "the production half of the port module must hold `{read}`"
            );
        }
        assert!(
            !production.contains("mod tests"),
            "the production half must end before the test module"
        );
        let needles = [
            concat!("fn ", "rollout"),
            concat!("fn ", "scale"),
            concat!("fn ", "apply"),
            concat!("fn ", "patch"),
            concat!("fn ", "delete"),
            concat!("fn ", "edit"),
            concat!("fn ", "replace"),
            concat!("fn ", "annotate"),
            concat!("fn ", "label"),
            concat!("fn ", "drain"),
            concat!("fn ", "cordon"),
            concat!("fn ", "taint"),
        ];
        for needle in needles {
            assert!(
                !production.contains(needle),
                "the port source declares {needle}"
            );
        }
    }

    #[test]
    fn configmap_rejects_malformed_json() {
        let err = configmap_data(b"[").unwrap_err();
        assert!(err.to_string().starts_with("parse configmap: "), "{err}");
    }
}
