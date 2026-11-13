//! Operations with scripted behavior for the TUI's unit tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ss_core::guard::Class;
use ss_core::ops::{Input, Meta, OpResult, Operation, Param, Runner, View};

/// What a [`StubOp`] does when it runs.
pub(crate) enum Behavior {
    /// Runs the named read steps, then returns the views.
    Read {
        /// The steps, in order.
        steps: Vec<&'static str>,
        /// The result.
        views: Vec<View>,
    },
    /// Returns one summary view listing the input it was given.
    Echo,
    /// Shows the preview, names the resource, then runs one mutating step
    /// that sets `done`.
    Write {
        /// The resource the operator must type for a destructive run.
        resource: &'static str,
        /// What is about to change.
        preview: Vec<View>,
        /// The mutating step's name.
        step: &'static str,
        /// Set once the mutating step has run.
        done: Arc<AtomicBool>,
    },
    /// A read step named `fetch` fails with this message.
    Fail(&'static str),
    /// A read step named `wait` runs until the run is cancelled.
    Hang,
    /// Waits forever outside any step, so cancelling the run does not end
    /// it.
    Stuck,
    /// Panics.
    Panic,
}

/// An operation with a fixed description and scripted behavior.
pub(crate) struct StubOp {
    /// The operation's description.
    pub(crate) meta: Meta,
    /// The parameters it declares.
    pub(crate) params: Vec<Param>,
    /// What it does.
    pub(crate) behavior: Behavior,
}

impl StubOp {
    /// A stub named after its id, in the `Stub` category, with no
    /// parameters.
    pub(crate) fn new(id: &'static str, class: Class, behavior: Behavior) -> StubOp {
        StubOp {
            meta: Meta {
                id,
                name: id,
                summary: "a stub operation",
                category: "Stub",
                class,
            },
            params: Vec::new(),
            behavior,
        }
    }

    /// A read-only stub that runs `steps` and returns one summary view.
    pub(crate) fn read(id: &'static str, steps: &[&'static str]) -> StubOp {
        StubOp::new(
            id,
            Class::Read,
            Behavior::Read {
                steps: steps.to_vec(),
                views: vec![View::summary([("Result", "ok")])],
            },
        )
    }

    /// A stub of `class` whose one mutating step, `apply`, changes
    /// `resource`; the returned flag is set once it has run.
    pub(crate) fn write(
        id: &'static str,
        class: Class,
        resource: &'static str,
    ) -> (StubOp, Arc<AtomicBool>) {
        let done = Arc::new(AtomicBool::new(false));
        let op = StubOp::new(
            id,
            class,
            Behavior::Write {
                resource,
                preview: vec![View::summary([("Will change", resource)])],
                step: "apply",
                done: Arc::clone(&done),
            },
        );
        (op, done)
    }

    /// This stub declaring `params`.
    pub(crate) fn with_params(mut self, params: Vec<Param>) -> StubOp {
        self.params = params;
        self
    }

    /// This stub shown as `name`, described by `summary`, under `category`.
    pub(crate) fn described(
        mut self,
        name: &'static str,
        summary: &'static str,
        category: &'static str,
    ) -> StubOp {
        self.meta.name = name;
        self.meta.summary = summary;
        self.meta.category = category;
        self
    }
}

#[async_trait]
impl Operation for StubOp {
    fn meta(&self) -> Meta {
        self.meta
    }

    fn params(&self) -> Vec<Param> {
        self.params.clone()
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        match &self.behavior {
            Behavior::Read { steps, views } => {
                for step in steps {
                    r.step(step, |_| async { Ok(()) }).await?;
                }
                Ok(OpResult {
                    views: views.clone(),
                })
            }
            Behavior::Echo => Ok(OpResult {
                views: vec![View::summary(input.as_map().clone())],
            }),
            Behavior::Write {
                resource,
                preview,
                step,
                done,
            } => {
                r.preview(preview.clone());
                r.set_resource(*resource);
                let done = Arc::clone(done);
                r.mutate(step, |_| async move {
                    done.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await?;
                Ok(OpResult {
                    views: vec![View::summary([("Changed", *resource)])],
                })
            }
            Behavior::Fail(message) => {
                r.step("fetch", |_| async {
                    Err::<(), _>(anyhow::anyhow!(*message))
                })
                .await?;
                Ok(OpResult::default())
            }
            Behavior::Hang => {
                r.step("wait", |token| async move {
                    token.cancelled().await;
                    Ok(())
                })
                .await?;
                Ok(OpResult::default())
            }
            Behavior::Stuck => std::future::pending().await,
            Behavior::Panic => panic!("stub operation panicked"),
        }
    }
}
