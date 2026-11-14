//! The live checklist of a run's steps.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ss_core::ops::{Progress, StepStatus};

use crate::cmd::elapsed_text;
use crate::tui::theme;

/// The steps a run has reported, in the order they started.
#[derive(Debug, Clone, Default)]
pub struct ProgressList {
    steps: Vec<Progress>,
}

impl ProgressList {
    /// Records a step event: a finished or failed step replaces its running
    /// row; anything else adds a row.
    pub fn apply(&mut self, event: Progress) {
        let running = self
            .steps
            .iter_mut()
            .rev()
            .find(|row| row.step == event.step && row.status == StepStatus::Running);
        match running {
            Some(row) if event.status != StepStatus::Running => *row = event,
            _ => self.steps.push(event),
        }
    }

    /// True when no step has been reported.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// How many rows the list has.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Draws one row per step; when there are more steps than rows, the
    /// latest are shown.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let hidden = self.steps.len().saturating_sub(usize::from(area.height));
        let rows: Vec<Line> = self.steps.iter().skip(hidden).map(row).collect();
        frame.render_widget(Paragraph::new(rows), area);
    }
}

fn row(step: &Progress) -> Line<'static> {
    let name = &step.step;
    match step.status {
        StepStatus::Running => Line::styled(format!("→ {name} …"), theme::changed()),
        StepStatus::Done => Line::styled(
            format!("✓ {name} ({})", elapsed_text(step.elapsed)),
            theme::added(),
        ),
        StepStatus::Failed => Line::styled(
            format!("✗ {name}: {}", step.err.as_deref().unwrap_or_default()),
            theme::error(),
        ),
        StepStatus::Skipped => Line::styled(format!("⊘ {name} (skipped)"), theme::dim()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ss_core::ops::StepStatus;

    use super::*;
    use crate::tui::testing::render_with;

    fn event(step: &str, status: StepStatus, millis: u64, err: Option<&str>) -> Progress {
        Progress {
            step: step.to_owned(),
            status,
            elapsed: Duration::from_millis(millis),
            err: err.map(str::to_owned),
        }
    }

    fn drawn(list: &ProgressList, width: u16, height: u16) -> String {
        render_with(width, height, |frame| list.render(frame, frame.area()))
    }

    #[test]
    fn progress_events_render_as_a_checklist() {
        let mut list = ProgressList::default();
        list.apply(event("fetch both versions", StepStatus::Running, 0, None));
        list.apply(event("fetch both versions", StepStatus::Done, 150, None));
        list.apply(event("compare", StepStatus::Running, 0, None));
        list.apply(event(
            "compare",
            StepStatus::Failed,
            3,
            Some("AccessDenied"),
        ));
        list.apply(event("apply", StepStatus::Skipped, 0, None));
        list.apply(event("report", StepStatus::Running, 0, None));

        assert_eq!(
            drawn(&list, 40, 5),
            "✓ fetch both versions (150ms)\n\
             ✗ compare: AccessDenied\n\
             ⊘ apply (skipped)\n\
             → report …\n\
             \n"
        );
    }

    #[test]
    fn a_step_that_ran_too_fast_to_time_shows_zero_seconds() {
        let mut list = ProgressList::default();
        list.apply(event("fetch", StepStatus::Running, 0, None));
        list.apply(event("fetch", StepStatus::Done, 0, None));

        assert_eq!(drawn(&list, 20, 1), "✓ fetch (0s)\n");
    }

    #[test]
    fn a_repeated_step_name_adds_a_row_once_the_first_has_finished() {
        let mut list = ProgressList::default();
        list.apply(event("page", StepStatus::Running, 0, None));
        list.apply(event("page", StepStatus::Done, 20, None));
        list.apply(event("page", StepStatus::Running, 0, None));

        assert_eq!(drawn(&list, 20, 3), "✓ page (20ms)\n→ page …\n\n");
    }

    #[test]
    fn more_steps_than_rows_shows_the_latest() {
        let mut list = ProgressList::default();
        for step in ["one", "two", "three"] {
            list.apply(event(step, StepStatus::Running, 0, None));
            list.apply(event(step, StepStatus::Done, 1000, None));
        }

        assert_eq!(drawn(&list, 20, 2), "✓ two (1s)\n✓ three (1s)\n");
    }

    #[test]
    fn an_empty_list_draws_nothing_and_says_it_is_empty() {
        let mut list = ProgressList::default();
        assert!(list.is_empty());
        assert_eq!(drawn(&list, 10, 1), "\n");

        list.apply(event("fetch", StepStatus::Running, 0, None));
        assert!(!list.is_empty());
    }

    #[test]
    fn a_zero_sized_area_draws_without_panicking() {
        let mut list = ProgressList::default();
        list.apply(event("fetch", StepStatus::Running, 0, None));

        render_with(4, 2, |frame| list.render(frame, Rect::new(0, 0, 0, 0)));
    }
}
