//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "idle_pool.rs"]
mod idle_pool;
#[path = "idle_sweep.rs"]
mod idle_sweep;
#[path = "kernel_pool.rs"]
mod kernel_pool;
#[path = "kernel_pool_real.rs"]
mod kernel_pool_real;
#[path = "pool_memory.rs"]
mod pool_memory;
