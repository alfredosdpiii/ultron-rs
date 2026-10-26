//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "agent_hub_ops.rs"]
mod agent_hub_ops;
#[path = "agent_ops.rs"]
mod agent_ops;
#[path = "agents_children.rs"]
mod agents_children;
#[path = "async_events.rs"]
mod async_events;
#[path = "auto_compaction.rs"]
mod auto_compaction;
#[path = "budget_gate.rs"]
mod budget_gate;
#[path = "budget_line.rs"]
mod budget_line;
#[path = "cell_file_changes.rs"]
mod cell_file_changes;
#[path = "checkpoints.rs"]
mod checkpoints;
#[path = "claude_fallback.rs"]
mod claude_fallback;
#[path = "clock_lanes.rs"]
mod clock_lanes;
#[path = "code_skill_env.rs"]
mod code_skill_env;
#[path = "codex_sockets.rs"]
mod codex_sockets;
#[path = "compaction_cancel.rs"]
mod compaction_cancel;
#[path = "context_images.rs"]
mod context_images;
#[path = "context_reminder.rs"]
mod context_reminder;
#[path = "contract_restart.rs"]
mod contract_restart;
#[path = "copilot_models.rs"]
mod copilot_models;
#[path = "custom_models.rs"]
mod custom_models;
#[path = "diagram_api.rs"]
mod diagram_api;
#[path = "durable_journal_conformance.rs"]
mod durable_journal_conformance;
#[path = "engineering_mode.rs"]
mod engineering_mode;
#[path = "event_masking.rs"]
mod event_masking;
#[path = "format_version.rs"]
mod format_version;
#[path = "frame_models.rs"]
mod frame_models;
#[path = "frame_traces.rs"]
mod frame_traces;
#[path = "goal_lanes.rs"]
mod goal_lanes;
#[path = "guard_stats_pool.rs"]
mod guard_stats_pool;
#[path = "header_env.rs"]
mod header_env;
#[path = "idle_kernel_count.rs"]
mod idle_kernel_count;
#[path = "idle_queue.rs"]
mod idle_queue;
#[path = "images_api.rs"]
mod images_api;
#[path = "initial_model.rs"]
mod initial_model;
#[path = "keep_going.rs"]
mod keep_going;
#[path = "kept_contracts.rs"]
mod kept_contracts;
#[path = "kernel_pool_wired.rs"]
mod kernel_pool_wired;
#[path = "lane_run_hints.rs"]
mod lane_run_hints;
#[path = "loki_log.rs"]
mod loki_log;
#[path = "loki_log_lines.rs"]
mod loki_log_lines;
#[path = "loki_notice.rs"]
mod loki_notice;
#[path = "loki_project_python_session.rs"]
mod loki_project_python_session;
#[path = "mcp_gateway.rs"]
mod mcp_gateway;
#[path = "mcp_proxy.rs"]
mod mcp_proxy;
#[path = "memory_pointers.rs"]
mod memory_pointers;
#[path = "model_auth.rs"]
mod model_auth;
#[path = "model_refresh.rs"]
mod model_refresh;
#[path = "module_values.rs"]
mod module_values;
#[path = "navigation_journal.rs"]
mod navigation_journal;
#[path = "next_run.rs"]
mod next_run;
#[path = "nudge_and_image_budget.rs"]
mod nudge_and_image_budget;
#[path = "persona.rs"]
mod persona;
#[path = "prompt_budget.rs"]
mod prompt_budget;
#[path = "prompt_schedules.rs"]
mod prompt_schedules;
#[path = "provider_headers.rs"]
mod provider_headers;
#[path = "provider_routes.rs"]
mod provider_routes;
#[path = "recovery.rs"]
mod recovery;
#[path = "request_auth_env.rs"]
mod request_auth_env;
#[path = "request_key.rs"]
mod request_key;
#[path = "request_keys.rs"]
mod request_keys;
#[path = "request_settings.rs"]
mod request_settings;
#[path = "request_stream_options.rs"]
mod request_stream_options;
#[path = "request_trace.rs"]
mod request_trace;
#[path = "retry_reopen.rs"]
mod retry_reopen;
#[path = "review_command.rs"]
mod review_command;
#[path = "session_env_cancel.rs"]
mod session_env_cancel;
#[path = "session_features.rs"]
mod session_features;
#[path = "session_resources.rs"]
mod session_resources;
#[path = "session_stats.rs"]
mod session_stats;
#[path = "session_stats_run.rs"]
mod session_stats_run;
#[path = "shared_memory.rs"]
mod shared_memory;
#[path = "snapshot_key_store.rs"]
mod snapshot_key_store;
#[path = "spawn_contracts.rs"]
mod spawn_contracts;
#[path = "stats_wiring.rs"]
mod stats_wiring;
#[path = "subscription_models.rs"]
mod subscription_models;
#[path = "swarm_ops.rs"]
mod swarm_ops;
#[path = "tool_arguments.rs"]
mod tool_arguments;
#[path = "view_image_tool.rs"]
mod view_image_tool;
#[path = "view_images_trust.rs"]
mod view_images_trust;
#[path = "worker_settings_live.rs"]
mod worker_settings_live;
#[path = "worktree_shell.rs"]
mod worktree_shell;

#[test]
fn every_test_file_is_built() {
    let (merged, manifest) = (include_str!("it.rs"), include_str!("../Cargo.toml"));
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(tests).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".rs") && name != "it.rs" {
            let listed = merged.contains(&format!("#[path = \"{name}\"]"))
                || manifest.contains(&format!("path = \"tests/{name}\""));
            assert!(
                listed,
                "tests/{name} is neither a module of tests/it.rs nor a [[test]] of Cargo.toml"
            );
        }
    }
}
