use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
/// What each megabyte of the session file adds to [`WORKER_STARTUP_TIMEOUT`]: opening replays the whole journal (a
/// 98 MB one took about 15 s on an idle disk, and every restart timed out on a busy one).
const WORKER_STARTUP_PER_MB: Duration = Duration::from_millis(250);

/// How long a worker for the session at `path` may take to start.
pub(super) fn startup_timeout(path: &std::path::Path) -> Duration {
    let megabytes = std::fs::metadata(path).map_or(0, |m| m.len()) / 1_000_000;
    let extra = WORKER_STARTUP_PER_MB.saturating_mul(u32::try_from(megabytes).unwrap_or(u32::MAX));
    WORKER_STARTUP_TIMEOUT.saturating_add(extra)
}

/// The error code and message of a call its client cancelled (packages/server server.ts `handleRequest`).
pub const CANCELLED_CODE: &str = "cancelled";
pub const CANCELLED_MESSAGE: &str = "RPC request cancelled";

/// The server's heartbeat to its workers (`ULTRON_WORKER_HEARTBEAT_MS`); a worker silent for
/// `HEARTBEAT_MISSES` periods is hung and is killed (and restarted when it had work).
pub const HEARTBEAT_ENV: &str = "ULTRON_WORKER_HEARTBEAT_MS";
const DEFAULT_HEARTBEAT: Duration = Duration::from_millis(5_000);
/// Restarts of a crashed worker back off from [`RESTART_BASE`] doubling to [`RESTART_MAX_DELAY`]; after
/// [`MAX_RESTARTS`] crashes without [`RESTART_STABLE`] of uptime in between, the session is left for the user.
pub const RESTART_BASE: Duration = Duration::from_millis(250);
pub const RESTART_MAX_DELAY: Duration = Duration::from_secs(8);
pub const MAX_RESTARTS: u32 = 5;
pub const RESTART_STABLE: Duration = Duration::from_secs(60);

/// The delay before restart `attempt` (1-based).
pub fn restart_delay(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(16);
    (RESTART_BASE * factor).min(RESTART_MAX_DELAY)
}

pub(super) fn heartbeat_period() -> Duration {
    std::env::var(HEARTBEAT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_HEARTBEAT, Duration::from_millis)
}

/// What new workers are started with (the server's model selection).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkerModel {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<String>,
    pub extension_mode: Option<String>,
    /// Sessions are not named (`--no-session` runs, whose session is removed when they end).
    pub no_naming: bool,
    /// `--models`: the scoped model patterns a new session's model is chosen from (else `enabledModels`).
    pub models: Option<Vec<String>>,
    /// `--system-prompt`, `--no-skills`, `--no-context-files`.
    pub session: session_core::session_flags::SessionFlags,
    /// Environment the workers start with on top of this process's: what the CLI set for its run (`--offline`'s
    /// `PI_OFFLINE`, the `httpProxy` setting's `HTTP_PROXY`/`HTTPS_PROXY`), as TypeScript sets `process.env`.
    pub env: Vec<(String, String)>,
    /// `--approve` (`Some(true)`) / `--no-approve` (`Some(false)`): the run's project trust override, which the
    /// workers read from [`config::trust::TRUST_OVERRIDE_ENV`].
    pub project_trust: Option<bool>,
}

impl WorkerModel {
    /// A client's own session options (`ultron durable --model ...`), as its `attach` request carries them to a
    /// server whose workers start on other options; `None` when it sets none.
    pub fn session_options(&self) -> Option<Value> {
        let mut options = json!({});
        if let Some(model) = &self.model {
            options["model"] = json!(model);
            if let Some(provider) = &self.provider {
                options["provider"] = json!(provider);
            }
        }
        if let Some(key) = &self.api_key {
            options["apiKey"] = json!(key);
        }
        if let Some(thinking) = &self.thinking {
            options["thinking"] = json!(thinking);
        }
        if let Some(models) = &self.models {
            options["models"] = json!(models);
        }
        if self.no_naming {
            options["noNaming"] = json!(true);
        }
        if let Some(trusted) = self.project_trust {
            options["projectTrust"] = json!(trusted);
        }
        self.session.write_worker_options(&mut options);
        options.as_object().is_some_and(|o| !o.is_empty()).then_some(options)
    }

    /// These options with a client's session options (see [`WorkerModel::session_options`]) over them.
    pub fn with_session_options(&self, options: &Value) -> WorkerModel {
        let text = |key: &str| options[key].as_str().map(str::to_string);
        let mut model = self.clone();
        if options.get("model").is_some() {
            model.provider = text("provider");
            model.model = text("model");
        }
        if let Some(key) = text("apiKey") {
            model.api_key = Some(key);
        }
        if let Some(thinking) = text("thinking") {
            model.thinking = Some(thinking);
        }
        if let Some(models) = options["models"].as_array() {
            model.models = Some(models.iter().filter_map(Value::as_str).map(str::to_string).collect());
        }
        model.no_naming |= options["noNaming"] == true;
        if let Some(trusted) = options["projectTrust"].as_bool() {
            model.project_trust = Some(trusted);
        }
        let flags = session_core::session_flags::SessionFlags::from_worker_options(options);
        if flags.system_prompt.is_some() {
            model.session.system_prompt = flags.system_prompt;
        }
        if flags.append_system_prompt.is_some() {
            model.session.append_system_prompt = flags.append_system_prompt;
        }
        model.session.no_skills |= flags.no_skills;
        model.session.no_context_files |= flags.no_context_files;
        if !flags.skill_paths.is_empty() {
            model.session.skill_paths = flags.skill_paths;
        }
        model
    }
}

