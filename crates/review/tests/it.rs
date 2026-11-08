//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

#[path = "../../hindsight/tests/common/mod.rs"]
mod common;
#[path = "../../cli/tests/executable_support/mod.rs"]
mod executable_support;
mod github_common;

#[path = "autoreview.rs"]
mod autoreview;
#[path = "ban.rs"]
mod ban;
#[path = "daemon.rs"]
mod daemon;
#[path = "engine.rs"]
mod engine;
#[path = "engine_env.rs"]
mod engine_env;
#[path = "eval.rs"]
mod eval;
#[path = "github_accounts.rs"]
mod github_accounts;
#[path = "github_fixtures.rs"]
mod github_fixtures;
#[path = "github_review.rs"]
mod github_review;
#[path = "ledger.rs"]
mod ledger;
#[path = "lessons.rs"]
mod lessons;
#[path = "prepare_env.rs"]
mod prepare_env;
#[path = "promotion.rs"]
mod promotion;
#[path = "repo_guide.rs"]
mod repo_guide;
#[path = "rereview.rs"]
mod rereview;
#[path = "retrying_websocket.rs"]
mod retrying_websocket;
#[path = "review_record.rs"]
mod review_record;
#[path = "rules.rs"]
mod rules;
#[path = "service.rs"]
mod service;
#[path = "settings_ban.rs"]
mod settings_ban;
#[path = "state_writes.rs"]
mod state_writes;
