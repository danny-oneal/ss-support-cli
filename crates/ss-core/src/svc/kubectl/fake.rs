//! An in-memory [`KubectlPort`] for tests.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;

use super::{KubectlPort, LOG_BUFFER};
use crate::svc::SvcError;

#[derive(Debug, Default)]
struct State {
    /// Single resources keyed by `(namespace, resource, name)`.
    items: BTreeMap<(String, String, String), Vec<u8>>,
    /// Listings keyed by `(namespace, resource)`.
    lists: BTreeMap<(String, String), Vec<u8>>,
    log_lines: Vec<String>,
    /// Namespaces that exist without holding anything seeded.
    namespaces: BTreeSet<String>,
}

/// A [`KubectlPort`] backed by canned responses. Clones share their data.
#[derive(Debug, Clone, Default)]
pub struct FakeKubectl {
    state: Arc<Mutex<State>>,
}

impl FakeKubectl {
    /// An empty fake: every lookup reports not found.
    pub fn new() -> Self {
        Self::default()
    }

    /// Serves `json` for `get_json(namespace, resource, name)`.
    pub fn insert(&self, namespace: &str, resource: &str, name: &str, json: impl Into<Vec<u8>>) {
        self.lock().items.insert(
            (namespace.to_owned(), resource.to_owned(), name.to_owned()),
            json.into(),
        );
    }

    /// Serves `json` for `list_json(namespace, resource)`.
    pub fn insert_list(&self, namespace: &str, resource: &str, json: impl Into<Vec<u8>>) {
        self.lock()
            .lists
            .insert((namespace.to_owned(), resource.to_owned()), json.into());
    }

    /// Makes `namespace` exist though nothing is seeded in it. A namespace
    /// also exists once a resource or a listing has been seeded in it.
    pub fn insert_namespace(&self, namespace: &str) {
        self.lock().namespaces.insert(namespace.to_owned());
    }

    /// Sets the lines `logs` replays before closing the channel.
    pub fn set_log_lines(&self, lines: Vec<String>) {
        self.lock().log_lines = lines;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl KubectlPort for FakeKubectl {
    async fn get_json(
        &self,
        namespace: &str,
        resource: &str,
        name: &str,
    ) -> Result<Vec<u8>, SvcError> {
        let state = self.lock();
        // A namespace is a resource of its own, outside any namespace.
        if namespace.is_empty() && resource == "namespace" {
            let exists = state.namespaces.contains(name)
                || state.items.keys().any(|(ns, _, _)| ns == name)
                || state.lists.keys().any(|(ns, _)| ns == name);
            return if exists {
                Ok(format!(r#"{{"metadata":{{"name":{name:?}}}}}"#).into_bytes())
            } else {
                Err(SvcError::not_found(format!("namespace/{name}")))
            };
        }
        let key = (namespace.to_owned(), resource.to_owned(), name.to_owned());
        state
            .items
            .get(&key)
            .cloned()
            .ok_or_else(|| SvcError::not_found(format!("{namespace}/{resource}/{name}")))
    }

    async fn list_json(&self, namespace: &str, resource: &str) -> Result<Vec<u8>, SvcError> {
        let key = (namespace.to_owned(), resource.to_owned());
        // kubectl lists a namespace that does not exist as empty.
        Ok(self
            .lock()
            .lists
            .get(&key)
            .cloned()
            .unwrap_or_else(|| br#"{"items":[]}"#.to_vec()))
    }

    async fn logs(
        &self,
        _namespace: &str,
        _pod: &str,
        _follow: bool,
    ) -> Result<tokio::sync::mpsc::Receiver<String>, SvcError> {
        let lines = self.lock().log_lines.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(LOG_BUFFER.max(lines.len()));
        for line in lines {
            // The channel holds every line, so this never waits or fails.
            if tx.try_send(line).is_err() {
                break;
            }
        }
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_serves_inserted_resources_and_reports_missing_ones() {
        let k = FakeKubectl::new();
        k.insert("menu", "configmap", "menu-config", b"{}".to_vec());
        k.insert_list("default", "pods", b"{\"items\":[]}".to_vec());

        assert_eq!(
            k.get_json("menu", "configmap", "menu-config")
                .await
                .unwrap(),
            b"{}"
        );
        let err = k.get_json("menu", "configmap", "x").await.unwrap_err();
        assert!(err.is_not_found());
        assert!(err.to_string().contains("menu/configmap/x"), "{err}");

        assert_eq!(
            k.list_json("default", "pods").await.unwrap(),
            b"{\"items\":[]}"
        );
        // A listing nobody seeded is empty, as kubectl reports a resource
        // type with nothing in it and a namespace that does not exist.
        assert_eq!(
            k.list_json("default", "svc").await.unwrap(),
            b"{\"items\":[]}"
        );
        assert_eq!(
            k.list_json("nowhere", "pods").await.unwrap(),
            b"{\"items\":[]}"
        );
    }

    #[tokio::test]
    async fn a_namespace_exists_once_it_is_named_or_holds_something() {
        let k = FakeKubectl::new();
        k.insert("menu", "configmap", "menu-config", b"{}".to_vec());
        k.insert_list("orders", "pods", b"{\"items\":[]}".to_vec());
        k.insert_namespace("empty");

        for known in ["menu", "orders", "empty"] {
            assert!(
                k.get_json("", "namespace", known).await.is_ok(),
                "{known} exists"
            );
        }
        let err = k.get_json("", "namespace", "nowhere").await.unwrap_err();
        assert!(err.is_not_found(), "{err}");
    }

    #[tokio::test]
    async fn fake_replays_log_lines_then_closes() {
        let k = FakeKubectl::new();
        k.set_log_lines(vec!["a".into(), "b".into()]);
        let mut rx = k.logs("ns", "pod", false).await.unwrap();
        assert_eq!(rx.recv().await.as_deref(), Some("a"));
        assert_eq!(rx.recv().await.as_deref(), Some("b"));
        assert_eq!(rx.recv().await, None);
    }
}
