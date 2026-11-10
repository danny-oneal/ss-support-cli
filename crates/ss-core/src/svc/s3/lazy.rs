//! An [`S3Port`] that builds its backing port on first use.

use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use tokio::sync::OnceCell;

use super::{Bucket, Object, ObjectMeta, Page, S3Port, Version};
use crate::ops::BoxFuture;
use crate::svc::SvcError;

type Build = dyn Fn() -> BoxFuture<anyhow::Result<Arc<dyn S3Port>>> + Send + Sync;

/// Defers building the backing port until a call needs it.
///
/// The build runs at most once, even when callers race. A failed build is
/// remembered, and every call reports the same message.
pub struct Lazy {
    build: Box<Build>,
    port: OnceCell<Result<Arc<dyn S3Port>, String>>,
}

impl Lazy {
    /// A port that calls `build` the first time any method is used.
    pub fn new(
        build: impl Fn() -> BoxFuture<anyhow::Result<Arc<dyn S3Port>>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            build: Box::new(build),
            port: OnceCell::new(),
        }
    }

    async fn resolve(&self) -> Result<&Arc<dyn S3Port>, SvcError> {
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
impl S3Port for Lazy {
    async fn list_buckets(&self) -> Result<Vec<Bucket>, SvcError> {
        self.resolve().await?.list_buckets().await
    }

    async fn list(
        &self,
        bucket: &str,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<Page, SvcError> {
        self.resolve().await?.list(bucket, prefix, token).await
    }

    async fn list_versions(&self, bucket: &str, key: &str) -> Result<Vec<Version>, SvcError> {
        self.resolve().await?.list_versions(bucket, key).await
    }

    async fn get_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Object, SvcError> {
        self.resolve()
            .await?
            .get_version(bucket, key, version_id)
            .await
    }

    async fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, SvcError> {
        self.resolve().await?.head(bucket, key).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::{Context, anyhow};

    use super::super::FakeS3;
    use super::super::S3Port;
    use super::super::contract::{fixture, run, seeded_fake};
    use super::*;

    fn counting(port: FakeS3) -> (Lazy, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let lazy = Lazy::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            let port: Arc<dyn S3Port> = Arc::new(port.clone());
            Box::pin(async move { Ok(port) })
        });
        (lazy, count)
    }

    #[tokio::test]
    async fn lazy_does_not_build_until_used() {
        let (_lazy, count) = counting(FakeS3::new());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lazy_builds_once_across_calls() {
        let (lazy, count) = counting(FakeS3::new());
        for _ in 0..3 {
            lazy.list_buckets().await.expect("list buckets");
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
            let err = lazy.list_buckets().await.expect_err("build fails");
            assert_eq!(err.to_string(), "load AWS config: no credentials");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lazy_satisfies_the_contract() {
        let (fake, ids, marker) = seeded_fake();
        let (lazy, _) = counting(fake.clone());
        let (paged, _) = counting(fake.paged(2));
        run(&fixture(&lazy, &paged, ids, marker)).await;
    }
}
