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
/// End of input declines. Unless the operator ends the prompt line by
/// answering, it is ended on their behalf, so whatever is printed next
/// starts on its own line.
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
                // What is shown comes from the stored data itself; none of
                // it may drive the terminal the answer is typed at.
                crate::tui::text::clean(&render::text(&spec.preview))
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
        let mut prompt_line = PromptLine {
            stderr,
            ended: false,
        };

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
        prompt_line.ended = true;
        let answer = line.trim();
        Ok(if destructive {
            answer == spec.resource
        } else {
            matches!(answer.to_lowercase().as_str(), "y" | "yes")
        })
    }
}

/// An open prompt line on stderr. Dropped before the operator ends it with
/// Enter (end of input, a read error, or the prompt abandoned because the
/// run was cancelled), it ends the line itself.
struct PromptLine {
    stderr: Sink,
    ended: bool,
}

impl Drop for PromptLine {
    fn drop(&mut self) {
        if !self.ended {
            // Nowhere is left to report a failure to end the line.
            let _ = self
                .stderr
                .write_all(b"\n")
                .and_then(|()| self.stderr.flush());
        }
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
    async fn control_characters_in_what_is_shown_do_not_reach_the_terminal() {
        let preview = vec![View::Doc {
            title: "Line diff".to_owned(),
            body: "-old\n+new\x1b[2J\x1b[1;1Hall good\x07\ttabbed".to_owned(),
            lang: "diff".to_owned(),
        }];

        let (_, captured) = answer("n\n", spec(Class::Mutate, preview)).await;

        let shown = captured.stderr();
        assert!(
            !shown.contains('\x1b') && !shown.contains('\x07'),
            "{shown:?}"
        );
        assert!(shown.contains("+new[2J[1;1Hall good"), "{shown}");
        assert!(shown.contains("-old\n"), "line breaks are kept: {shown:?}");
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
    async fn end_of_input_declines_and_ends_the_prompt_line() {
        let (ok, captured) = answer("", spec(Class::Mutate, Vec::new())).await;
        assert!(!ok);
        assert_eq!(captured.stderr(), "Proceed with test.write? [y/N] \n");
    }

    /// A stdin that blocks until its sender is dropped, then reads as empty.
    struct Gate(std::sync::mpsc::Receiver<()>);

    impl std::io::Read for Gate {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    #[tokio::test]
    async fn abandoned_prompt_ends_the_prompt_line() {
        let (release, gate) = std::sync::mpsc::channel();
        let (io, captured) = Io::capture("");
        let confirmer = StdinConfirmer {
            stdin: crate::io::Source::new(std::io::BufReader::new(Gate(gate))),
            stderr: io.stderr,
        };
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            confirmer.confirm(spec(Class::Mutate, Vec::new())),
        )
        .await;
        drop(release);
        assert!(abandoned.is_err(), "the read is still waiting");
        assert_eq!(captured.stderr(), "Proceed with test.write? [y/N] \n");
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
