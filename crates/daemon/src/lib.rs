//! Ultron's local session daemon (packages/coding-agent/src/experimental: coordinator.ts, server.ts,
//! session-worker-manager.ts, session-worker.ts, process.ts) on the protocol of crates/protocol.

pub mod catalog;
pub mod chord;
pub mod control;
mod coord_client;
pub mod coordinator;
pub mod dream;
pub mod extension_ui;
pub mod instance;
pub mod lane;
pub mod lifecycle;
pub mod owner;
pub mod paths;
pub mod process;
pub mod prompt_schedules;
mod routed;
pub mod server;
pub mod session_commands;
pub mod session_control;
mod sockets;
pub mod watchdog;
pub mod worker;
pub mod worker_settings;
pub mod workers;
