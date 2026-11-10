//! A [`DdbPort`] that builds its backing port on first use.

use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use tokio::sync::OnceCell;

use super::{DdbPort, Item, ItemPage, Key, QuerySpec, TableInfo};
use crate::ops::BoxFuture;
use crate::svc::SvcError;

type Build = dyn Fn() -> BoxFuture<anyhow::Result<Arc<dyn DdbPort>>> + Send + Sync;

/// Defers building the backing port until a call needs it.
///
/// The build runs at most once, even when callers race. A failed build is
/// remembered, and every call reports the same message.
pub struct Lazy {
    build: Box<Build>,
    port: OnceCell<Result<Arc<dyn DdbPort>, String>>,
}

impl Lazy {
    /// A port that calls `build` the first time any method is used.
    pub fn new(
        build: impl Fn() -> BoxFuture<anyhow::Result<Arc<dyn DdbPort>>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            build: Box::new(build),
            port: OnceCell::new(),
        }
    }

    async fn resolve(&self) -> Result<&Arc<dyn DdbPort>, SvcError> {
        let built = self
            .port
            .get_or_init(|| async { (self.build)().await.map_err(|e| format!("{e:#}")) })
            .await;
        built
            .as_ref()
            .map_err(|msg| SvcError::Other(anyhow!(msg.clone())))
    }
}

#[async_trait]
impl DdbPort for Lazy {
    async fn list_tables(&self) -> Result<Vec<String>, SvcError> {
        self.resolve().await?.list_tables().await
    }

    async fn describe_table(&self, table: &str) -> Result<TableInfo, SvcError> {
        self.resolve().await?.describe_table(table).await
    }

    async fn get_item(&self, table: &str, key: &Key) -> Result<Item, SvcError> {
        self.resolve().await?.get_item(table, key).await
    }

    async fn query(&self, q: &QuerySpec) -> Result<ItemPage, SvcError> {
        self.resolve().await?.query(q).await
    }

    async fn query_index(
        &self,
        table: &str,
        index: &str,
        q: &QuerySpec,
    ) -> Result<ItemPage, SvcError> {
        self.resolve().await?.query_index(table, index, q).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context, anyhow};

    use super::super::DdbPort;
    use super::super::FakeDdb;
    use super::super::contract;
    use super::*;

    fn seeded() -> FakeDdb {
        let ddb = FakeDdb::new();
        ddb.create_table(contract::table_info("tenants"));
        for item in contract::seed_items() {
            ddb.put("tenants", item);
        }
        ddb
    }

    fn counting(port: FakeDdb) -> (Lazy, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let lazy = Lazy::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            let port: Arc<dyn DdbPort> = Arc::new(port.clone());
            Box::pin(async move { Ok(port) })
        });
        (lazy, count)
    }

    #[tokio::test]
    async fn lazy_does_not_build_until_used() {
        let (_lazy, count) = counting(FakeDdb::new());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lazy_builds_once_across_calls() {
        let (lazy, count) = counting(FakeDdb::new());
        for _ in 0..3 {
            lazy.list_tables().await.expect("list tables");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lazy_build_failure_is_reported_on_every_call_and_built_once() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let lazy = Lazy::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(anyhow!("no credentials")).context("load AWS config") })
        });
        for _ in 0..2 {
            let err = lazy.list_tables().await.expect_err("build fails");
            assert_eq!(err.to_string(), "load AWS config: no credentials");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lazy_satisfies_the_contract() {
        let (lazy, _) = counting(seeded());
        contract::run(&lazy, "tenants", "nope").await;
    }
}
