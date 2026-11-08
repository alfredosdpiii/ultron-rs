//! Which Hindsight server and bank Ultron uses, and the identity tags of a session's memory.
//!
//! Every Ultron process on a machine resolves the same server and bank: `ULTRON_HINDSIGHT_URL`, else the global
//! `hindsightUrl` setting (project settings never redirect memory), else `http://localhost:8888`; "off", "none", "0"
//! or "false" (any case) turn memory off. The bank is `ULTRON_HINDSIGHT_BANK`, else "ultron". So the global
//! durable instance and every per-project instance share one memory: a `global` memory is tagged with the home
//! directory's identity and a `project` memory with the project directory's, never with the instance.

use crate::backend::{HindsightBackend, HindsightOptions};
use kernel_apis::MemoryError;
use serde_json::Value;
use std::path::Path;

pub const HINDSIGHT_BANK_ENV: &str = "ULTRON_HINDSIGHT_BANK";
pub const HINDSIGHT_URL_ENV: &str = "ULTRON_HINDSIGHT_URL";

/// `DEFAULT_HINDSIGHT_URL` (core/defaults.ts).
pub const DEFAULT_HINDSIGHT_URL: &str = "http://localhost:8888";
const DEFAULT_BANK: &str = "ultron";

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(jsrt::string::trim).filter(|v| !v.is_empty())
}

/// `hindsightUrl(configured, setting)` of session-worker.ts, also `hindsightUrlFrom(env, setting, DEFAULT)` of
/// autoreview/lessons.ts: the environment, else the setting, else the default; off: `None`
/// ([`config::settings::service_url`]).
pub fn hindsight_url(env: Option<&str>, setting: Option<&str>) -> Option<String> {
    config::settings::service_url(env, setting, DEFAULT_HINDSIGHT_URL)
}

/// Where [`resolve`] found the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Setting,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::Setting => "setting",
            Source::Default => "default",
        }
    }
}

/// `effectiveHindsightUrl` of cli/setup/core.ts: [`hindsight_url`] (`None`: off) and where it came from.
pub fn resolve(env: Option<&str>, setting: Option<&str>) -> (Option<String>, Source) {
    let source = if trimmed(env).is_some() {
        Source::Env
    } else if trimmed(setting).is_some() {
        Source::Setting
    } else {
        Source::Default
    };
    (hindsight_url(env, setting), source)
}

/// [`resolve`] of `ULTRON_HINDSIGHT_URL` in `var` and the global `hindsightUrl` setting of `manager`.
pub fn resolve_in(
    var: &dyn Fn(&str) -> Option<String>,
    manager: &config::settings::SettingsManager,
) -> (Option<String>, Source) {
    resolve(var(HINDSIGHT_URL_ENV).as_deref(), setting_url(manager).as_deref())
}

/// `SettingsManager.getHindsightUrl()`: the global `hindsightUrl` setting, trimmed, when a non-blank string.
pub fn setting_url(manager: &config::settings::SettingsManager) -> Option<String> {
    manager
        .global_settings()
        .get("hindsightUrl")
        .and_then(Value::as_str)
        .map(jsrt::string::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// `process.env.ULTRON_HINDSIGHT_BANK || "ultron"`.
pub fn bank(env: Option<&str>) -> String {
    env.filter(|v| !v.is_empty()).unwrap_or(DEFAULT_BANK).to_string()
}

pub use kernel_apis::memory::{directory_identity, worker_scope_tags};

/// The server and bank this process uses (`None` from [`Endpoint::at`]: memory is off).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub bank: String,
}

impl Endpoint {
    /// The server at `url` ([`resolve_in`]'s, `None`: memory is off) on the bank `ULTRON_HINDSIGHT_BANK` names.
    pub fn at(url: Option<String>, var: &dyn Fn(&str) -> Option<String>) -> Option<Endpoint> {
        Some(Endpoint {
            url: url?,
            bank: bank(var(HINDSIGHT_BANK_ENV).as_deref()),
        })
    }

    /// From `ULTRON_HINDSIGHT_URL`, the global `hindsightUrl` setting under `agent_dir`, and `ULTRON_HINDSIGHT_BANK`.
    pub fn from_env(var: &dyn Fn(&str) -> Option<String>, cwd: &Path, agent_dir: &Path) -> Option<Endpoint> {
        let manager = config::settings::SettingsManager::create(cwd, agent_dir, false);
        Self::at(resolve_in(var, &manager).0, var)
    }

    /// The session worker's backend (`createWorkerServices`): this bank, created on first use, tagged for the
    /// session, its project directory and the home directory.
    pub fn session_backend(&self, session_id: &str, cwd: &str, home: &str) -> Result<HindsightBackend, MemoryError> {
        let mut options = HindsightOptions::new(&self.url, &self.bank, worker_scope_tags(session_id, cwd, home));
        options.ensure_bank = true;
        HindsightBackend::new(options)
    }
}
