//! Which memory backend a process uses, and where memorantado is.
//!
//! `memory.backend` (a global setting: it points at a service) picks `memorantado` or `hindsight`. Unset, memorantado
//! is used once its server answers a health check, and that choice is kept in the agent directory
//! ([`config::paths::AgentPaths::memory_backend`]); until then, Hindsight. A Hindsight server set explicitly
//! (`ULTRON_HINDSIGHT_URL`, `hindsightUrl`) is kept while memorantado was never chosen: nothing is detected. Once memorantado is the backend (set or
//! detected), a server that does not answer pauses memory with a notice ([`Endpoint::paused_notice`]) and nothing
//! is written to Hindsight, so memories never split across two stores. The server is `ULTRON_MEMORANTADO_URL`, else the
//! global `memorantadoUrl` setting, else [`DEFAULT_MEMORANTADO_URL`]; "off", "none", "0" or "false" turn it off. The
//! project is `ULTRON_MEMORANTADO_PROJECT`, else "ultron". Every Ultron process on a machine shares the project;
//! the scope tags (kernel_apis `worker_scope_tags`) keep sessions, projects and the user's global memory apart.

use crate::backend::{MemorantadoBackend, MemorantadoOptions};
use blocking_http::{HttpRequest, Transport};
use kernel::Signal;
use kernel_apis::MemoryError;
use kernel_apis::memory::worker_scope_tags;
use serde_json::Value;
use std::path::Path;
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
    /// The backend `text` names ([`config::keys::memory_backend_name`]).
    pub fn parse(text: &str) -> Option<Choice> {
        match config::keys::memory_backend_name(text)? {
            "memorantado" => Some(Choice::Memorantado),
            _ => Some(Choice::Hindsight),
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

/// The global `memory.backend` setting, when it names a backend.
pub fn choice(manager: &config::settings::SettingsManager) -> Option<Choice> {
    config::keys::memory_backend(manager.global_settings()).and_then(Choice::parse)
}

/// The global `memorantadoUrl` setting, trimmed, when a non-blank string.
pub fn setting_url(manager: &config::settings::SettingsManager) -> Option<String> {
    trimmed(manager.global_settings().get(URL_SETTING).and_then(Value::as_str)).map(String::from)
}

/// The environment, else the setting, else the default, without a trailing `/`; off: `None`
/// ([`config::settings::service_url`]).
pub fn memorantado_url(env: Option<&str>, setting: Option<&str>) -> Option<String> {
    config::settings::service_url(env, setting, DEFAULT_MEMORANTADO_URL).map(|url| url.trim_end_matches('/').into())
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

/// [`answers`] over a fresh HTTP client (a client that cannot be built: no answer).
pub fn probe(url: &str) -> bool {
    blocking_http::ReqwestTransport::new().is_ok_and(|transport| answers(&transport, url))
}

/// The Hindsight server a process resolved (hindsight::resolve), as the backend choice needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hindsight {
    /// Turned off (`ULTRON_HINDSIGHT_URL=off`): memory is off unless memorantado is set.
    Off,
    /// The default URL only: a guess, so memorantado is detected first.
    Default,
    /// Set explicitly (`ULTRON_HINDSIGHT_URL` or `hindsightUrl`).
    Set,
}

/// The memory backend a process uses ([`Endpoint::choose`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    /// memorantado; `up`: it answered at start (else memory is paused until it does).
    Memorantado { endpoint: Endpoint, up: bool },
    /// Hindsight, when its server is configured.
    Hindsight,
    /// No memory.
    Off,
}

impl Backend {
    /// The memorantado endpoint, when memorantado is the backend.
    pub fn memorantado(&self) -> Option<&Endpoint> {
        match self {
            Backend::Memorantado { endpoint, .. } => Some(endpoint),
            _ => None,
        }
    }
}

/// The backend detected and kept in `agent_dir` while `memory.backend` is unset.
pub fn remembered(agent_dir: &Path) -> Option<Choice> {
    let path = config::paths::AgentPaths::new(agent_dir).memory_backend();
    std::fs::read_to_string(path).ok().as_deref().and_then(Choice::parse)
}

/// Keeps a detected backend; a profile whose directory cannot be written detects again next time.
fn remember(agent_dir: &Path, choice: Choice) {
    let path = config::paths::AgentPaths::new(agent_dir).memory_backend();
    let _ = std::fs::write(path, format!("{}\n", choice.as_str()));
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

    /// The memory backend this process uses. `memory.backend: memorantado`, or a detection kept in `agent_dir`
    /// (unless `hindsight` is off: memory turned off), is memorantado whether or not it answers (`up` says; an
    /// outage pauses memory), or no memory when memorantado is turned off; `hindsight` is Hindsight. Unset and
    /// never detected, a Hindsight server set explicitly is Hindsight; else memorantado is used when `answers`
    /// says its server is up, and the choice is kept; otherwise Hindsight. Offline (`PI_OFFLINE`), nothing is
    /// detected: memorantado is used only when chosen.
    pub fn choose(
        var: &dyn Fn(&str) -> Option<String>,
        manager: &config::settings::SettingsManager,
        agent_dir: &Path,
        hindsight: Hindsight,
        answers: &dyn Fn(&str) -> bool,
    ) -> Backend {
        let configured = Self::configured(var, manager);
        let chosen = |endpoint: Option<Endpoint>| match endpoint {
            Some(endpoint) => Backend::Memorantado {
                up: answers(&endpoint.url),
                endpoint,
            },
            None => Backend::Off,
        };
        match choice(manager) {
            Some(Choice::Memorantado) => chosen(configured),
            Some(Choice::Hindsight) => Backend::Hindsight,
            None if hindsight == Hindsight::Off => Backend::Off,
            None if remembered(agent_dir) == Some(Choice::Memorantado) => chosen(configured),
            None if hindsight == Hindsight::Set || config::env::offline(var) => Backend::Hindsight,
            None => match configured {
                Some(endpoint) if answers(&endpoint.url) => {
                    remember(agent_dir, Choice::Memorantado);
                    Backend::Memorantado { endpoint, up: true }
                }
                _ => Backend::Hindsight,
            },
        }
    }

    /// What a session says when memorantado is the backend but does not answer.
    pub fn paused_notice(&self) -> String {
        format!(
            "Memory is paused: memorantado does not answer at {}. Nothing is kept elsewhere; memory resumes once it answers (`ultron setup` chooses the memory backend).",
            self.url
        )
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
