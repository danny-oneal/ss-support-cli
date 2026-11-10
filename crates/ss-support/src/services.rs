//! What an invocation is built from: the operation registry and the
//! environment variable snapshot every path and default is resolved against.

use std::path::Path;
use std::sync::Arc;

use ss_core::config::Environment;
use ss_core::ops::{self, Registry};
use ss_core::svc::ddb::{self, DdbPort};
use ss_core::svc::s3::{self, S3Port};
use ss_core::vars::{self, Vars};

use crate::aws;

/// Builds the S3 port for the resolved environment and variable snapshot.
pub type S3Factory = Arc<dyn Fn(&Environment, &Vars) -> Arc<dyn S3Port> + Send + Sync>;

/// Builds the DynamoDB port for the resolved environment and variable
/// snapshot.
pub type DdbFactory = Arc<dyn Fn(&Environment, &Vars) -> Arc<dyn DdbPort> + Send + Sync>;

/// The registry, variables, and S3 and DynamoDB sources one invocation runs
/// against.
#[derive(Clone)]
pub struct Services {
    /// Every operation the CLI can run.
    pub registry: Registry,
    /// The environment variables paths and defaults are resolved from.
    pub vars: Vars,
    /// Builds the S3 port for the resolved environment.
    pub s3: S3Factory,
    /// Builds the DynamoDB port for the resolved environment.
    pub ddb: DdbFactory,
}

/// A factory whose port fails every call with "DynamoDB is not configured".
fn unconfigured_ddb() -> DdbFactory {
    Arc::new(|_, _| {
        Arc::new(ddb::Lazy::new(|| {
            Box::pin(async { Err(anyhow::anyhow!("DynamoDB is not configured")) })
        }))
    })
}

impl Services {
    /// The built-in operations and the process environment.
    pub fn from_process() -> Self {
        Self {
            registry: ops::builtin(),
            vars: vars::process(),
            s3: aws::s3_factory(),
            ddb: unconfigured_ddb(),
        }
    }

    /// The built-in operations with a home directory of `root` and nothing
    /// else set, so config, presets, audit log, and cache all live under
    /// `root`.
    pub fn isolated(root: &Path) -> Self {
        Self {
            registry: ops::builtin(),
            vars: Vars::from([
                ("HOME".to_owned(), root.display().to_string()),
                ("USER".to_owned(), "tester".to_owned()),
            ]),
            s3: Arc::new(|_, _| {
                Arc::new(s3::Lazy::new(|| {
                    Box::pin(async { Err(anyhow::anyhow!("S3 is not configured")) })
                }))
            }),
            ddb: unconfigured_ddb(),
        }
    }

    /// These services with every invocation using `port` for DynamoDB.
    pub fn with_ddb(mut self, port: Arc<dyn DdbPort>) -> Self {
        self.ddb = Arc::new(move |_, _| Arc::clone(&port));
        self
    }

    /// These services with every invocation using `port` for S3.
    pub fn with_s3(mut self, port: Arc<dyn S3Port>) -> Self {
        self.s3 = Arc::new(move |_, _| Arc::clone(&port));
        self
    }
}

#[cfg(test)]
mod tests {
    use ss_core::{audit, cache, config, preset};

    use ss_core::svc::ddb::{FakeDdb, contract};

    use super::*;

    fn services() -> Services {
        Services::isolated(Path::new("/tmp/ss-isolated"))
    }

    #[tokio::test]
    async fn isolated_ddb_is_unconfigured() {
        let s = services();
        let port = (s.ddb)(&Environment::default(), &s.vars);
        let err = port.list_tables().await.expect_err("unconfigured");
        assert!(
            err.to_string().contains("DynamoDB is not configured"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn with_ddb_serves_the_given_port() {
        let fake = FakeDdb::new();
        fake.create_table(contract::table_info("tenants"));
        let s = services().with_ddb(Arc::new(fake));
        let port = (s.ddb)(&Environment::default(), &s.vars);
        assert_eq!(
            port.list_tables().await.expect("list tables"),
            vec!["tenants".to_owned()]
        );
    }

    #[test]
    fn isolated_paths_stay_under_root() {
        let root = Path::new("/tmp/ss-isolated");
        let services = Services::isolated(root);
        let v = &services.vars;
        for path in [
            config::default_path(v),
            preset::default_path(v),
            audit::default_path(v),
            cache::default_dir(v),
        ] {
            assert!(
                path.starts_with(root),
                "{} escapes {}",
                path.display(),
                root.display()
            );
        }
    }
}
