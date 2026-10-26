//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "a01_invocation_parity.rs"]
mod a01_invocation_parity;
#[path = "a02_large_data.rs"]
mod a02_large_data;
#[path = "a03_bounded_repair.rs"]
mod a03_bounded_repair;
#[path = "a04_model_pinning.rs"]
mod a04_model_pinning;
#[path = "a05_cancellation.rs"]
mod a05_cancellation;
#[path = "a05_native_host.rs"]
mod a05_native_host;
#[path = "a06_agent_limits.rs"]
mod a06_agent_limits;
#[path = "a06_limits.rs"]
mod a06_limits;
#[path = "a06_task_timeout.rs"]
mod a06_task_timeout;
#[path = "a07_branch_fork_resume.rs"]
mod a07_branch_fork_resume;
#[path = "a08_snapshot_restorability.rs"]
mod a08_snapshot_restorability;
#[path = "a09_budget_admission.rs"]
mod a09_budget_admission;
#[path = "a10_worker_isolation.rs"]
mod a10_worker_isolation;
#[path = "a12_graph_contract.rs"]
mod a12_graph_contract;
#[path = "a13_admission_not_dependency.rs"]
mod a13_admission_not_dependency;
#[path = "a14_no_false_verification.rs"]
mod a14_no_false_verification;
#[path = "a15_release_gate.rs"]
mod a15_release_gate;
#[path = "a16_memory_skip_scope.rs"]
mod a16_memory_skip_scope;
#[path = "a17_correction_supersedes.rs"]
mod a17_correction_supersedes;
#[path = "a18_retention_forget.rs"]
mod a18_retention_forget;
#[path = "a19_refiner_protected.rs"]
mod a19_refiner_protected;
#[path = "a20_refinement_behavior.rs"]
mod a20_refinement_behavior;
#[path = "a21_lost_response_replay.rs"]
mod a21_lost_response_replay;
#[path = "a22_detached_work.rs"]
mod a22_detached_work;
#[path = "a23_goal_command.rs"]
mod a23_goal_command;
#[path = "a23_schedules.rs"]
mod a23_schedules;
#[path = "a24_accounting_reconciliation.rs"]
mod a24_accounting_reconciliation;
#[path = "a25_migration.rs"]
mod a25_migration;
#[path = "a27_self_contained_rlm.rs"]
mod a27_self_contained_rlm;
#[path = "a28_retrieval_decision.rs"]
mod a28_retrieval_decision;
#[path = "a29_memory_inspection.rs"]
mod a29_memory_inspection;
#[path = "a30_forget.rs"]
mod a30_forget;
#[path = "a31_skills.rs"]
mod a31_skills;
#[path = "a32_skill_proposals.rs"]
mod a32_skill_proposals;
#[path = "a33_experiment_history.rs"]
mod a33_experiment_history;
#[path = "a34_optional_service_outage.rs"]
mod a34_optional_service_outage;
#[path = "a35_strategy_parity.rs"]
mod a35_strategy_parity;
#[path = "a36_instances.rs"]
mod a36_instances;
#[path = "a37_artifact_range.rs"]
mod a37_artifact_range;
#[path = "a37_bounded_preview.rs"]
mod a37_bounded_preview;
#[path = "a38_nested_context.rs"]
mod a38_nested_context;
#[path = "a39_live_capabilities.rs"]
mod a39_live_capabilities;
#[path = "a41_family.rs"]
mod a41_family;
#[path = "a42_intervention.rs"]
mod a42_intervention;
#[path = "a43_artifacts.rs"]
mod a43_artifacts;
#[path = "a43_resource_release.rs"]
mod a43_resource_release;
#[path = "a44_progress.rs"]
mod a44_progress;
#[path = "a45_saturated_recursion.rs"]
mod a45_saturated_recursion;
#[path = "a46_demo.rs"]
mod a46_demo;
#[path = "a46_live_demo.rs"]
mod a46_live_demo;
#[path = "a47_a50_bounded_inference.rs"]
mod a47_a50_bounded_inference;
#[path = "a47_a50_frame_tasks.rs"]
mod a47_a50_frame_tasks;
#[path = "a51_collapse_on_return.rs"]
mod a51_collapse_on_return;
#[path = "a52_context_guards.rs"]
mod a52_context_guards;
#[path = "a52_ctx_child_lanes.rs"]
mod a52_ctx_child_lanes;
#[path = "a53_code_skills.rs"]
mod a53_code_skills;
#[path = "a54_skill_reuse.rs"]
mod a54_skill_reuse;
#[path = "a55_agent_classes.rs"]
mod a55_agent_classes;
#[path = "a55_agent_classes_registry.rs"]
mod a55_agent_classes_registry;
#[path = "a55_agent_classes_rpc.rs"]
mod a55_agent_classes_rpc;
#[path = "a56_async_cli.rs"]
mod a56_async_cli;
#[path = "a56_async_events.rs"]
mod a56_async_events;
#[path = "a56_collapse_cli.rs"]
mod a56_collapse_cli;
#[path = "a56_hints.rs"]
mod a56_hints;
#[path = "a56_shell_job_ends.rs"]
mod a56_shell_job_ends;
#[path = "a56_shell_jobs.rs"]
mod a56_shell_jobs;
#[path = "after_cell_hooks.rs"]
mod after_cell_hooks;
#[path = "spawn_fork.rs"]
mod spawn_fork;
#[path = "w22_persona_rpc.rs"]
mod w22_persona_rpc;

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
