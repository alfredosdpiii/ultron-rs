//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

#[path = "keep_going.rs"]
mod keep_going;
#[path = "persona.rs"]
mod persona;
#[path = "runtime_guide_contracts.rs"]
mod runtime_guide_contracts;
#[path = "skill_ignores.rs"]
mod skill_ignores;
#[path = "typescript_parity.rs"]
mod typescript_parity;
