//! Which memory backend a process uses, and where memorantado is.
//!
//! `memory.backend` (a global setting: it points at a service) picks `memorantado` or `hindsight`; unset, memorantado
//! is used when its server answers a health check, else Hindsight. The server is `ULTRON_MEMORANTADO_URL`, else the
//! global `memorantadoUrl` setting, else [`DEFAULT_MEMORANTADO_URL`]; "off", "none", "0" or "false" turn it off. The
//! project is `ULTRON_MEMORANTADO_PROJECT`, else "ultron". Every Ultron process on a machine shares the project;
//! the scope tags (kernel_apis `worker_scope_tags`) keep sessions, projects and the user's global memory apart.

use crate::backend::{MemorantadoBackend, MemorantadoOptions};
use blocking_http::{HttpRequest, Transport};
use kernel::Signal;
use kernel_apis::MemoryError;
use kernel_apis::memory::worker_scope_tags;
use serde_json::Value;
use std::time::Duration;

pub const MEMORANTADO_URL_ENV: &str = "ULTRON_MEMORANTADO_URL";
pub const MEMORANTADO_PROJECT_ENV: &str = "ULTRON_MEMORANTADO_PROJECT";
pub const DEFAULT_MEMORANTADO_URL: &str = "http://127.0.0.1:3789";
const DEFAULT_PROJECT: &str = "ultron";
/// The global setting naming the server.
pub const URL_SETTING: &str = "memorantadoUrl";
/// How long the health check waits: a local server answers in milliseconds.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// `memory.backend`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Memorantado,
    Hindsight,
}

impl Choice {
    pub fn parse(text: &str) -> Option<Choice> {
        match jsrt::string::trim(text).to_ascii_lowercase().as_str() {
            "memorantado" => Some(Choice::Memorantado),
            "hindsight" => Some(Choice::Hindsight),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Choice::Memorantado => "memorantado",
            Choice::Hindsight => "hindsight",
        }
    }
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(jsrt::string::trim).filter(|v| !v.is_empty())
}

fn off(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "off" | "none" | "0" | "false")
}

/// The global `memory.backend` setting, when it names a backend.
pub fn choice(manager: &config::settings::SettingsManager) -> Option<Choice> {
    manager
        .global_settings()
        .get("memory")
        .and_then(|memory| memory.get("backend"))
        .and_then(Value::as_str)
        .and_then(Choice::parse)
}

/// The global `memorantadoUrl` setting, trimmed, when a non-blank string.
pub fn setting_url(manager: &config::settings::SettingsManager) -> Option<String> {
    trimmed(manager.global_settings().get(URL_SETTING).and_then(Value::as_str)).map(String::from)
}

/// The environment, else the setting, else the default; off: `None`.
pub fn memorantado_url(env: Option<&str>, setting: Option<&str>) -> Option<String> {
    match trimmed(env).or_else(|| trimmed(setting)) {
        None => Some(DEFAULT_MEMORANTADO_URL.into()),
        Some(value) if off(value) => None,
        Some(value) => Some(value.trim_end_matches('/').into()),
    }
}

/// Whether a memorantado server answers at `url` (`GET /api/health` is `{status: "ok"}`), within [`PROBE_TIMEOUT`].
pub fn answers(transport: &dyn Transport, url: &str) -> bool {
    let request = HttpRequest {
        method: "GET".into(),
        url: format!("{}/api/health", url.trim_end_matches('/')),
        headers: Vec::new(),
        body: None,
    };
    transport
        .send(request, 4096, PROBE_TIMEOUT, &Signal::new())
        .ok()
        .filter(|response| (200..300).contains(&response.status))
        .and_then(|response| serde_json::from_slice::<Value>(&response.body).ok())
        .is_some_and(|health| health["status"] == "ok")
}

/// The memorantado server and project a process uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub project: String,
}

impl Endpoint {
    /// The server [`memorantado_url`] resolves from `var` and the settings (`None`: off), on the project
    /// `ULTRON_MEMORANTADO_PROJECT` names.
    pub fn configured(
        var: &dyn Fn(&str) -> Option<String>,
        manager: &config::settings::SettingsManager,
    ) -> Option<Endpoint> {
        let url = memorantado_url(var(MEMORANTADO_URL_ENV).as_deref(), setting_url(manager).as_deref())?;
        let project = trimmed(var(MEMORANTADO_PROJECT_ENV).as_deref())
            .unwrap_or(DEFAULT_PROJECT)
            .to_string();
        Some(Endpoint { url, project })
    }

    /// The memorantado endpoint this process uses, `None` for Hindsight (or no memory): `memory.backend:
    /// memorantado` uses it unchecked (an outage pauses the client), `hindsight` never; unset, it is used when
    /// `answers` says its server is up, unless `memory_off` (memory turned off, as `ULTRON_HINDSIGHT_URL=off` does).
    pub fn choose(
        var: &dyn Fn(&str) -> Option<String>,
        manager: &config::settings::SettingsManager,
        memory_off: bool,
        answers: &dyn Fn(&str) -> bool,
    ) -> Option<Endpoint> {
        let endpoint = Self::configured(var, manager)?;
        match choice(manager) {
            Some(Choice::Memorantado) => Some(endpoint),
            Some(Choice::Hindsight) => None,
            None if memory_off => None,
            None => answers(&endpoint.url).then_some(endpoint),
        }
    }

    /// A backend with `scope_tags`.
    pub fn backend(&self, scope_tags: serde_json::Map<String, Value>) -> Result<MemorantadoBackend, MemoryError> {
        MemorantadoBackend::new(MemorantadoOptions::new(&self.url, &self.project, scope_tags))
    }

    /// The session worker's backend, tagged for the session, its project directory and the home directory.
    pub fn session_backend(&self, session_id: &str, cwd: &str, home: &str) -> Result<MemorantadoBackend, MemoryError> {
        self.backend(worker_scope_tags(session_id, cwd, home))
    }
}
