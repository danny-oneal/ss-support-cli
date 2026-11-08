//! What an invocation is built from: the operation registry and the
//! environment variable snapshot every path and default is resolved against.

use std::path::Path;

use ss_core::ops::{self, Registry};
use ss_core::vars::{self, Vars};

/// The registry and variables one invocation runs against.
#[derive(Clone)]
pub struct Services {
    /// Every operation the CLI can run.
    pub registry: Registry,
    /// The environment variables paths and defaults are resolved from.
    pub vars: Vars,
}

impl Services {
    /// The built-in operations and the process environment.
    pub fn from_process() -> Self {
        Self {
            registry: ops::builtin(),
            vars: vars::process(),
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
        }
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
