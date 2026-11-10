//! `ss-support run <operation>`: resolve the input, run the operation, print the
//! result, then export the report and save the preset when asked.

use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use chrono::Utc;
use ss_core::ops::{
    FailureKind, Input, Progress, Registry, RunFailure, Runner, StepStatus, with_defaults,
};
use ss_core::preset::{self, Store};
use ss_core::{jsonutil, render, report};

use super::{note, print};
use crate::cli::{GlobalFlags, OpCall, Output, RunCmd};
use crate::confirm::StdinConfirmer;
use crate::io::{Io, Sink};
use crate::run::{App, Failure};

pub(super) async fn run(
    cmd: &RunCmd,
    global: &GlobalFlags,
    registry: &Registry,
    app: &App,
    io: &Io,
) -> Result<(), Failure> {
    let format = global
        .output_format()
        .map_err(|e| Failure::Usage(anyhow!(e)))?;
    let call = cmd
        .call
        .as_ref()
        .ok_or_else(|| Failure::Usage(anyhow!("no operation given (try `ss-support ops list`)")))?;
    let op = registry.get(&call.id).ok_or_else(|| {
        Failure::Usage(anyhow!(
            "unknown operation {:?} (try `ss-support ops list`)",
            call.id
        ))
    })?;
    let meta = op.meta();
    let input = resolve_input(non_empty(&cmd.preset), call, &app.presets)?;
    // A preset that could not be saved is refused before anything runs.
    if let Some(name) = non_empty(&cmd.save_preset) {
        preset::validate(meta.id, name, input.as_map())?;
    }

    let mut runner = Runner::new(app.deps.clone())
        .guard(Arc::clone(&app.guard))
        .audit(Arc::clone(&app.audit))
        .dry_run(global.dry_run)
        .yes(global.yes)
        .user(app.user.clone())
        .cancel(app.cancel.clone())
        .on_warning({
            let stderr = io.stderr.clone();
            move |w| {
                // Nowhere is left to report a failure to write a warning.
                let _ = writeln!(stderr.clone(), "warning: {w}");
            }
        })
        .confirmer(Arc::new(StdinConfirmer {
            stdin: io.stdin.clone(),
            stderr: io.stderr.clone(),
        }));
    if global.verbose {
        let stderr = io.stderr.clone();
        runner = runner.progress(move |p| show_progress(&stderr, &p));
    }
    let result = runner
        .execute(op.as_ref(), input.clone())
        .await
        .map_err(failure)?;

    match format {
        Output::Text => print(io, &render::text(&result.views))?,
        Output::Json => print(
            io,
            &format!("{}\n", jsonutil::pretty(&render::json(&result.views))),
        )?,
    }

    if let Some(path) = non_empty(&global.export) {
        let resolved = with_defaults(op.as_ref(), input.clone());
        let md = report::markdown(&meta, &resolved, &result.views, Utc::now());
        fs::write(path, md)
            .with_context(|| format!("export {path}"))
            .map_err(Failure::Failed)?;
        note(io, &format!("exported to {path}"));
    }

    // Only a run that succeeded is worth replaying, and only the params the
    // user chose are saved, so later changes to defaults still apply.
    if let Some(name) = non_empty(&cmd.save_preset) {
        app.presets.save(meta.id, name, input.as_map())?;
        note(io, &format!("saved preset {name:?} for {}", meta.id));
    }
    Ok(())
}

/// The preset's params, if one is named, overlaid by the params given on the
/// command line.
fn resolve_input(preset: Option<&str>, call: &OpCall, store: &Store) -> Result<Input, Failure> {
    let mut input = Input::default();
    if let Some(name) = preset {
        let Some(params) = store.get(&call.id, name)? else {
            let names = store.list(&call.id)?;
            let have = if names.is_empty() {
                "none saved".to_owned()
            } else {
                format!("have: {}", names.join(", "))
            };
            return Err(Failure::Usage(anyhow!(
                "no preset {name:?} for {} ({have}); save one with --save-preset",
                call.id
            )));
        };
        input = params.into_iter().collect();
    }
    for (k, v) in call.input.as_map() {
        input.insert(k.clone(), v.clone());
    }
    Ok(input)
}

/// A flag given as an empty string counts as not given.
fn non_empty(flag: &Option<String>) -> Option<&str> {
    flag.as_deref().filter(|s| !s.is_empty())
}

/// A missing or invalid param is the user's to fix; Ctrl-C, SIGTERM, and SIGHUP
/// are an interruption; every other failure (step error, guard denial,
/// decline) is the operation's.
fn failure(f: RunFailure) -> Failure {
    match f.kind {
        FailureKind::Usage => Failure::Usage(f.error),
        FailureKind::Cancelled => Failure::Interrupted(f.error),
        FailureKind::Denied | FailureKind::Declined | FailureKind::Failed => {
            Failure::Failed(f.error)
        }
    }
}

fn show_progress(stderr: &Sink, p: &Progress) {
    // Progress is best effort; a closed stderr must not fail the run.
    let _ = stderr.clone().write_all(progress_line(p).as_bytes());
}

/// One stderr line per step event.
fn progress_line(p: &Progress) -> String {
    let step = &p.step;
    match p.status {
        StepStatus::Running => format!("→ {step} ...\n"),
        StepStatus::Done => match round_to_millis(p.elapsed) {
            Duration::ZERO => format!("✓ {step} (0s)\n"),
            d => format!("✓ {step} ({d:?})\n"),
        },
        StepStatus::Failed => format!("✗ {step}: {}\n", p.err.as_deref().unwrap_or_default()),
        StepStatus::Skipped => format!("⊘ {step} (dry-run)\n"),
    }
}

/// Rounds half up to whole milliseconds.
fn round_to_millis(d: Duration) -> Duration {
    let millis = (d.as_nanos() + 500_000) / 1_000_000;
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(status: StepStatus, elapsed: Duration, err: Option<&str>) -> Progress {
        Progress {
            step: "fetch".to_owned(),
            status,
            elapsed,
            err: err.map(str::to_owned),
        }
    }

    #[test]
    fn progress_lines() {
        assert_eq!(
            progress_line(&event(StepStatus::Running, Duration::ZERO, None)),
            "→ fetch ...\n"
        );
        assert_eq!(
            progress_line(&event(
                StepStatus::Done,
                Duration::from_micros(150_499),
                None
            )),
            "✓ fetch (150ms)\n"
        );
        assert_eq!(
            progress_line(&event(StepStatus::Done, Duration::from_micros(499), None)),
            "✓ fetch (0s)\n"
        );
        assert_eq!(
            progress_line(&event(
                StepStatus::Done,
                Duration::from_micros(1_499_500),
                None
            )),
            "✓ fetch (1.5s)\n"
        );
        assert_eq!(
            progress_line(&event(
                StepStatus::Failed,
                Duration::from_millis(3),
                Some("AccessDenied")
            )),
            "✗ fetch: AccessDenied\n"
        );
        assert_eq!(
            progress_line(&event(StepStatus::Skipped, Duration::ZERO, None)),
            "⊘ fetch (dry-run)\n"
        );
    }
}