type TerminationListener = Box<dyn FnOnce(Option<String>) + Send>;

/// Resolves once with `None` for an expected stop or the unexpected termination's message.
#[derive(Default)]
pub struct Terminated {
    state: Mutex<Option<Option<String>>>,
    listeners: Mutex<Vec<TerminationListener>>,
    changed: Condvar,
}

impl Terminated {
    pub(super) fn resolve(&self, error: Option<String>) {
        {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.is_some() {
                return;
            }
            *state = Some(error.clone());
        }
        self.changed.notify_all();
        let listeners = std::mem::take(&mut *self.listeners.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for listener in listeners {
            listener(error.clone());
        }
    }

    /// Run `listener` once it resolves (now, if it already has).
    pub fn on_resolve(&self, listener: Box<dyn FnOnce(Option<String>) + Send>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match state {
            Some(error) => listener(error),
            None => self
                .listeners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(listener),
        }
    }

    pub fn is_resolved(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    pub(super) fn wait(&self, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |s| s.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.is_some()
    }
}

/// Why a demand update did not settle as applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemandError {
    /// The worker did not answer in time: it may hold the demand or not.
    TimedOut,
    /// Not sent, refused, or the worker went away: it does not hold the demand.
    Failed(String),
}

impl std::fmt::Display for DemandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DemandError::TimedOut => f.write_str("Session worker demand update timed out"),
            DemandError::Failed(message) => f.write_str(message),
        }
    }
}

/// One attachment's demand as the server holds it for a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentDemand {
    /// Attached (or its attach under way): requests on it are routed and it keeps the worker alive.
    Attached,
    /// Released here, but the worker has not confirmed the detach: it may still hold the demand (and so never retire),
    /// so the detach is sent again until the worker applies it, refuses it or goes away.
    Withdrawing,
}

/// The attachments a server demands of one worker, and the detaches it still owes the worker.
#[derive(Debug, Default)]
pub struct DemandLedger {
    attachments: HashMap<String, AttachmentDemand>,
}

impl DemandLedger {
    /// An attach of `attachment` is about to be sent.
    pub fn attach(&mut self, attachment: &str) {
        self.attachments
            .insert(attachment.to_string(), AttachmentDemand::Attached);
    }

    pub fn state(&self, attachment: &str) -> Option<AttachmentDemand> {
        self.attachments.get(attachment).copied()
    }

    pub fn is_attached(&self, attachment: &str) -> bool {
        self.state(attachment) == Some(AttachmentDemand::Attached)
    }

    /// Any client is attached (a crashed worker with one is restarted).
    pub fn any_attached(&self) -> bool {
        self.attachments.values().any(|d| *d == AttachmentDemand::Attached)
    }

    /// Release `attachment`: whether the worker must be told (it holds, or may hold, the demand).
    pub fn withdraw(&mut self, attachment: &str) -> bool {
        match self.attachments.get_mut(attachment) {
            Some(demand) => {
                *demand = AttachmentDemand::Withdrawing;
                true
            }
            None => false,
        }
    }

    /// The outcome of the attach (`attached`) or detach of `attachment`. A timed-out attach is withdrawn (the worker
    /// may hold it); a failed one is dropped. A detach settles when the worker applied or refused it or went away; one
    /// that timed out stays owed ([`DemandLedger::owed`]).
    pub fn settle(&mut self, attachment: &str, attached: bool, outcome: &Result<(), DemandError>) {
        let Some(demand) = self.attachments.get_mut(attachment) else {
            return;
        };
        match (attached, *demand, outcome) {
            (true, AttachmentDemand::Attached, Ok(())) => {}
            (true, AttachmentDemand::Attached, Err(DemandError::TimedOut)) => *demand = AttachmentDemand::Withdrawing,
            (true, AttachmentDemand::Attached, Err(DemandError::Failed(_))) => {
                self.attachments.remove(attachment);
            }
            (false, AttachmentDemand::Withdrawing, Ok(()) | Err(DemandError::Failed(_))) => {
                self.attachments.remove(attachment);
            }
            _ => {}
        }
    }

    /// The detaches the worker has not confirmed: sent again with each heartbeat.
    pub fn owed(&self) -> Vec<String> {
        let mut owed: Vec<String> = self
            .attachments
            .iter()
            .filter(|(_, d)| **d == AttachmentDemand::Withdrawing)
            .map(|(a, _)| a.clone())
            .collect();
        owed.sort();
        owed
    }
}
