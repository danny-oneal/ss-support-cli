//! Semantic JSON diff engine.
//!
//! Compares two parsed JSON trees and produces a [`Delta`] of typed
//! [`Change`]s. Ported from the Go `internal/diff` package: functional
//! parity, with the Go golden fixtures shared directly.

pub mod glob;
pub mod json;
pub mod lcs;
pub mod opts;
pub mod path;
pub mod stats;
pub mod text;

pub use glob::{GlobError, PathGlob, compile_globs};
pub use json::{Change, Delta, Kind, json};
pub use opts::Opts;
pub use path::{Path, Segment};
pub use stats::Stats;
pub use text::text;
