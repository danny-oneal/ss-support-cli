//! Demo operations that exercise the whole pipeline (registry, parameters,
//! suggestions, steps, progress, views, every renderer) without any cloud
//! calls.

mod hello;
mod json_diff;

pub use hello::Hello;
pub use json_diff::{JsonDiff, json_files_in};
