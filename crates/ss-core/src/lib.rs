//! `ss` — support operations CLI (Rust port).
//!
//! This crate is being ported from the Go implementation. The spine is built
//! first: the semantic JSON [`diff`] engine, then the operations layer.

pub mod diff;
