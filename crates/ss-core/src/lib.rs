//! Core library for the `ss` support operations CLI: the semantic JSON
//! [`diff`] engine and the [`jsonutil`] helpers it is built on, the [`vars`]
//! environment snapshot, and the [`config`] file with its precedence rules.

pub mod config;
pub mod diff;
pub mod jsonutil;
pub mod vars;
