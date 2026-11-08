//! Claude Code as the root agent of Ultron (packages/coding-agent/src/ultron/claude): `ultron claude` (the
//! launcher), `ultron mcp` (Ultron's REPL as the one MCP server Claude Code talks to, exposing exactly the `rlm`
//! tool), `ultron hook` (Claude Code hook commands) and `ultron guide`, plus the `--claude` root-model selection of
//! the session worker. `ultron watch`, the terminal viewer of a running `ultron mcp`, is crates/cli's (`cli::watch`):
//! it is a client of this crate's control socket ([`control`]) drawn with the TUI, which this crate does not need.
//!
//! RLM-first: Claude Code sees one tool, `mcp__ultron__rlm`; every capability is a kernel API reached from a cell.

pub mod child;
mod child_usage;
pub mod claude_cli;
pub mod cli;
pub mod control;
pub mod durable_apis;
mod external_lane;
pub mod external_root;
pub mod guide;
pub mod herdr_hook;
pub mod hooks;
pub mod launcher;
pub mod mcp_protocol;
pub mod mcp_server;
pub mod persona_hook;
pub mod root_sink;
mod schedules;
pub mod self_cmd;
pub mod settings;
mod shared_world;
mod typed_agents;
pub mod unavailable;
pub mod worker_root;

use session_core::RunEnv;
use std::path::PathBuf;

/// `<model>` of a `claude-code/<model>` reference.
pub(crate) fn claude_code_id(model: &str) -> Option<&str> {
    model.strip_prefix(ai::claude_code::PROVIDER)?.strip_prefix('/')
}

/// `claude-code/<id>`.
pub(crate) fn claude_code_ref(id: &str) -> String {
    format!("{}/{id}", ai::claude_code::PROVIDER)
}

/// A usage error: printed without a stack, exit code 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

/// What a command sees: the process environment and locations.
pub struct Ctx {
    pub env: RunEnv,
}

impl Ctx {
    /// The bundled skills directory (`<package>/skills`), unless `ULTRON_BUNDLED_SKILLS` turns it off.
    pub fn bundled_skills(&self) -> Option<PathBuf> {
        session_core::session_resources::bundled_skills(&self.env.skills(), &*self.env.var).map(PathBuf::from)
    }
}
