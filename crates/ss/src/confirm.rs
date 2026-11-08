//! The terminal confirmation prompt asked before an operation's first
//! mutating step.

use std::io::Write;

use anyhow::Context;
use async_trait::async_trait;
use ss_core::guard::Class;
use ss_core::ops::{ConfirmSpec, Confirmer};
use ss_core::render;

use crate::io::{Sink, Source};

/// Shows the preview on stderr and reads the answer from stdin: `y`/`yes`
/// for a mutating operation, the exact resource name for a destructive one.
/// End of input declines.
pub(crate) struct StdinConfirmer {
    /// Supplies the operator's answer.
    pub stdin: Source,
    /// Receives the preview and the prompt.
    pub stderr: Sink,
}

#[async_trait]
impl Confirmer for StdinConfirmer {
    async fn confirm(&self, spec: ConfirmSpec) -> anyhow::Result<bool> {
        let destructive = spec.class == Class::Destructive;
        let mut prompt = String::new();
        if !spec.preview.is_empty() {
            prompt = format!(
                "\nAbout to run {} ({}) — step {:?} will change:\n\n{}\n",
                spec.op_id,
                spec.class,
                spec.step,
                render::text(&spec.preview)
            );
        }
        if destructive {
            prompt += &format!("This is DESTRUCTIVE. Type {:?} to confirm: ", spec.resource);
        } else {
            prompt += &format!("Proceed with {}? [y/N] ", spec.op_id);
        }
        let mut stderr = self.stderr.clone();
        stderr
            .write_all(prompt.as_bytes())
            .and_then(|()| stderr.flush())
            .context("write confirmation prompt")?;

        // The read blocks, so it runs off the async workers: a Ctrl-C while
        // the operator is deciding can then stop the run at once.
        let stdin = self.stdin.clone();
        let line = tokio::task::spawn_blocking(move || stdin.read_line())
            .await
            .context("read confirmation")?
            .context("read confirmation")?;
        let Some(line) = line else {
            return Ok(false);
        };
        let answer = line.trim();
        Ok(if destructive {
            answer == spec.resource
        } else {
            matches!(answer.to_lowercase().as_str(), "y" | "yes")
        })
    }
}

#[cfg(test)]
mod tests {
    use ss_core::ops::View;

    use super::*;
    use crate::io::{Captured, Io};

    fn spec(class: Class, preview: Vec<View>) -> ConfirmSpec {
        ConfirmSpec {
            op_id: "test.write".to_owned(),
            class,
            resource: "cfg/acme.json".to_owned(),
            step: "write one".to_owned(),
            preview,
        }
    }

    async fn answer(stdin: &str, spec: ConfirmSpec) -> (bool, Captured) {
        let (io, captured) = Io::capture(stdin);
        let confirmer = StdinConfirmer {
            stdin: io.stdin,
            stderr: io.stderr,
        };
        let ok = confirmer.confirm(spec).await.expect("confirm");
        (ok, captured)
    }

    #[tokio::test]
    async fn yes_answers_proceed() {
        for input in ["y\n", "YES\n", "  yes  \n"] {
            let (ok, captured) = answer(input, spec(Class::Mutate, Vec::new())).await;
            assert!(ok, "{input:?}");
            assert_eq!(captured.stderr(), "Proceed with test.write? [y/N] ");
        }
    }

    #[tokio::test]
    async fn anything_else_declines() {
        for input in ["n\n", "", "\n", "yep\n"] {
            let (ok, _) = answer(input, spec(Class::Mutate, Vec::new())).await;
            assert!(!ok, "{input:?}");
        }
    }

    #[tokio::test]
    async fn destructive_requires_the_exact_resource() {
        let (ok, captured) = answer("cfg/acme.json\n", spec(Class::Destructive, Vec::new())).await;
        assert!(ok);
        assert_eq!(
            captured.stderr(),
            "This is DESTRUCTIVE. Type \"cfg/acme.json\" to confirm: "
        );
        for input in ["cfg\n", "y\n", ""] {
            let (ok, _) = answer(input, spec(Class::Destructive, Vec::new())).await;
            assert!(!ok, "{input:?}");
        }
    }

    #[tokio::test]
    async fn preview_is_rendered_before_the_prompt() {
        let preview = vec![View::summary([("Key", "cfg/acme.json")])];
        let (_, captured) = answer("n\n", spec(Class::Mutate, preview)).await;
        assert_eq!(
            captured.stderr(),
            "\nAbout to run test.write (mutate) — step \"write one\" will change:\n\n\
             Key : cfg/acme.json\n\nProceed with test.write? [y/N] "
        );
    }
}
