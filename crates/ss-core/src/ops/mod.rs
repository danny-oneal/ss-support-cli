//! The operations layer: the lenient [`Input`] every front-end produces, the
//! closed [`View`] union operations return for renderers to visit, and the
//! [`Operation`] trait with its [`Param`] declarations, shared [`Deps`], and
//! [`Registry`].

mod confirm;
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
