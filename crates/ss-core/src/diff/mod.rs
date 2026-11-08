//! Semantic JSON diff engine.
//!
//! Compares two parsed JSON trees and produces a [`Delta`] of typed
//! [`Change`]s: objects by key-set union, arrays by identity key or longest
//! common subsequence, scalars by typed compare.

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
