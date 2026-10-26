//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

#[path = "agent_hub.rs"]
mod agent_hub;
#[path = "agent_hub_actions.rs"]
mod agent_hub_actions;
#[path = "app.rs"]
mod app;
#[path = "bug_report.rs"]
mod bug_report;
#[path = "changelog.rs"]
mod changelog;
#[path = "commands.rs"]
mod commands;
#[path = "conversation_parity.rs"]
mod conversation_parity;
#[path = "copy_targets.rs"]
mod copy_targets;
#[path = "diagram_display_images.rs"]
mod diagram_display_images;
#[path = "easter_eggs.rs"]
mod easter_eggs;
#[path = "extension_ui.rs"]
mod extension_ui;
#[path = "footer_data.rs"]
mod footer_data;
#[path = "fullscreen_viewport.rs"]
mod fullscreen_viewport;
#[path = "history_search.rs"]
mod history_search;
#[path = "idle_kernel_strip.rs"]
mod idle_kernel_strip;
#[path = "inspection.rs"]
mod inspection;
#[path = "keybindings_reload.rs"]
mod keybindings_reload;
#[path = "link_click.rs"]
mod link_click;
#[path = "mcp_dialog_theme.rs"]
mod mcp_dialog_theme;
#[path = "model_hub.rs"]
mod model_hub;
#[path = "mouse_surfaces.rs"]
mod mouse_surfaces;
#[path = "next_model.rs"]
mod next_model;
#[path = "p_tui.rs"]
mod p_tui;
#[path = "qol.rs"]
mod qol;
#[path = "qol_parity.rs"]
mod qol_parity;
#[path = "radius_recovery.rs"]
mod radius_recovery;
#[path = "render_settings.rs"]
mod render_settings;
#[path = "restore_files.rs"]
mod restore_files;
#[path = "retry.rs"]
mod retry;
#[path = "rlm_highlight.rs"]
mod rlm_highlight;
#[path = "rlm_views.rs"]
mod rlm_views;
#[path = "split_images.rs"]
mod split_images;
#[path = "startup_env_fallbacks.rs"]
mod startup_env_fallbacks;
#[path = "startup_notices.rs"]
mod startup_notices;
#[path = "startup_notices_app.rs"]
mod startup_notices_app;
#[path = "terminal_debug.rs"]
mod terminal_debug;
#[path = "terminal_input.rs"]
mod terminal_input;
#[path = "theme_follow.rs"]
mod theme_follow;
#[path = "theme_osc11.rs"]
mod theme_osc11;
#[path = "tool_images.rs"]
mod tool_images;
#[path = "transcript_picker.rs"]
mod transcript_picker;
#[path = "trust_debug.rs"]
mod trust_debug;

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
