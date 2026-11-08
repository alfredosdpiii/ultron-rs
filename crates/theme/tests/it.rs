//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. `crates/workspace-checks` keeps the list whole.

#[path = "theme_parity.rs"]
mod theme_parity;
#[path = "theme_registry_parity.rs"]
mod theme_registry_parity;
#[path = "theme_styles_parity.rs"]
mod theme_styles_parity;
