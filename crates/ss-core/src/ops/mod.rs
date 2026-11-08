//! The operations layer: the lenient [`Input`] every front-end produces, the
//! closed [`View`] union operations return for renderers to visit, and the
//! [`Operation`] trait with its [`Param`] declarations, shared [`Deps`], and
//! [`Registry`].

use std::sync::Arc;

mod confirm;
pub mod demo;
mod input;
mod operation;
mod registry;
mod runner;
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
        .register(Arc::new(demo::Hello))
        .expect("builtin operation ids are unique");
    registry
        .register(Arc::new(demo::JsonDiff))
        .expect("builtin operation ids are unique");
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registers_the_demo_ops() {
        let ids: Vec<_> = builtin().all().iter().map(|op| op.meta().id).collect();
        assert_eq!(ids, ["demo.hello", "demo.json-diff"]);
    }
}
