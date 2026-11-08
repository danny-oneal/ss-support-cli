//! Core library for the `ss` support operations CLI: the semantic JSON
//! [`diff`] engine and the [`jsonutil`] helpers it is built on, the [`vars`]
//! environment snapshot, the [`config`] file with its precedence rules, the
//! environment [`guard`], the [`audit`] trail, the [`cache`] store, and the
//! named [`preset`] parameter sets, the [`fuzzy`] subsequence matcher, and the
//! [`ops`] layer's input and view types.

pub mod audit;
pub mod cache;
pub mod config;
pub mod diff;
pub mod fuzzy;
pub mod guard;
pub mod jsonutil;
pub mod ops;
pub mod preset;
pub mod vars;
