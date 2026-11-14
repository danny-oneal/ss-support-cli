//! A confirmer for tests: it answers every question the same way, keeps
//! what it was shown, and can change the world while the operator decides.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{ConfirmSpec, Confirmer};

/// Answers every confirmation the same way and keeps what it was shown.
pub(crate) struct Asked {
    answer: bool,
    /// Every confirmation asked for, in order.
    pub(crate) seen: Mutex<Vec<ConfirmSpec>>,
    /// Runs while the operator is deciding.
    meanwhile: Box<dyn Fn() + Send + Sync>,
}

impl Asked {
    /// A confirmer that answers `answer`.
    pub(crate) fn answering(answer: bool) -> Arc<Asked> {
        Asked::after(answer, || {})
    }

    /// A confirmer that runs `meanwhile` and then answers `answer`.
    pub(crate) fn after(answer: bool, meanwhile: impl Fn() + Send + Sync + 'static) -> Arc<Asked> {
        Arc::new(Asked {
            answer,
            seen: Mutex::new(Vec::new()),
            meanwhile: Box::new(meanwhile),
        })
    }

    /// How many times it was asked.
    pub(crate) fn times(&self) -> usize {
        self.seen.lock().expect("seen").len()
    }
}

#[async_trait]
impl Confirmer for Asked {
    async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool> {
        (self.meanwhile)();
        self.seen.lock().expect("seen").push(spec);
        Ok(self.answer)
    }
}
