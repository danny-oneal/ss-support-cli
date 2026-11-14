//! The operations layer: the lenient [`Input`] every front-end produces, the
//! closed [`View`] union operations return for renderers to visit, and the
//! [`Operation`] trait with its [`Param`] declarations, shared [`Deps`], and
//! [`Registry`].

use std::sync::Arc;

pub mod compare;
mod confirm;
pub mod ddb;
pub mod demo;
pub mod flow;
mod input;
mod operation;
mod registry;
mod runner;
pub mod s3;
mod view;

pub use confirm::{ConfirmSpec, Confirmer};
pub use input::Input;
pub use operation::{BoxFuture, Choice, Deps, Meta, Operation, Param, ParamType, SuggestFn};
pub use registry::{Registry, RegistryError};
pub use runner::{
    FailureKind, Progress, RunFailure, Runner, RunnerError, StepStatus, find_cause, with_defaults,
};
pub use view::{Node, OpResult, View};

/// The registry of every operation that ships with the tool.
pub fn builtin() -> Registry {
    let mut registry = Registry::new();
    registry
        .register(Arc::new(flow::ConfigDrift))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(flow::RecordTrace))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(demo::Hello))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(demo::JsonDiff))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(ddb::GsiConsistency))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(ddb::ItemCompare))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(ddb::ItemHistory))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(s3::ObjectInspect))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(s3::PrefixAudit))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(s3::RestoreVersion))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(s3::VersionDiff))
        .expect("builtin operation ids are unique");
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registers_every_operation() {
        let ids: Vec<_> = builtin().all().iter().map(|op| op.meta().id).collect();
        assert_eq!(
            ids,
            [
                "config.drift",
                "record.trace",
                "demo.hello",
                "demo.json-diff",
                "ddb.gsi-consistency",
                "ddb.item-compare",
                "ddb.item-history",
                "s3.object-inspect",
                "s3.prefix-audit",
                "s3.restore-version",
                "s3.version-diff"
            ]
        );
    }
}
