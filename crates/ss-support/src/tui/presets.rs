//! Saving a run's parameters as a named preset, reported in the words the
//! status line shows.

use ss_core::ops::Input;

use super::screen::Ctx;

/// Saves the non-empty values of `input` as preset `name` of operation
/// `op_id` and returns what to tell the operator.
pub fn save(ctx: &Ctx, op_id: &str, name: &str, input: &Input) -> String {
    match ctx.presets.save(op_id, name, input.as_map()) {
        Ok(()) => format!("saved preset {name:?} for {op_id}"),
        Err(e) => format!("preset not saved: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use ss_core::preset;

    use super::*;
    use crate::tui::testing::ctx;

    #[test]
    fn a_saved_preset_can_be_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx();
        ctx.presets = preset::Store::new(dir.path().join("presets.yaml"));
        let mut input = Input::default();
        input.insert("bucket", "config-dev");
        input.insert("key", "");

        let message = save(&ctx, "s3.version-diff", "nightly", &input);

        assert_eq!(message, "saved preset \"nightly\" for s3.version-diff");
        let saved = ctx
            .presets
            .get("s3.version-diff", "nightly")
            .unwrap()
            .unwrap();
        assert_eq!(saved.get("bucket").map(String::as_str), Some("config-dev"));
        assert!(!saved.contains_key("key"), "empty values are not saved");
    }

    #[test]
    fn nothing_to_save_is_reported_and_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx();
        ctx.presets = preset::Store::new(dir.path().join("presets.yaml"));

        let message = save(&ctx, "s3.version-diff", "nightly", &Input::default());

        assert!(message.starts_with("preset not saved: "), "{message}");
        assert!(!dir.path().join("presets.yaml").exists());
    }

    #[test]
    fn an_unwritable_presets_file_is_reported() {
        let message = {
            let mut input = Input::default();
            input.insert("bucket", "config-dev");
            save(&ctx(), "s3.version-diff", "nightly", &input)
        };

        assert!(message.starts_with("preset not saved: "), "{message}");
    }
}
