//! Claude Code as the root agent of Ultron (packages/coding-agent/src/ultron/claude): `ultron claude` (the
//! launcher), `ultron mcp` (Ultron's REPL as the one MCP server Claude Code talks to, exposing exactly the `rlm`
//! tool), `ultron hook` (Claude Code hook commands) and `ultron guide`, plus the `--claude` root-model selection of
//! the session worker. `ultron watch`, the terminal viewer of a running `ultron mcp`, is crates/cli's (`cli::watch`):
//! it is a client of this crate's control socket ([`control`]) drawn with the TUI, which this crate does not need.
//!
//! RLM-first: Claude Code sees one tool, `mcp__ultron__rlm`; every capability is a kernel API reached from a cell.

mod checkpoints;
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
use std::path::{Path, PathBuf};

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

/// What a command sees: the process environment and locations, and what the CLI (which owns the session report)
/// provides.
pub struct Ctx {
    pub env: RunEnv,
    /// Answers `ultron mcp`'s `usage.report` inspections; `None`: the report is not available.
    pub session_report: Option<mcp_server::SessionReport>,
}

impl Ctx {
    /// The bundled skills directory (`<package>/skills`), unless `ULTRON_BUNDLED_SKILLS` turns it off.
    pub fn bundled_skills(&self) -> Option<PathBuf> {
        guide::bundled_skills_enabled((self.env.var)(session_core::session_resources::BUNDLED_SKILLS_ENV).as_deref())
            .then(|| config::paths::normalize(&self.env.docs.join("../skills")))
    }
}

/// `os.tmpdir()`: `TMPDIR` without a trailing slash, else `/tmp`.
pub fn tmpdir(var: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match var("TMPDIR").filter(|v| !v.is_empty()) {
        Some(dir) if dir.len() > 1 => PathBuf::from(dir.trim_end_matches('/')),
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/tmp"),
    }
}

/// `mkdtempSync(join(tmpdir(), prefix))`: a new private directory.
pub fn temp_dir(prefix: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let base = tmpdir(var);
    for _ in 0..16 {
        let path = base.join(format!("{prefix}{}", jsrt::random::random_hex(3)));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Err(format!("could not create a temp directory in {}", base.display()))
}

/// Write `content` to a new owner-only file.
pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())
}
