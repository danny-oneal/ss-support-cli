//! Core library for the `ss` support operations CLI: the semantic JSON
//! [`diff`] engine and the [`jsonutil`] helpers it is built on, the [`vars`]
//! environment snapshot, the [`config`] file with its precedence rules, the
//! environment [`guard`], the [`audit`] trail, and the [`cache`] store.

pub mod audit;
pub mod cache;
pub mod config;
pub mod diff;
pub mod guard;
pub mod jsonutil;
pub mod vars;
