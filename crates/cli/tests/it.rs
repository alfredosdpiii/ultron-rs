//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

mod common;
mod daemon_common;
mod executable_support;
#[path = "../../mcp/tests/support/fake_proxy.rs"]
mod fake_proxy;
#[path = "../../review/tests/github_common/mod.rs"]
mod github_common;
#[path = "../../hindsight/tests/common/mod.rs"]
mod hindsight_common;
#[path = "../../ai/tests/claude_subscription_support/isolate.rs"]
mod isolate;
#[path = "../../memorantado/tests/support/fake.rs"]
mod memorantado_fake;
mod radius_bridge;
mod recorded_version;
#[path = "../../kernel/tests/traceback_support/mod.rs"]
mod traceback_support;
mod tui_harness;

#[path = "acp.rs"]
mod acp;
#[path = "acp_units.rs"]
mod acp_units;
#[path = "autoreview.rs"]
mod autoreview;
#[path = "autoreview_ban.rs"]
mod autoreview_ban;
#[path = "autoreview_claude_code.rs"]
mod autoreview_claude_code;
#[path = "autoreview_doctor.rs"]
mod autoreview_doctor;
#[path = "autoreview_github.rs"]
mod autoreview_github;
#[path = "autoreview_prepare.rs"]
mod autoreview_prepare;
#[path = "autoreview_toggle.rs"]
mod autoreview_toggle;
#[path = "bug_report.rs"]
mod bug_report;
#[path = "bug_report_provider.rs"]
mod bug_report_provider;
#[path = "checkpoint_tree.rs"]
mod checkpoint_tree;
#[path = "claude_code_auth.rs"]
mod claude_code_auth;
#[path = "claude_code_provider.rs"]
mod claude_code_provider;
#[path = "claude_root.rs"]
mod claude_root;
#[path = "claude_root_children.rs"]
mod claude_root_children;
#[path = "claude_root_flag.rs"]
mod claude_root_flag;
#[path = "claude_root_relay.rs"]
mod claude_root_relay;
#[path = "claude_root_resources.rs"]
mod claude_root_resources;
#[path = "claude_root_resume.rs"]
mod claude_root_resume;
#[path = "claude_root_stats.rs"]
mod claude_root_stats;
#[path = "claude_root_subscription_children.rs"]
mod claude_root_subscription_children;
#[path = "claude_subscription.rs"]
mod claude_subscription;
#[path = "clear_command_tui.rs"]
mod clear_command_tui;
#[path = "cli.rs"]
mod cli;
#[path = "cli_proxy.rs"]
mod cli_proxy;
#[path = "collect_waits.rs"]
mod collect_waits;
#[path = "composition.rs"]
mod composition;
#[path = "config_tui.rs"]
mod config_tui;
#[path = "custom_providers.rs"]
mod custom_providers;
#[path = "daemon_coordinator.rs"]
mod daemon_coordinator;
#[path = "daemon_durability.rs"]
mod daemon_durability;
#[path = "daemon_durable_client.rs"]
mod daemon_durable_client;
#[path = "daemon_durable_home.rs"]
mod daemon_durable_home;
#[path = "daemon_durable_model.rs"]
mod daemon_durable_model;
#[path = "daemon_durable_selectors.rs"]
mod daemon_durable_selectors;
#[path = "daemon_durable_session_options.rs"]
mod daemon_durable_session_options;
#[path = "daemon_entry_parity.rs"]
mod daemon_entry_parity;
#[path = "daemon_instances.rs"]
mod daemon_instances;
#[path = "daemon_mcp.rs"]
mod daemon_mcp;
#[path = "daemon_next_run.rs"]
mod daemon_next_run;
#[path = "daemon_queues.rs"]
mod daemon_queues;
#[path = "daemon_review_command.rs"]
mod daemon_review_command;
#[path = "daemon_schedules.rs"]
mod daemon_schedules;
#[path = "daemon_session_control.rs"]
mod daemon_session_control;
#[path = "daemon_subagents.rs"]
mod daemon_subagents;
#[path = "daemon_traffic.rs"]
mod daemon_traffic;
#[path = "entry_parity_tui.rs"]
mod entry_parity_tui;
#[path = "exec_race.rs"]
mod exec_race;
#[path = "experimental_cli.rs"]
mod experimental_cli;
#[path = "export_theme.rs"]
mod export_theme;
#[path = "first_frame.rs"]
mod first_frame;
#[path = "frame_models_e2e.rs"]
mod frame_models_e2e;
#[path = "herdr.rs"]
mod herdr;
#[path = "interactive.rs"]
mod interactive;
#[path = "interactive_abort.rs"]
mod interactive_abort;
#[path = "interactive_agent_hub.rs"]
mod interactive_agent_hub;
#[path = "interactive_bug_report.rs"]
mod interactive_bug_report;
#[path = "interactive_cmds.rs"]
mod interactive_cmds;
#[path = "interactive_fullscreen_scroll.rs"]
mod interactive_fullscreen_scroll;
#[path = "interactive_import.rs"]
mod interactive_import;
#[path = "interactive_mcp.rs"]
mod interactive_mcp;
#[path = "interactive_observer.rs"]
mod interactive_observer;
#[path = "interactive_overlays.rs"]
mod interactive_overlays;
#[path = "interactive_trust_debug.rs"]
mod interactive_trust_debug;
#[path = "interactive_user_bash.rs"]
mod interactive_user_bash;
#[path = "line_input.rs"]
mod line_input;
#[path = "local_resources_units.rs"]
mod local_resources_units;
#[path = "local_themes.rs"]
mod local_themes;
#[path = "login_options.rs"]
mod login_options;
#[path = "mcp_login.rs"]
mod mcp_login;
#[path = "mcp_proxy.rs"]
mod mcp_proxy;
#[path = "memory_import.rs"]
mod memory_import;
#[path = "memory_migrate.rs"]
mod memory_migrate;
#[path = "models_flag.rs"]
mod models_flag;
#[path = "native_flags.rs"]
mod native_flags;
#[path = "native_run.rs"]
mod native_run;
#[path = "oauth_login_tui.rs"]
mod oauth_login_tui;
#[path = "optimize.rs"]
mod optimize;
#[path = "package_assets.rs"]
mod package_assets;
#[path = "package_dir.rs"]
mod package_dir;
#[path = "presentation_plugins.rs"]
mod presentation_plugins;
#[path = "print_mode.rs"]
mod print_mode;
#[path = "print_notices.rs"]
mod print_notices;
#[path = "provider_auth.rs"]
mod provider_auth;
#[path = "provider_wire_auth.rs"]
mod provider_wire_auth;
#[path = "radius_auth.rs"]
mod radius_auth;
#[path = "radius_client.rs"]
mod radius_client;
#[path = "radius_login.rs"]
mod radius_login;
#[path = "radius_server.rs"]
mod radius_server;
#[path = "radius_units.rs"]
mod radius_units;
#[path = "resume_picker.rs"]
mod resume_picker;
#[path = "resume_unavailable_model.rs"]
mod resume_unavailable_model;
#[path = "rpc_compaction.rs"]
mod rpc_compaction;
#[path = "rpc_mode.rs"]
mod rpc_mode;
#[path = "rpc_pi_events.rs"]
mod rpc_pi_events;
#[path = "rpc_startup_trace.rs"]
mod rpc_startup_trace;
#[path = "rpc_units.rs"]
mod rpc_units;
#[path = "scoped_models.rs"]
mod scoped_models;
#[path = "scoped_models_refresh.rs"]
mod scoped_models_refresh;
#[path = "service_cli.rs"]
mod service_cli;
#[path = "session_models.rs"]
mod session_models;
#[path = "sessions.rs"]
mod sessions;
#[path = "setup_tui.rs"]
mod setup_tui;
#[path = "setup_wizard.rs"]
mod setup_wizard;
#[path = "skills_cli.rs"]
mod skills_cli;
#[path = "startup_checks.rs"]
mod startup_checks;
#[path = "startup_trace.rs"]
mod startup_trace;
#[path = "subcommand_units.rs"]
mod subcommand_units;
#[path = "subcommands.rs"]
mod subcommands;
#[path = "terminal_settings.rs"]
mod terminal_settings;
#[path = "tool_env_warnings.rs"]
mod tool_env_warnings;
#[path = "tools_manager.rs"]
mod tools_manager;
#[path = "trace_check.rs"]
mod trace_check;
#[path = "tui_login.rs"]
mod tui_login;
#[path = "tui_login_providers.rs"]
mod tui_login_providers;
#[path = "update_models.rs"]
mod update_models;
#[path = "usage_subscription_providers.rs"]
mod usage_subscription_providers;
#[path = "user_keybindings.rs"]
mod user_keybindings;
#[path = "version_check.rs"]
mod version_check;
#[path = "watch.rs"]
mod watch;
