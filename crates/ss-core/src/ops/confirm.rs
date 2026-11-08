//! The proceed-or-decline question asked before the first mutating step.

use async_trait::async_trait;

use crate::guard::Class;

use super::View;

/// Everything a front-end needs to ask the operator well.
#[derive(Clone, Debug)]
pub struct ConfirmSpec {
    /// The operation being run.
    pub op_id: String,
    /// The operation's blast radius.
    pub class: Class,
    /// The name the operator must type back for destructive operations.
    pub resource: String,
    /// The mutating step about to run.
    pub step: String,
    /// Views describing what is about to change.
    pub preview: Vec<View>,
}

/// Collects the operator's decision at the first mutating step. It runs
/// mid-operation so the preview can show real fetched state.
#[async_trait]
pub trait Confirmer: Send + Sync {
    /// Asks whether to proceed; `Ok(false)` is a decline.
    async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool>;
}
