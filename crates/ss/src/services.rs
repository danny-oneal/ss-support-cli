//! What an invocation is built from: the operation registry and the
//! environment variable snapshot every path and default is resolved against.

use std::path::Path;
use std::sync::Arc;

use ss_core::config::Environment;
use ss_core::ops::{self, Registry};
use ss_core::svc::s3::{Lazy, S3Port};
use ss_core::vars::{self, Vars};

use crate::aws;

/// Builds the S3 port for the resolved environment and variable snapshot.
pub type S3Factory = Arc<dyn Fn(&Environment, &Vars) -> Arc<dyn S3Port> + Send + Sync>;

/// The registry, variables, and S3 source one invocation runs against.
#[derive(Clone)]
pub struct Services {
    /// Every operation the CLI can run.
    pub registry: Registry,
    /// The environment variables paths and defaults are resolved from.
    pub vars: Vars,
    /// Builds the S3 port for the resolved environment.
    pub s3: S3Factory,
}

impl Services {
    /// The built-in operations and the process environment.
    pub fn from_process() -> Self {
        Self {
            registry: ops::builtin(),
            vars: vars::process(),
            s3: aws::s3_factory(),
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
                Arc::new(Lazy::new(|| {
                    Box::pin(async { Err(anyhow::anyhow!("S3 is not configured")) })
                }))
            }),
        }
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

    use super::*;

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
