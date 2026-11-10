//! Core library for the `ss` support operations CLI: the semantic JSON
//! [`diff`] engine and the [`jsonutil`] helpers it is built on, the [`vars`]
//! environment snapshot, the [`config`] file with its precedence rules, the
//! environment [`guard`], the [`audit`] trail, the [`cache`] store, the named
//! [`preset`] parameter sets, the [`fuzzy`] subsequence matcher, the [`ops`]
//! layer's input and view types, the [`render`] text and JSON output for those
//! views, the markdown [`report`] export, and the [`svc`] service ports and
//! their in-memory fakes.

pub mod audit;
pub mod cache;
pub mod config;
pub mod diff;
pub mod fuzzy;
pub mod guard;
pub mod jsonutil;
pub mod ops;
pub mod preset;
pub mod render;
pub mod report;
pub mod svc;
pub mod vars;
