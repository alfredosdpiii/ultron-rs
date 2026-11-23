//! The composition root of a session's host stack. A cell's host request passes, outermost first:
//!
//! 1. [`kernel_apis::KernelApis`]: the kernel-API modules (`skills.*` with code skills, `memory.*`,
//!    `hints.*`, `images.*`, `diagram.*`, and the local services of [`services`]: `refinements.*`, `experiments.*`, `gates.*`),
//!    matched by prefix;
//! 2. [`inference::Inference`]: `rlm.infer`, `rlm.load`, `rlm.frames`;
//! 3. [`Router`]: subagents (`rlm.spawn`, `rlm.collect`, ... served by [`delegation::host::Delegation`] for the
//!    calling lane, in host collect mode: a cell never waits); the lane API (`rlm.lanes`, `rlm.lane.*`, `rlm.cells`, `rlm.cell.cancel`, `rlm.recovery`) is answered by the
//!    session's REPL lanes ([`lanes::Lanes`]) first;
//! 4. [`host::Host`], sandboxed ([`host::Host::sandboxed`] with the `sandbox.*` settings) over the
//!    session's file hooks (Loki): shell, jobs and the before-write check.
//!
//! Each lane's kernel runs inside the session's shell sandbox (`KernelOptions::sandbox`), with `ULTRON_FILE_HOOKS=1`
//! (its writes ask the host first), a per-session spill dir, a signed snapshot under the profile's `rlm-snapshots/<session>/` with the profile
//! snapshot key, and the continuity policy (checkpoint after each cell, restore on restart). Subagent and detached
//! cell completions continue the run between turns ([`RuntimeEvents`]). The model sees one tool, `rlm`.

use crate::RunEnv as Env;
use crate::async_events::RuntimeEvent;
use crate::lane::{Provider, Rlm, rlm_over};
use crate::session_stats::{GuardOutcome, SessionStats};
use agent::{AgentOptions, AgentTool, StreamFn};
use ai::{Message, Model, UserMessage, UserMessageContent};
use delegation::agents::{Agents, AgentsOptions, Completion, PredictCall, predict_adapter};
use delegation::child::agent_runner;
use delegation::host::{ChildRun, ChildRunner, Delegation, DelegationOptions, DetachedEnd};
use delegation::usage::UsageLedger;
use host::Host;
use host::file_hooks::{FileHooks, GuardRecord, OnRecord};
use host::loki_guard::{LokiGuard, LokiMode, git_root, loki_mode};
use inference::Inference;
use jsrt::sync;
use kernel::store::{FileDocumentStore, MemoryDocumentStore};
use kernel::{ContinuityPolicy, HostHandler, Kernel, KernelOptions, Signal};
use kernel_apis::{
    CodeSkills, CodeSkillsOptions, DurableWork, DurableWorkOptions, GoalDriver, HintsModule, KernelApis, MemoryApi,
    MemoryBackend, MemoryService, SchedulesModule, SkillSource, SkillsModule, TaskGoalJobs,
};
use lanes::Park;
use lanes::{CellEnd, Lanes, LanesOptions};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

pub const CONTEXT_REMINDER_ENV: &str = "ULTRON_CONTEXT_REMINDER";
pub const CONTEXT_REMINDER_TOKENS_ENV: &str = "ULTRON_CONTEXT_REMINDER_TOKENS";
pub const GUIDE_ENV: &str = "ULTRON_GUIDE";
pub const LOKI_ENV: &str = "ULTRON_LOKI";
pub const LOKI_TIMEOUT_MS_ENV: &str = "ULTRON_LOKI_TIMEOUT_MS";
pub const MASK_SECRETS_ENV: &str = "ULTRON_MASK_SECRETS";
pub const READ_HANDLE_BYTES_ENV: &str = "ULTRON_READ_HANDLE_BYTES";

/// What a cell's `ext.call` / `ext.list` (runtime.py's Pi extension functions) is answered.
pub const EXTENSIONS_UNSUPPORTED: &str = "Pi extensions are not supported by ultron-rs";

/// Told the session's MCP gateway when its stack opens.
pub type McpListener = Arc<dyn Fn(&Arc<mcp::gateway::Gateway>) + Send + Sync>;

mod host_dispatch;
mod system_prompt;

pub use host_dispatch::{CELL_CANCELLED_SHELL, RuntimeEvents, only_rlm, session_mask, task_event_summary};
use host_dispatch::{CellShell, FramesOfCell, ObservedByHints, Router, add_guard_stats};
pub use system_prompt::{ChildReader, SystemPrompt};

/// Collaborators a caller may supply in place of the defaults (tests, embedders).
#[derive(Clone, Default)]
pub struct Services {
    /// The memory backend behind `memory.*`; none: the memorantado server this process chose
    /// ([`memorantado::Endpoint::choose`]), else the Hindsight server and bank it resolves ([`hindsight::Endpoint`]),
    /// or memory off ("Hindsight is not configured") when that resolves to off.
    pub memory: Option<Arc<dyn MemoryBackend>>,
    /// Told the session's MCP gateway once its stack opens (the session worker serves `/mcp` with it).
    pub on_mcp: Option<McpListener>,
    /// Where the gateway's notices go when the session starts (configuration errors, an eager server that needs
    /// sign-in): the attached client's UI (`ctx.ui.notify`). None: nowhere, as without a UI in TypeScript.
    pub mcp_notices: Option<mcp::gateway::NoticeSink>,
    /// `--system-prompt`, `--no-skills`, `--no-context-files` (crate::session_flags).
    pub session: crate::session_flags::SessionFlags,
    /// The local skills and prompts (crate::session_resources), resolved by [`Runtime::new`].
    pub resources: Arc<crate::session_resources::SessionResources>,
    /// The session whose values the module stores are (`ultron.module/<name>`, `ultron.local/<key>`, the memory
    /// journal), set by the session actor: resuming restores them and a tree fork copies them (A07). None: files
    /// under `rlm-modules/<session id>/` (no session).
    pub values: Option<services::SessionValues>,
    /// Told the session's active schedules (`schedule.*`, kernel_apis::prompt_schedule) after every change: the
    /// session worker reports them to its server, which fires them.
    pub on_schedules: Option<kernel_apis::prompt_schedule::Listener>,
    /// Runs typed agents' `rlm` lanes for a stack without a [`SubagentHost`] (its typed agents' records stay in
    /// memory); none: such a stack has no `rlm` strategy. A host's own [`AgentStores::lanes`] wins.
    pub agent_lanes: Option<AgentLanes>,
}

/// Builds a typed-agent lane runner from the stack being built (it may hold the stack weakly).
pub type AgentLanes = Arc<dyn Fn(Weak<Stack>) -> delegation::agents::LaneRunner + Send + Sync>;

/// What every lane of this process shares: settings, locations, guards and the snapshot key. Owned
/// values only, so subagent lanes can be built on their own threads.
pub struct Config {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub agent_dir: PathBuf,
    /// The merged settings (`sandbox.*`, `loki`, ...).
    pub settings: Value,
    pub loki: LokiMode,
    /// Which hygiene advisories Loki adds after a write (`ULTRON_LOKI_HYGIENE`, else `loki.hygiene`, else on).
    pub loki_hygiene: loki::hygiene::Level,
    /// Where Loki's policy is read: the Git work tree containing the cwd, else the cwd (None when Loki is off).
    pub loki_root: Option<PathBuf>,
    pub runtime_path: PathBuf,
    pub skills_dir: PathBuf,
    pub mask_secrets: Option<String>,
    pub output_bytes: Option<String>,
    /// `ULTRON_CELL_YIELD_AFTER`: how long a woken root's cell may hold its tool call before it continues in the
    /// background (crate::lane `cell_yield_after`).
    pub cell_yield_after: Option<String>,
    pub snapshot_key: Option<Vec<u8>>,
    pub spawn_depth: u32,
    /// `ULTRON_ASYNC_EVENTS` is not off: completions of detached work continue the run.
    pub async_events: bool,
    /// `ULTRON_GUIDE=ts` ([`ts_guide`]).
    pub ts_guide: bool,
    /// `ULTRON_HINTS=on` ([`hints`]).
    pub hints: bool,
    /// Not `ULTRON_TURN_GUARDS=off`, outside the TypeScript guide ([`turn_guards`]).
    pub turn_guards: bool,
    /// The global `rlm.resultFooter` setting, outside the TypeScript guide: REPL results end with
    /// [`crate::lane::result_footer`]s, and the runtime guide says so.
    pub result_footer: bool,
    /// Rounds without a user message before the root is nudged (`ULTRON_TOOL_ROUNDS_NUDGE`).
    pub tool_rounds_nudge: u64,
    /// Turns before a skill-extraction nudge (`ULTRON_SKILL_NUDGE`).
    pub skill_nudge: u64,
    /// Tool-result images a request keeps (`ULTRON_CONTEXT_IMAGES`; 0: all).
    pub context_images: usize,
    /// The frame settings `rlm.map` / `rlm.infer` read from the environment (`ULTRON_RLM_FRAME_MODEL`,
    /// `ULTRON_REVIEW_MODEL`, `ULTRON_RLM_MAP_TOKENS`).
    pub frame_env: HashMap<String, String>,
    /// The process environment, for models.json keys and headers (`$VAR` values).
    pub var: crate::VarLookup,
    /// Cell output masking for every lane: the values MCP servers resolve (env, headers) are registered here, as
    /// TypeScript's process-wide `registerSecretValues`.
    pub masker: Arc<host::secrets::OutputMasker>,
    /// Whether the project's own configuration (`.pi/mcp.json`) may be read.
    pub project_trusted: bool,
    /// The current key of a `/login` provider, for MCP servers with `auth.provider`.
    pub provider_token: mcp::connection::ProviderToken,
    /// Files Loki's own setup commit changed (crate::loki_notice): not the changes of the cell it ran beside.
    pub outside_changes: host::file_hooks::OutsideChanges,
    /// The session's language servers, shared by every lane; None when they are off (`lsp::enabled`).
    pub lsp: Option<lsp::LspGuard>,
    /// The terminal appearance the attached TUI reported ([`crate::core::Op::SetTerminalTheme`]): `diagram.mermaid`'s
    /// `auto` theme follows it before the session's `COLORFGBG`.
    pub terminal_theme: Arc<std::sync::Mutex<Option<String>>>,
}

/// After-cell checks picture the directory between cells (workspace-snapshot.ts).
pub fn workspace_snapshotter() -> host::file_hooks::Snapshotter {
    std::sync::Arc::new(|dir: &Path| {
        let picture = delegation::workspace::snapshot_workspace(dir);
        Ok(host::file_hooks::Snapshot {
            root: picture.root,
            entries: picture.entries,
            complete: picture.complete,
        })
    })
}

/// Variables a session's commands and kernels receive from the session's environment even when this process has
/// none of that name (the session's lookup cannot be listed): the shell's essentials and the kernel's own settings.
const SESSION_ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "PYTHONPATH",
    "PYTHONHOME",
    "VIRTUAL_ENV",
    config::env::OFFLINE_ENV,
    config::env::SKIP_VERSION_CHECK_ENV,
    kernel::env::PYTHON_ENV,
    kernel::env::RLM_ENV_ALLOW_ENV,
    kernel::env::BASH_YIELD_AFTER_ENV,
    kernel::env::RLM_MAX_MEMORY_MB_ENV,
    kernel::env::RLM_MAX_CPU_SECONDS_ENV,
    kernel::env::RLM_OUTPUT_BYTES_ENV,
    kernel::env::RLM_MAX_TREE_MEMORY_MB_ENV,
    kernel::env::RLM_TREE_MEMORY_BACKEND_ENV,
];

/// The environment a session's kernels and commands start from (TS: the worker's `process.env`, which kernel.ts
/// `kernelEnvironment` filters and `getShellEnv` extends): every name this process has plus [`SESSION_ENV_NAMES`],
/// each with the value the session's `var` gives it (dropped when it gives none). Values come only from the session's
/// environment, never from this process's: a session whose PATH differs runs its tools from its own PATH.
pub fn session_environment(var: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .collect();
    for name in SESSION_ENV_NAMES {
        if !names.iter().any(|known| known == name) {
            names.push((*name).to_string());
        }
    }
    names
        .into_iter()
        .filter_map(|name| var(&name).map(|value| (name, value)))
        .collect()
}

/// Whether `ULTRON_GUIDE=ts` asks for the TypeScript guide and output (the TS-recorded replays use it): no Rust-only
/// APIs, wording or cell-result additions.
pub fn ts_guide(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var(GUIDE_ENV).as_deref() == Some("ts")
}

/// Whether `ULTRON_HINTS=on` ends cell results with situational `[hint:<tag>]` lines (kernel_apis::cell_hints), which
/// the runtime guide then explains.
pub fn hints(var: &dyn Fn(&str) -> Option<String>) -> bool {
    kernel_apis::cell_hints::hints_enabled(var(kernel_apis::cell_hints::HINTS_ENV).as_deref())
}

/// `ULTRON_TURN_GUARDS`: `off` turns the turn guards off.
pub const TURN_GUARDS_ENV: &str = "ULTRON_TURN_GUARDS";

/// Whether the turn guards are on: a lane going in circles is told so and its seventh identical cell in a row is
/// refused (kernel_apis::cell_hints), and the root's turn is continued after an empty reply or with what its
/// subagents left undone (crate::core `turn_guard`). Off under `ULTRON_TURN_GUARDS=off` and the TypeScript guide.
pub fn turn_guards(var: &dyn Fn(&str) -> Option<String>) -> bool {
    !ts_guide(var) && !crate::flag_off(var(TURN_GUARDS_ENV).as_deref())
}

impl Config {
    pub fn child_env(&self) -> Vec<(String, String)> {
        session_environment(&*self.var)
    }

    /// The kernel environment and output budget a lane reads.
    pub fn from_env(env: &Env) -> Config {
        let agent_dir = env.agent_dir();
        let manager = config::settings::SettingsManager::for_worker(&env.cwd, &agent_dir, &env.home);
        let settings = Value::Object(manager.settings().clone());
        let loki_settings = loki::settings::saved(manager.global_settings());
        let loki = loki_mode((env.var)(LOKI_ENV).as_deref(), &loki_settings);
        let loki_hygiene = loki::hygiene::level(
            (env.var)("ULTRON_LOKI_HYGIENE")
                .or_else(|| (env.var)(loki::hygiene::HYGIENE_ENV))
                .as_deref(),
            &loki_settings,
        );
        let loki_root = (loki != LokiMode::Off).then(|| git_root(&env.cwd).unwrap_or_else(|| env.cwd.clone()));
        let skills_dir = (env.var)(kernel::env::CODE_SKILLS_DIR_ENV)
            .map(|dir| dir.trim().to_string())
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| agent_dir.join("skills"), PathBuf::from);
        let lsp = lsp::enabled((env.var)(lsp::LSP_ENV).as_deref(), &settings)
            .then(|| lsp::LspGuard::new(session_environment(&*env.var)));
        Config {
            cwd: env.cwd.clone(),
            home: env.home.clone(),
            snapshot_key: kernel::snapshot_store::session_snapshot_key(&agent_dir, &*env.var),
            agent_dir,
            settings,
            loki,
            loki_hygiene,
            loki_root,
            runtime_path: env.runtime(),
            skills_dir,
            mask_secrets: (env.var)(MASK_SECRETS_ENV),
            output_bytes: (env.var)(kernel::env::RLM_OUTPUT_BYTES_ENV),
            cell_yield_after: (env.var)(kernel::env::CELL_YIELD_AFTER_ENV),
            spawn_depth: delegation::host::spawn_depth_limit((env.var)(delegation::host::SPAWN_DEPTH_ENV).as_deref()),
            async_events: crate::async_events::async_events_enabled(
                (env.var)(crate::async_events::ASYNC_EVENTS_ENV).as_deref(),
            ),
            ts_guide: ts_guide(&*env.var),
            hints: hints(&*env.var),
            turn_guards: turn_guards(&*env.var),
            result_footer: !ts_guide(&*env.var) && config::keys::result_footer(manager.global_settings()),
            tool_rounds_nudge: crate::nudge::tool_rounds_nudge_from_env(
                (env.var)(crate::nudge::TOOL_ROUNDS_NUDGE_ENV).as_deref(),
            ),
            skill_nudge: crate::nudge::skill_nudge_from_env((env.var)(crate::nudge::SKILL_NUDGE_ENV).as_deref()),
            context_images: crate::view_images::context_image_budget(
                (env.var)(crate::view_images::CONTEXT_IMAGES_ENV).as_deref(),
            ),
            frame_env: inference::frame_env(&*env.var),
            var: env.var.clone(),
            masker: Arc::default(),
            project_trusted: manager.is_project_trusted(),
            outside_changes: host::file_hooks::OutsideChanges::default(),
            lsp,
            terminal_theme: Arc::default(),
            provider_token: {
                let env = env.clone();
                Arc::new(move |provider: &str| crate::lane::api_key(&crate::ModelFlags::default(), &env, provider))
            },
        }
    }

    /// Reads the settings as they are now, with the worker's project trust (fixed when the session opened): what the
    /// TypeScript worker reads from its live `SettingsManager` per frame, spawn or request.
    pub fn settings_reader(&self) -> Arc<dyn Fn() -> config::settings::SettingsManager + Send + Sync> {
        let (cwd, agent_dir, trusted) = (self.cwd.clone(), self.agent_dir.clone(), self.project_trusted);
        Arc::new(move || config::settings::SettingsManager::create(&cwd, &agent_dir, trusted))
    }

    /// The bundled skills directory (packages/coding-agent/skills, beside the runtime), unless
    /// `ULTRON_BUNDLED_SKILLS` turns it off.
    pub fn bundled_skills(&self) -> Option<String> {
        let root = self.runtime_path.ancestors().nth(4)?;
        crate::session_resources::bundled_skills(&root.join("skills"), &*self.var)
    }

    /// The session's MCP gateway (TypeScript's extensions/mcp, built in): the servers of `mcp.json`, and of `.pi/mcp.json` in a trusted
    /// project, reached from the REPL through the `mcp` tool. Server log messages go to `<agentDir>/mcp.log`.
    pub fn mcp_gateway(&self) -> Arc<mcp::gateway::Gateway> {
        let masker = self.masker.clone();
        mcp::gateway::Gateway::new(mcp::gateway::GatewayOptions {
            agent_dir: self.agent_dir.clone(),
            project_trusted: self.project_trusted,
            servers: None,
            env: mcp::connection::ConnectionEnv {
                cwd: self.cwd.clone(),
                home: self.home.clone(),
                env_overlay: HashMap::new(),
                client_version: config::version().to_string(),
                log: Some(Arc::new(mcp::log::ServerLog::new(self.agent_dir.join("mcp.log")))),
                secrets: Some(Arc::new(move |values: &[String]| {
                    let values: Vec<Value> = values.iter().map(|v| Value::String(v.clone())).collect();
                    masker.register_secret_values(&values);
                })),
                provider_token: Some(self.provider_token.clone()),
                proxy: Some(ai::net::ProxyEnv::from_lookup(&*self.var)),
                var: Some(self.var.clone()),
                ..mcp::connection::ConnectionEnv::default()
            },
            open_url: Some(config::browser::browser_opener(
                (self.var)("PATH"),
                (self.var)("HOME"),
                config::browser::opener_vars(&*self.var),
            )),
        })
    }

    /// The model `provider`/`id` of the built-in catalog or models.json, ready for requests.
    pub fn model(&self, provider: &str, id: &str) -> Option<Model> {
        let env = Env {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            var: self.var.clone(),
            package: PathBuf::new(),
        };
        crate::lane::catalog_model(provider, id, &env).ok()
    }

    /// `rlm.find_models` (session-worker.ts): the available models (`registry.getAvailable()`) as
    /// `{provider, id, name}`, whatever the query.
    pub fn find_models(&self) -> Value {
        let env = Env {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            var: self.var.clone(),
            package: PathBuf::new(),
        };
        Value::Array(
            crate::lane::available_models(&env)
                .iter()
                .map(|m| json!({"provider": m.provider, "id": m.id, "name": m.name}))
                .collect(),
        )
    }

    /// `rlm.race_models`: `rlm.raceModels` (/settings -> Models) read now, keeping the refs that name a known model.
    pub fn race_models(&self) -> Value {
        let picked = config::keys::rlm_model_settings((self.settings_reader())().global_settings()).race_models;
        picked
            .into_iter()
            .filter(|spec| spec.split_once('/').is_some_and(|(p, id)| self.model(p, id).is_some()))
            .map(Value::String)
            .collect()
    }

    /// Refuses a spawn's `model=` unless it names a model of the built-in catalog or models.json.
    fn model_check(self: &Arc<Self>) -> delegation::host::ModelCheck {
        let config = self.clone();
        Arc::new(move |spec: &str| {
            match spec
                .split_once('/')
                .and_then(|(provider, id)| config.model(provider, id))
            {
                Some(_) => Ok(()),
                None => Err(format!("model must be provider/model naming a known model, not {spec}")),
            }
        })
    }

    /// The session's file hooks (session-worker.ts): Loki's guard unless `ULTRON_LOKI` or the global `loki.mode`
    /// setting turns it off.
    /// In `cwd` (a worktree subagent's own), Loki checks that tree. Every guard outcome goes to `ULTRON_LOKI_LOG`, then
    /// to `record` (the session's counters).
    fn file_hooks_in(&self, cwd: &Path, record: Option<OnRecord>) -> FileHooks {
        let mut hooks = FileHooks::with_snapshots(cwd, workspace_snapshotter());
        hooks.ignore_outside(self.outside_changes.clone());
        hooks.protect(config::paths::AgentPaths::new(&self.agent_dir).protected(), &self.home);
        match (
            crate::guard_log::loki_log((self.var)(crate::guard_log::LOKI_LOG_ENV).as_deref()),
            record,
        ) {
            (Some(log), Some(record)) => hooks.on_record(Arc::new(move |outcome: &GuardRecord| {
                log(outcome);
                record(outcome);
            })),
            (Some(only), None) | (None, Some(only)) => hooks.on_record(only),
            (None, None) => {}
        }
        if let Some(root) = &self.loki_root {
            let root = if cwd == self.cwd {
                root.clone()
            } else {
                git_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
            };
            let post_write = git_root(cwd).is_some() && root.join(".loki").is_dir();
            hooks.add(Box::new(
                LokiGuard::new(&root, self.loki)
                    .with_post_write(post_write)
                    .with_hygiene(self.loki_hygiene)
                    .with_project_python(crate::guard_log::project_python(&*self.var, &self.cwd))
                    .with_timeout(host::loki_guard::loki_timeout(
                        (self.var)(LOKI_TIMEOUT_MS_ENV).as_deref(),
                    )),
            ));
        }
        if let Some(lsp) = &self.lsp {
            hooks.add(Box::new(lsp.clone()));
        }
        hooks
    }

    /// Per-session directories under the profile, private like snapshots.
    fn session_dir(&self, kind: &str, session_id: &str) -> PathBuf {
        self.agent_dir.join(kind).join(session_id)
    }
}

/// Lane names as file names (`laneDir` of session-worker.ts).
fn lane_dir(lane: &str) -> String {
    lane.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Live kernels a session keeps (`maxLive` of the TS rlm tool, 16); `rlm.maxLiveKernels` overrides it.
fn max_live_kernels(settings: &Value) -> usize {
    settings["rlm"]["maxLiveKernels"]
        .as_u64()
        .filter(|n| *n >= 1)
        .map_or(16, |n| n.min(1024) as usize)
}

/// Where each lane's kernel snapshot lives (the path its kernel restores at startup): the pool snapshots an idle
/// kernel there before evicting it.
fn lane_snapshot_path(config: &Arc<Config>, session_id: &str) -> lanes::SnapshotPathFn {
    let snapshots = config.session_dir("rlm-snapshots", session_id);
    kernel::sweep_orphan_snapshots(&snapshots, std::time::Duration::from_secs(600));
    Arc::new(move |lane: &str| {
        Some(
            snapshots
                .join(format!("{}.snapshot", lane_dir(lane)))
                .display()
                .to_string(),
        )
    })
}

/// The runtime guide's view of the `mcp` gateway: the tool and its servers when `mcp.json` names any.
pub fn mcp_runtime_guide(config: &Config) -> resources::runtime_guide::RuntimeGuideOptions {
    let tool = config.mcp_gateway().tool();
    resources::runtime_guide::RuntimeGuideOptions {
        mcp_servers: tool
            .as_ref()
            .map_or_default(|t| mcp::gateway::server_names_of(&t.description)),
        extension_tools: tool
            .into_iter()
            .map(|t| resources::runtime_guide::ExtensionToolSummary {
                name: t.name,
                description: t.description,
            })
            .collect(),
        durable_apis: !config.ts_guide,
        delivery: if config.async_events {
            resources::runtime_guide::EventDelivery::Wake
        } else {
            resources::runtime_guide::EventDelivery::Off
        },
        host_waits: !config.ts_guide,
        hints: config.hints,
        result_footer: config.result_footer,
        ..Default::default()
    }
}

/// The process-wide composition: configuration and injected services.
pub struct Runtime {
    pub config: Arc<Config>,
    pub services: Services,
    /// Problems found building the session (the `alwaysSkills` warnings of the system prompt), reported once.
    pub diagnostics: Vec<String>,
    /// The root lane's system prompt (subagents use it too).
    pub system: SystemPrompt,
    /// Where session-scoped host files (job logs, inference context) live: the cwd's session directory.
    pub sessions_dir: PathBuf,
    /// `print`, `json`, `rpc` or `interactive`.
    pub mode: String,
    /// The Hindsight server and bank every instance shares (`ULTRON_HINDSIGHT_URL`, the global `hindsightUrl`
    /// setting, else localhost:8888; `ULTRON_HINDSIGHT_BANK`, else "ultron"); `None`: memory is off.
    pub hindsight: Option<hindsight::Endpoint>,
    /// The memorantado server memory uses instead, when `memory.backend` says so or (unset) it answered once
    /// (memorantado::resolve).
    pub memorantado: Option<memorantado::Endpoint>,
    /// Set when memorantado is the backend but did not answer at start: memory is paused (never written to
    /// Hindsight instead); the session shows it once.
    pub memory_notice: Option<String>,
    /// Whether the memory server was set explicitly (`ULTRON_HINDSIGHT_URL` or the `hindsightUrl` setting;
    /// `memory.backend: memorantado`, `ULTRON_MEMORANTADO_URL` or `memorantadoUrl`), not a default: automatic
    /// memory defaults to it (crate::auto_memory).
    pub memory_explicit: bool,
}

/// The memory backend a process uses ([`memory_backend`]).
pub struct MemoryChoice {
    pub backend: memorantado::Backend,
    /// The Hindsight server resolved from `ULTRON_HINDSIGHT_URL` and `hindsightUrl` (`None`: off), whichever backend
    /// was chosen.
    pub hindsight: Option<hindsight::Endpoint>,
    /// Memory was configured on purpose (a server set, or `memory.backend`), not only the defaults.
    pub explicit: bool,
}

/// The memory backend of a process with `var` and the settings of `manager` (profile `agent_dir`):
/// [`memorantado::Endpoint::choose`] over the Hindsight server resolved the same way, probing memorantado over HTTP.
/// Sessions and `ultron memory` both choose through it.
pub fn memory_backend(
    var: &dyn Fn(&str) -> Option<String>,
    manager: &config::settings::SettingsManager,
    agent_dir: &Path,
) -> MemoryChoice {
    let (url, source) = hindsight::resolve::resolve_in(var, manager);
    let hindsight_set = source != hindsight::resolve::Source::Default;
    let hindsight = hindsight::Endpoint::at(url, var);
    let resolved = match (&hindsight, hindsight_set) {
        (None, _) => memorantado::Hindsight::Off,
        (Some(_), false) => memorantado::Hindsight::Default,
        (Some(_), true) => memorantado::Hindsight::Set,
    };
    let backend = memorantado::Endpoint::choose(var, manager, agent_dir, resolved, &memorantado::resolve::probe);
    let explicit = hindsight_set
        || (backend.memorantado().is_some()
            && (memorantado::resolve::choice(manager) == Some(memorantado::Choice::Memorantado)
                || var(memorantado::resolve::MEMORANTADO_URL_ENV).is_some()
                || memorantado::resolve::setting_url(manager).is_some()));
    MemoryChoice {
        backend,
        hindsight,
        explicit,
    }
}

impl Runtime {
    /// Read the settings and the local resources for `cwd` (`mode`: "print", "json" or "rpc") and build the root
    /// system prompt.
    pub fn new(env: &Env, mode: &str, sessions_dir: PathBuf, services: Services) -> Result<Runtime, String> {
        let mut services = services;
        let config = Config::from_env(env);
        let trusted = config.project_trusted;
        let manager = config::settings::SettingsManager::create(&env.cwd, &config.agent_dir, trusted);
        let mut resolved = crate::session_resources::resolve(
            env,
            config::settings::SettingsManager::create(&env.cwd, &config.agent_dir, trusted),
            services.session.append_system_prompt.as_deref(),
        );
        resolved
            .skill_paths
            .extend(services.session.skill_paths.iter().cloned());
        services.resources = Arc::new(resolved);
        let mut diagnostics: Vec<String> = Vec::new();
        let MemoryChoice {
            backend,
            hindsight,
            explicit: memory_explicit,
        } = memory_backend(&*env.var, &manager, &config.agent_dir);
        let memory_notice = match &backend {
            memorantado::Backend::Memorantado { endpoint, up: false } => Some(endpoint.paused_notice()),
            _ => None,
        };
        diagnostics.extend(memory_notice.clone());
        let hindsight = hindsight.filter(|_| backend == memorantado::Backend::Hindsight);
        let memorantado = backend.memorantado().cloned();
        let runtime_guide = resources::runtime_guide::RuntimeGuideOptions {
            memory_topics: memorantado.is_some() && services.memory.is_none(),
            ..mcp_runtime_guide(&config)
        };
        let system = match &services.session.system_prompt {
            Some(text) => SystemPrompt::fixed(text.clone()),
            None => SystemPrompt::rendered(
                env,
                &config,
                trusted,
                runtime_guide,
                &services.session,
                services.resources.clone(),
            )?,
        };
        diagnostics.extend(system.warnings());
        if let Err(error) =
            crate::core::turn_guard::parse_checks(config.settings.as_object().unwrap_or(&Default::default()))
        {
            diagnostics.push(format!("{error}; no turn-end check runs"));
        }
        Ok(Runtime {
            config: Arc::new(config),
            services,
            diagnostics,
            system,
            sessions_dir,
            mode: mode.to_string(),
            hindsight,
            memorantado,
            memory_notice,
            memory_explicit,
        })
    }

    /// The host stack of one session, whose lanes call `model`; `api_key` (`--api-key`) is the key of `model`'s
    /// provider, every other provider's requests resolve their own credentials per request.
    pub fn stack(&self, model: &Model, api_key: Option<String>, session_id: &str) -> Result<Arc<Stack>, String> {
        self.stack_with(model, api_key, session_id, None)
    }

    /// [`Runtime::stack`] whose subagents run as `subagents` says (default: each on its agent loop, with an
    /// in-memory task journal).
    pub fn stack_with(
        &self,
        model: &Model,
        api_key: Option<String>,
        session_id: &str,
        subagents: Option<SubagentHost>,
    ) -> Result<Arc<Stack>, String> {
        let mut subagents = subagents;
        let memory_children = subagents
            .is_none()
            .then(|| Arc::new(crate::memory_children::MemoryChildren::default()));
        let config = self.config.clone();
        let values = self.services.values.clone();
        let jobs: Arc<dyn kernel::store::DocumentStore> = match &values {
            Some(values) => Arc::new(crate::session_modules::ModuleJobJournal(
                crate::session_modules::ModuleDocument(values.document(session_journal::ns::MODULE, "jobs")),
            )),
            None => Arc::new(kernel::store::FileDocumentStore::new(
                config.session_dir("rlm-modules", session_id).join("jobs.json"),
            )),
        };
        let stats = Arc::new(SessionStats::system("lane"));
        let memory_records = Arc::new(kernel_apis::memory_records::RecordIndex::for_agent_dir(
            &config.agent_dir,
        ));
        let skill_runs = Arc::new(delegation::skill_runs::RunBook::default());
        let memory_pointers = self.memory_pointers(session_id, &memory_records);
        let situational_skills = self.situational_skills_in(&config.cwd);
        let observers = memory_pointers
            .clone()
            .into_iter()
            .chain(
                situational_skills
                    .clone()
                    .map(|observer| observer as Arc<dyn host::file_hooks::CellObserver>),
            )
            .chain(Some(skill_runs.clone() as Arc<dyn host::file_hooks::CellObserver>))
            .collect();
        let host = Arc::new(
            self.host_in(&config.cwd, session_id, "bash", Some(&stats), observers)?
                .with_journal(jobs)?,
        );
        crate::guard_log::loki_setup(&config);
        let modules = |name: &str| -> Arc<dyn kernel::store::DocumentStore> {
            match &values {
                Some(values) => Arc::new(crate::session_modules::ModuleDocument(
                    values.document(session_journal::ns::MODULE, name),
                )),
                None => Arc::new(FileDocumentStore::new(
                    config
                        .session_dir("rlm-modules", session_id)
                        .join(format!("{name}.json")),
                )),
            }
        };
        let (cwd, home) = (config.cwd.display().to_string(), config.home.display().to_string());
        let backend = match (&self.services.memory, &self.memorantado, &self.hindsight) {
            (Some(backend), _, _) => Some(backend.clone()),
            (None, Some(endpoint), _) => Some(Arc::new(
                endpoint
                    .session_backend(session_id, &cwd, &home)
                    .map_err(|e| e.to_string())?,
            ) as Arc<dyn MemoryBackend>),
            (None, None, Some(endpoint)) => Some(Arc::new(
                endpoint
                    .session_backend(session_id, &cwd, &home)
                    .map_err(|e| e.to_string())?,
            ) as Arc<dyn MemoryBackend>),
            (None, None, None) => None,
        };
        let memory = match &backend {
            Some(backend) => Some(
                MemoryService::new(
                    match &values {
                        Some(values) => Arc::new(crate::session_modules::ModuleDocument(values.document(
                            session_journal::ns::MEMORY_ROOT.namespace,
                            session_journal::ns::MEMORY_ROOT.key,
                        ))),
                        None => modules("memory"),
                    },
                    backend.clone(),
                    kernel_apis::worker_gate(),
                )
                .map_err(|e| format!("memory: {e:?}"))?
                .with_records(memory_records.clone())
                .with_root(config.cwd.clone()),
            ),
            None => None,
        };
        let skills_config = config.clone();
        let no_skills = self.services.session.no_skills;
        let extra = self.services.resources.clone();
        let loader: kernel_apis::SkillLoader = Arc::new(move || Ok(skill_sources(&skills_config, &extra, no_skills)));
        let code = CodeSkills::new(CodeSkillsOptions::for_session(
            &config.skills_dir,
            &config.runtime_path,
            config.child_env(),
        ));
        let clock = kernel_apis::util::system_clock();
        let local = Arc::new(services::LocalServices::worker(match &values {
            Some(values) => Box::new(values.keyed(session_journal::ns::LOCAL)),
            None => Box::new(kernel_apis::artifacts::FileKeyedDocuments::new(
                config.session_dir("rlm-modules", session_id).join("local.json"),
            )),
        }));
        let branch = values.as_ref().map(crate::session_modules::main_branch);
        let refinements = services::instruction_hook(local.clone(), branch.clone());
        let ledger = crate::auto_refine::Ledger {
            services: local.clone(),
            branch: branch.clone(),
            store: Some(crate::auto_refine::ScopedStore::new(
                config.agent_dir.clone(),
                config.cwd.clone(),
            )),
        };
        let gateway = config.mcp_gateway();
        let notices = gateway.start(self.services.mcp_notices.clone());
        if let Some(notify) = &self.services.mcp_notices {
            for (message, level) in notices {
                notify(&message, &level);
            }
        }
        if let Some(on_mcp) = &self.services.on_mcp {
            on_mcp(&gateway);
        }
        let images = Arc::new(crate::view_images::ViewImages::new(
            model.clone(),
            config.cwd.clone(),
            config.agent_dir.clone(),
            config.project_trusted,
        ));
        let keys = crate::request_keys::Keys::new(
            api_key.map(|key| (model.provider.clone(), key)),
            config.provider_token.clone(),
        );
        let root_model = Arc::new(Mutex::new(model.clone()));
        self.system.set_model(&model.id);
        let judge_ledger: Arc<std::sync::OnceLock<UsageLedger>> = Arc::default();
        let judge_rows: Arc<std::sync::OnceLock<services::SessionValues>> = Arc::default();
        if let Some(values) = &values {
            let _ = judge_rows.set(values.clone());
        }
        let judge = Arc::new(crate::session_judge::module(
            &config,
            root_model.clone(),
            keys.clone(),
            judge_rows.clone(),
            judge_ledger.clone(),
        ));
        let skills = Arc::new(SkillsModule::new(modules("skills"), loader, clock.clone(), Some(code)));
        let skill_checks: Arc<std::sync::OnceLock<delegation::spawn_contract::CheckRunner>> = Arc::default();
        let mined_procedures = Arc::new(delegation::procedure_miner::ProcedureStore::new(
            config.agent_dir.clone(),
            config.cwd.clone(),
        ));
        let procedures = Arc::new(delegation::skill_runs::SkillRuns {
            book: skill_runs.clone(),
            skills: skills.clone(),
            judge: judge.clone(),
            cwd: config.cwd.clone(),
            env: config.child_env(),
            runner: skill_checks.clone(),
            proposals: config::paths::AgentPaths::new(&config.agent_dir).skill_proposals(),
            mined: Some(mined_procedures.clone()),
            session_id: Some(session_id.to_string()),
            now: clock.clone(),
        });
        let _ = procedures.interrupt_orphans();
        let apis = ApiModules {
            local: Arc::new(services::LocalServicesModule {
                services: local,
                branch,
            }),
            gates: Arc::new(services::ReleaseGateModule::new(modules("release-gates"), None)),
            procedures,
            skills,
            memory: Arc::new(MemoryApi {
                service: memory.map(|memory| memory.with_judge(crate::session_judge::decider(judge.clone()))),
            }),
            hints: Arc::new(HintsModule::new(modules("hints"))),
            images: Arc::new(crate::session_images::module(&config, session_id)),
            diagram: Arc::new(crate::session_diagrams::module(&config, session_id, images.clone())),
            knowledge: Arc::new(crate::session_knowledge::module(
                &config,
                self.memorantado.clone(),
                judge.clone(),
            )),
            judge,
            schedule: Arc::new(
                kernel_apis::prompt_schedule::ScheduleModule::new(
                    match &values {
                        Some(values) => Box::new(values.keyed(kernel_apis::prompt_schedule::NAMESPACE)),
                        None => Box::new(kernel_apis::artifacts::MemoryKeyedDocuments::default()),
                    },
                    session_id,
                    kernel_apis::prompt_schedule::clock_for(&*config.var),
                    self.services.on_schedules.clone(),
                )
                .with_github(kernel_apis::prompt_schedule::GitHub::for_env(&*config.var)),
            ),
            tools: Arc::new(kernel_apis::ToolCalls::new(
                gateway.tool().is_some().then(|| {
                    Arc::new(crate::session_tools::SessionTools { mcp: gateway.clone() })
                        as Arc<dyn kernel_apis::ToolRunner>
                }),
                modules("tool-calls"),
            )),
            instances: Arc::new(kernel_apis::InstancesModule::new(modules("instances"), None)),
            progress: Arc::new(kernel_apis::ProgressModule::new(modules("progress"))),
            family: Arc::new(kernel_apis::FamilyModule::new(
                modules("family"),
                kernel_apis::FamilyOptions::default(),
            )),
            sessions: Arc::new(session_search(&config, &self.sessions_dir, session_id)),
        };
        let var = |key: &str| (config.var)(key);
        let context_reminders = Arc::new(crate::context_reminder::ContextReminders::new(
            crate::context_reminder::ReminderSettings::from_vars(
                var(CONTEXT_REMINDER_ENV).as_deref(),
                var(CONTEXT_REMINDER_TOKENS_ENV).as_deref(),
            ),
        ));
        let cell_hints = Arc::new(kernel_apis::cell_hints::CellHints::new(
            apis.hints.clone(),
            kernel_apis::cell_hints::CellHintsOptions {
                enabled: config.hints,
                guards: config.turn_guards,
                max_per_tag: kernel_apis::cell_hints::hint_max_per_tag(
                    var(kernel_apis::cell_hints::HINTS_MAX_ENV).as_deref(),
                ),
                async_events: config.async_events,
                read_handle_bytes: kernel_apis::cell_hints::read_handle_bytes(var(READ_HANDLE_BYTES_ENV).as_deref()),
                ..Default::default()
            },
        ));
        let session_id = session_id.to_string();
        let system = self.system.clone();
        let sessions_dir = self.sessions_dir.clone();
        let services = self.services.clone();
        let model = model.clone();
        let frame_stream = crate::request_keys::lane_stream(config.settings_reader(), keys.clone());
        let child = ChildDefaults {
            model: model.clone(),
            keys: keys.clone(),
        };
        let mut built: Result<(), String> = Ok(());
        let (schedules_store, goal_store) = (modules("schedules"), modules("goal"));
        let wake = crate::wake::RootWake::default();
        let hints_wake = wake.clone();
        cell_hints.set_lane_woken(Arc::new(move |lane: &str| hints_wake.woken(lane)));
        let memory_children_on = memory_children.is_some();
        let agent_lanes = self.services.agent_lanes.clone();
        let stack = Arc::new_cyclic(|me: &Weak<Stack>| {
            let runner_stack = me.clone();
            let (store, inner, agent_stores) = match subagents.take() {
                Some(host) => (Some(host.store), (host.runner)(me.clone()), host.agents),
                None => {
                    let factory: delegation::child::ChildAgentFactory =
                        Arc::new(move |run: &ChildRun| child_agent(&runner_stack, &child, run));
                    let runner = match &memory_children {
                        Some(children) => children.runner(factory),
                        None => agent_runner(factory),
                    };
                    let agent_stores = agent_lanes.as_ref().map(|lanes| {
                        let lanes = lanes.clone();
                        AgentStores {
                            tasks: Box::new(MemoryDocumentStore::default()),
                            definitions: Box::new(MemoryDocumentStore::default()),
                            usage: Box::new(MemoryDocumentStore::default()),
                            lanes: Box::new(move |stack| lanes(stack)),
                            frames: Arc::new(inference::MemoryTraceStore::default()),
                        }
                    });
                    (None, runner, agent_stores)
                }
            };
            let goal_slot: Arc<std::sync::OnceLock<Arc<GoalDriver>>> = Arc::new(std::sync::OnceLock::new());
            let frames = inference::FrameSession::new(match &agent_stores {
                Some(stores) => stores.frames.clone(),
                None => Arc::new(inference::MemoryTraceStore::default()),
            });
            let (agents, usage, task_ends) = typed_agents(
                &config,
                agent_stores,
                me.clone(),
                root_model.clone(),
                keys.clone(),
                goal_slot.clone(),
                Some(refinements),
                crate::frames::frame_runner(frames.clone()),
                &stats,
            );
            let _ = judge_ledger.set(usage.clone());
            frames.set_tasks(Arc::new(crate::frames::StackFrameTasks {
                agents: agents.clone(),
                usage: usage.clone(),
                stack: me.clone(),
            }));
            let schedules = Arc::new(SchedulesModule::new(schedules_store, None));
            let goal = Arc::new(GoalDriver::new(
                goal_store,
                Arc::new(TaskGoalJobs::new(Arc::new(agents.clone()), host.clone())),
                kernel_apis::util::system_clock(),
            ));
            let _ = goal_slot.set(goal.clone());
            let closer = me.clone();
            let runner: ChildRunner = Arc::new(move |run: ChildRun| {
                let lane = run.lane_name.clone();
                let result = inner(run);
                if let Some(stack) = closer.upgrade() {
                    stack.cell_hints.run_ended(&lane);
                    let _ = stack.lanes.close_lane(&lane);
                    stack.retire_workspace(&lane);
                }
                result
            });
            let mut options = DelegationOptions::new(runner);
            if let Some(store) = store {
                options.store = store;
            }
            let lane_wake = wake.clone();
            options.collect = Arc::new(move |lane: &str| lane_wake.collect_mode(lane));
            options.spawn_depth = config.spawn_depth;
            options.session_id = session_id.clone();
            options.workspace = Some(config.cwd.clone());
            options.now = Box::new(|| jsrt::date::now_ms() as f64);
            options.keep_worktrees = delegation::worktrees::keep_worktrees(
                (config.var)(delegation::worktrees::KEEP_WORKTREES_ENV).as_deref(),
            );
            let settings = config.settings_reader();
            options.worktree_setup = Arc::new(move || {
                let configured = config::keys::worktree_settings(settings().settings());
                Some(delegation::worktrees::WorktreeSetup {
                    link: configured.link,
                    copy: configured.copy,
                    command: configured.command,
                    timeout_ms: None,
                })
            });
            let merges = stats.clone();
            options.on_merge = Some(Arc::new(move |task: &str, status: &str| merges.merge(task, status)));
            options.model_check = Some(config.model_check());
            options.check_runner = crate::contract_checks::check_runner(me.clone());
            let _ = skill_checks.set(options.check_runner.clone());
            options.command_env = config.child_env();
            let (delegation, child_ends) = Delegation::new(options);
            let settings = config.settings_reader();
            delegation.set_child_model(Arc::new(move || {
                config::keys::rlm_model_settings(settings().global_settings()).child_model
            }));
            let pruning = delegation.clone();
            std::thread::spawn(move || pruning.prune_orphan_worktrees());
            agents.set_worktree_children(delegation.clone());
            let factory_stack = me.clone();
            let handler_stack = me.clone();
            let lanes = Lanes::new(LanesOptions {
                create: Arc::new(move |lane| {
                    let stack = factory_stack.upgrade().ok_or("the session is closed")?;
                    stack.kernel(lane)
                }),
                handlers: Arc::new(move |lane, signal, park| match handler_stack.upgrade() {
                    Some(stack) => stack.cell_handler(lane, Some((signal, park))),
                    None => Arc::new(Closed) as Arc<dyn HostHandler>,
                }),
                max_lanes: 64,
                cancel_grace: Duration::from_secs(5),
                pool: Some(lanes::PoolLimits::new(
                    max_live_kernels(&config.settings),
                    Some(lane_snapshot_path(&config, &session_id)),
                )),
            });
            let (lanes, cell_ends) = match lanes {
                Ok(pair) => {
                    let calls = stats.clone();
                    pair.0
                        .observe_requests(Arc::new(move |kind: &str| calls.host_call(kind)));
                    let guards_of = me.clone();
                    pair.0.extend_pool_stats(Arc::new(move |pool: &mut Value| {
                        if let Some(stack) = guards_of.upgrade() {
                            let guards = stack.guard_stats();
                            if !guards.is_empty() {
                                pool["guards"] = Value::Array(guards);
                            }
                        }
                    }));
                    pair
                }
                Err(error) => {
                    built = Err(error);
                    let (lanes, ends) = Lanes::new(LanesOptions {
                        create: Arc::new(|_| Err("no lanes".into())),
                        handlers: Arc::new(|_, _, _| Arc::new(Closed) as Arc<dyn HostHandler>),
                        max_lanes: 1,
                        cancel_grace: Duration::ZERO,
                        pool: None,
                    })
                    .expect("one lane is a valid limit");
                    (lanes, ends)
                }
            };
            let events = Arc::new(RuntimeEvents::new(
                (task_ends, child_ends, cell_ends),
                config.async_events,
                session_mask(&config),
            ));
            Stack {
                config,
                mcp: gateway,
                host,
                delegation,
                apis,
                model,
                root_model,
                keys,
                frame_stream,
                session_id,
                sessions_dir,
                system,
                services,
                lanes,
                workspaces: Mutex::new(HashMap::new()),
                kernel_sandbox: std::sync::OnceLock::new(),
                events,
                cell_progress: Arc::new(Mutex::new(None)),
                agents,
                usage,
                frames,
                schedules,
                goal,
                work: std::sync::OnceLock::new(),
                cell_hints,
                context: std::sync::OnceLock::new(),
                images,
                swarm: std::sync::OnceLock::new(),
                budget_note: Arc::new(std::sync::OnceLock::new()),
                request_layer: Arc::new(std::sync::OnceLock::new()),
                context_reminders,
                situational_skills,
                memory_pointers,
                skill_runs,
                lane_steer: Arc::new(std::sync::OnceLock::new()),
                stats,
                judge_rows,
                retired_guards: Mutex::new(Vec::new()),
                wake,
                memory_children: memory_children_on,
                ledger,
                mined_procedures,
            }
        });
        built?;
        if let Some(children) = memory_children {
            children.attach_reminders(stack.context_reminders.clone());
            let _ = stack
                .lane_steer
                .set(Arc::new(move |lane: &str, message: &str| children.steer(lane, message)));
        }
        stack.lanes.open(session_journal::run::MAIN_LANE)?;
        let work = DurableWork::open(
            DurableWorkOptions::new(Arc::new(stack.agents.clone()))
                .schedules(stack.schedules.clone())
                .goal(stack.goal.clone())
                .jobs(stack.host.clone()),
        )?;
        let _ = stack.work.set(work);
        Ok(stack)
    }

    /// The file hooks of a host in `cwd`: Loki's (see [`Config::file_hooks_in`]), with `observers` told each cell's
    /// changes.
    fn file_hooks_in(
        &self,
        cwd: &Path,
        stats: Option<&Arc<SessionStats>>,
        observers: Vec<Arc<dyn host::file_hooks::CellObserver>>,
    ) -> FileHooks {
        let record = stats.map(|stats| {
            let stats = stats.clone();
            Arc::new(move |record: &GuardRecord| {
                stats.guard(&GuardOutcome {
                    guard: &record.guard,
                    phase: record.phase,
                    outcome: record.outcome,
                    ms: record.ms as f64,
                });
            }) as OnRecord
        });
        let hooks = self.config.file_hooks_in(cwd, record);
        for observer in observers {
            hooks.observe(observer);
        }
        hooks
    }

    /// The session's situational skills (crate::situational_skills) rooted at `target_cwd`: the settings' rules over
    /// the session's skills, while engineering mode is on.
    fn situational_skills_in(&self, target_cwd: &Path) -> Option<Arc<crate::situational_skills::SituationalSkills>> {
        let config = self.config.clone();
        let (extra, no_skills) = (self.services.resources.clone(), self.services.session.no_skills);
        let resolve = move |name: &str| {
            let cwd = config.cwd.display().to_string();
            let agent_dir = config.agent_dir.display().to_string();
            let bundled = config.bundled_skills();
            let skill = crate::session_resources::skills(&cwd, &agent_dir, &extra, no_skills, bundled.as_deref())
                .into_iter()
                .find(|skill| skill.name == name)?;
            let raw = std::fs::read_to_string(&skill.file_path).ok()?;
            Some((skill.file_path, resources::frontmatter::strip(&raw).ok()?))
        };
        let (cwd, agent_dir, home) = (
            self.config.cwd.clone(),
            self.config.agent_dir.clone(),
            self.config.home.clone(),
        );
        let engineering = move || {
            config::settings::SettingsManager::for_worker(&cwd, &agent_dir, &home)
                .settings()
                .get("engineering")
                == Some(&Value::Bool(true))
        };
        crate::situational_skills::build_observer(
            &self.config.settings,
            target_cwd,
            Box::new(resolve),
            Box::new(engineering),
            self.config.loki_hygiene,
        )
    }

    /// The session's memory pointers: on while memory is (an injected backend or a Hindsight server) and the
    /// `memory.filePointers` setting is not false; scoped by the session's identity tags.
    fn memory_pointers(
        &self,
        session_id: &str,
        records: &Arc<kernel_apis::memory_records::RecordIndex>,
    ) -> Option<Arc<dyn host::file_hooks::CellObserver>> {
        let config = &self.config;
        let filters = match &self.services.memory {
            Some(backend) => backend.scope_tags(),
            None if self.memorantado.is_some() || self.hindsight.is_some() => kernel_apis::memory::worker_scope_tags(
                session_id,
                &config.cwd.display().to_string(),
                &config.home.display().to_string(),
            ),
            None => return None,
        };
        kernel_apis::memory_pointers::session_pointers(&config.settings, records.clone(), filters)
    }

    /// A sandboxed host with its working directory at `cwd` (the session's, or a worktree subagent's); its guard
    /// outcomes are counted in `stats`.
    fn host_in(
        &self,
        cwd: &Path,
        session_id: &str,
        spill: &str,
        stats: Option<&Arc<SessionStats>>,
        observers: Vec<Arc<dyn host::file_hooks::CellObserver>>,
    ) -> Result<Host, String> {
        let config = &self.config;
        let jobs_dir = self.sessions_dir.join("rlm-jobs").join(session_id);
        let spill_dir = config.session_dir("rlm-output", session_id).join(spill);
        let tmp_dir = config.session_dir("rlm-tmp", session_id);
        for dir in [&spill_dir, &tmp_dir] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let shell_path = config
            .settings
            .as_object()
            .and_then(|settings| config::keys::shell_path(settings, &config.home));
        Ok(Host::new(
            cwd,
            jobs_dir,
            spill_dir,
            Box::new(self.file_hooks_in(cwd, stats, observers)),
        )
        .with_shell_path(shell_path)
        .with_base_env(config.child_env())
        .recording_cell_writes(!config.ts_guide)
        .sandboxed(&config.settings, &config.home, Some(&tmp_dir)))
    }
}

/// The skill catalog `skills.*` serves: the session's skills with their file contents.
fn skill_sources(
    config: &Config,
    extra: &crate::session_resources::SessionResources,
    no_skills: bool,
) -> Vec<SkillSource> {
    let cwd = config.cwd.display().to_string();
    let agent_dir = config.agent_dir.display().to_string();
    let bundled = config.bundled_skills();
    crate::session_resources::skills(&cwd, &agent_dir, extra, no_skills, bundled.as_deref())
        .into_iter()
        .map(|skill| SkillSource {
            content: std::fs::read_to_string(&skill.file_path).unwrap_or_default(),
            name: skill.name,
            description: skill.description,
            file_path: skill.file_path,
            disable_model_invocation: skill.disable_model_invocation,
        })
        .collect()
}

struct ApiModules {
    local: Arc<dyn kernel_apis::ApiModule>,
    gates: Arc<services::ReleaseGateModule>,
    skills: Arc<SkillsModule>,
    procedures: Arc<delegation::skill_runs::SkillRuns>,
    memory: Arc<MemoryApi>,
    hints: Arc<HintsModule>,
    images: Arc<kernel_apis::ImagesModule>,
    diagram: Arc<diagram::DiagramModule>,
    judge: Arc<judge::JudgeModule>,
    knowledge: Arc<graph::KnowledgeModule>,
    schedule: Arc<kernel_apis::prompt_schedule::ScheduleModule>,
    tools: Arc<kernel_apis::ToolCalls>,
    instances: Arc<kernel_apis::InstancesModule>,
    progress: Arc<kernel_apis::ProgressModule>,
    family: Arc<kernel_apis::FamilyModule>,
    sessions: Arc<kernel_apis::SessionsModule>,
}

/// The session stores `sessions.*`, `/continue` and the dream's quiet check search: the session root itself, then the
/// profile's native and Pi-format stores, then every project instance's own store (`<home>/.ultron/projects/*/sessions`,
/// where a session started in a project is kept), in name order.
// ponytail: one directory listing per call; cache the project stores if `sessions.*` shows up in a profile.
pub fn store_roots(agent_dir: &Path, home: &Path, session_root: Option<&Path>) -> Vec<PathBuf> {
    let paths = config::paths::AgentPaths::new(agent_dir);
    let mut projects: Vec<PathBuf> =
        std::fs::read_dir(home.join(config::paths::AGENT_CONFIG_DIR_NAME).join("projects"))
            .map_or_default(|entries| entries.flatten().map(|entry| entry.path().join("sessions")).collect());
    projects.sort();
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in session_root
        .map(Path::to_path_buf)
        .into_iter()
        .chain([paths.experimental_sessions_dir(), paths.sessions_dir()])
        .chain(projects)
    {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// Where a session's `sessions.*` and `/continue` look for sessions: its own session root (the parent of the cwd's
/// `sessions_dir`), then the profile's native and Pi-format stores.
pub(crate) fn session_roots(config: &Config, sessions_dir: &Path) -> Vec<PathBuf> {
    store_roots(&config.agent_dir, &config.home, sessions_dir.parent())
}

/// `sessions.*` of session `session_id`: its own session root (the parent of the cwd's `sessions_dir`) first, then
/// the profile's native and Pi-format stores.
fn session_search(config: &Config, sessions_dir: &Path, session_id: &str) -> kernel_apis::SessionsModule {
    kernel_apis::SessionsModule::new(
        session_roots(config, sessions_dir),
        session_id,
        &config.cwd.display().to_string(),
        kernel_apis::util::system_clock(),
    )
}

/// A worktree subagent's lane: its working directory, extra environment and host.
struct Workspace {
    cwd: PathBuf,
    env: Vec<(String, String)>,
    host: Arc<Host>,
}

/// A handler for a closed session.
struct Closed;

impl HostHandler for Closed {
    fn handle(&self, kind: &str, _: Map<String, Value>, _: &Signal) -> Result<Value, String> {
        Err(format!("The session is closed ({kind})"))
    }
}

/// One session's host stack: the sandboxed host, the REPL lanes, the subagent host and the kernel-API modules.
pub struct Stack {
    config: Arc<Config>,
    /// The session's MCP servers (closed with the session).
    mcp: Arc<mcp::gateway::Gateway>,
    pub host: Arc<Host>,
    pub delegation: Delegation,
    pub lanes: Lanes,
    pub events: Arc<RuntimeEvents>,
    apis: ApiModules,
    model: Model,
    /// The root lane's model now (a model switch changes it): frames and the predict adapter call it.
    root_model: Arc<Mutex<Model>>,
    /// The key of each request's provider (crate::request_keys).
    keys: crate::request_keys::Keys,
    /// What frames request through (crate::request_keys::lane_stream).
    frame_stream: Arc<dyn StreamFn>,
    session_id: String,
    sessions_dir: PathBuf,
    system: SystemPrompt,
    services: Services,
    workspaces: Mutex<HashMap<String, Workspace>>,
    /// The root lane kernels' sandbox, once built.
    kernel_sandbox: std::sync::OnceLock<Option<Arc<sandbox::Launcher>>>,
    /// The durable progress of the root lane's running cell: what its shell operations print is its partial
    /// output, kept if the worker dies before the cell ends.
    pub cell_progress: Arc<Mutex<Option<harness::chat::CellProgress>>>,
    /// The typed-agent host (`agents.*`, `background.*`, `workflows.run`) and its usage ledger.
    pub agents: Agents,
    pub usage: UsageLedger,
    /// The session's bounded-inference frames (`rlm.infer`, `rlm.map`, `rlm.frames`): budgets, traces, tasks.
    pub frames: Arc<inference::FrameSession>,
    /// `schedules.*` and the session goal (`goal.*`, `/goal`), on the root lane.
    pub schedules: Arc<SchedulesModule>,
    pub goal: Arc<GoalDriver>,
    /// Schedules, the goal and shell jobs as durable work: recovered when the stack opens, its tick running.
    pub work: std::sync::OnceLock<DurableWork>,
    /// Situational hints over every lane's cells (crate::lane::Rlm appends them).
    cell_hints: Arc<kernel_apis::cell_hints::CellHints>,
    /// Model-owned context of every lane (`ctx.*`; collapse on return on the root), once the session actor opened it.
    pub context: std::sync::OnceLock<Arc<kernel_apis::ContextControl>>,
    /// The images each lane's running cell attached with `view_image`, and the model each lane's results go to.
    pub images: Arc<crate::view_images::ViewImages>,
    /// `swarm.*`, `board.*`, `notes.*` and persistent children, once the session actor opened the durable runtime.
    pub swarm: std::sync::OnceLock<Arc<crate::swarm_ops::SwarmOps>>,
    /// The budget line every lane's cell results end with, once the session actor wired it (crate::budget_note).
    pub budget_note: Arc<std::sync::OnceLock<crate::budget_note::BudgetNote>>,
    /// A layer over every lane's host requests, set by a composition root whose root is not the session actor's
    /// (crate::request_layer); none: requests pass through.
    pub request_layer: Arc<std::sync::OnceLock<crate::request_layer::RequestLayer>>,
    /// The context-pressure reminder every lane's cell results may end with (crate::context_reminder): each lane's
    /// limit and, once the session actor wired it, the meter of its context use.
    pub context_reminders: Arc<crate::context_reminder::ContextReminders>,
    /// Situational skills observer on the root host (rearmed after context compaction).
    pub situational_skills: Option<Arc<crate::situational_skills::SituationalSkills>>,
    /// Shared file-linked memory pointers observer for worktree subagents.
    memory_pointers: Option<Arc<dyn host::file_hooks::CellObserver>>,
    /// Shared skill-procedure run book for worktree subagent observers.
    skill_runs: Arc<delegation::skill_runs::RunBook>,
    /// Steering into a child lane's running conversation (crate::agent_hub), once the session actor wired it.
    pub lane_steer: Arc<std::sync::OnceLock<crate::agent_hub::LaneSteer>>,
    /// The session's runtime counters (crate::session_stats), for `/usage`.
    pub stats: Arc<SessionStats>,
    /// Where `judge.decide` writes its usage rows: the session's values (see [`Stack::record_judge_rows_in`]).
    judge_rows: Arc<std::sync::OnceLock<services::SessionValues>>,
    /// The guard statistics of worktree subagent hosts whose runs ended (TypeScript has one set of file hooks for the
    /// whole session, so they stay in its totals).
    retired_guards: Mutex<Vec<Value>>,
    /// Whether the host wakes the root lane when its work ends (crate::wake): set by the composer that drives the
    /// root; every other lane waits in its cell.
    pub wake: crate::wake::RootWake,
    /// Subagents run as in-memory agent loops (crate::memory_children): no durable runtime behind their lanes.
    memory_children: bool,
    /// The session's refinements ledger (crate::auto_refine proposes into it and delivers from it).
    pub ledger: crate::auto_refine::Ledger,
    /// Per-project store of automatically mined procedures (delegation::procedure_miner).
    pub mined_procedures: Arc<delegation::procedure_miner::ProcedureStore>,
}

impl Stack {
    /// `judge.decide` records its calls as usage rows of `session`: for a composer whose modules keep no session
    /// values (an `ultron claude` server); a native session's go to its values already.
    pub fn record_judge_rows_in(&self, session: crate::durable_store::SharedSession) {
        let _ = self.judge_rows.set(services::SessionValues::shared(session));
    }

    /// Rearm per-compaction reminders and situational skills on `lane` after a compaction completes.
    pub fn rearm_after_compaction(&self, lane: &str) {
        self.context_reminders.rearm(lane);
        if let Some(skills) = &self.situational_skills {
            skills.rearm(lane);
        }
    }

    pub(crate) fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// `memory.*`'s service, when memory is on (crate::auto_memory recalls and keeps through it).
    pub fn memory(&self) -> Option<&kernel_apis::MemoryService> {
        self.apis.memory.service.as_ref()
    }

    /// `schedule.*`: the session's scheduled prompts (fired by the server through the session actor).
    pub fn prompt_schedules(&self) -> &Arc<kernel_apis::prompt_schedule::ScheduleModule> {
        &self.apis.schedule
    }

    /// `poolStats()` as the worker answers an inspected `rlm.pool`: the lanes' kernel pool, plus the session's
    /// file-hook guard statistics (`guards`) when it has guards.
    pub fn pool_stats(&self) -> Value {
        self.lanes.pool_stats()
    }

    /// `fileHooks.stats()` over the whole session: the session host's guards, with the same guards of every worktree
    /// subagent's host (live or ended) added in.
    pub fn guard_stats(&self) -> Vec<Value> {
        let mut totals = self.host.guard_stats();
        let others: Vec<Vec<Value>> = sync::lock(&self.workspaces)
            .values()
            .map(|w| w.host.guard_stats())
            .chain(std::iter::once(sync::lock(&self.retired_guards).clone()))
            .collect();
        for stats in others {
            add_guard_stats(&mut totals, &stats);
        }
        totals
    }

    /// A new root turn began (`fileHooks.beginTurn()`): every host's per-turn guard times start from zero.
    pub fn begin_turn(&self) {
        self.host.begin_turn();
        for workspace in sync::lock(&self.workspaces).values() {
            workspace.host.begin_turn();
        }
        for retired in sync::lock(&self.retired_guards).iter_mut() {
            retired["turnMs"] = json!(0);
        }
    }

    /// A worktree subagent's lane ended: its host goes (its shell jobs are the session's and keep running), its guard
    /// statistics stay in the session's.
    fn retire_workspace(&self, lane: &str) {
        let Some(workspace) = sync::lock(&self.workspaces).remove(lane) else {
            return;
        };
        add_guard_stats(&mut sync::lock(&self.retired_guards), &workspace.host.guard_stats());
    }

    /// The host `lane` runs its shell in: a worktree subagent's own, else the session's.
    fn host_of(&self, lane: &str) -> Arc<Host> {
        sync::lock(&self.workspaces)
            .get(lane)
            .map_or_else(|| self.host.clone(), |w| w.host.clone())
    }

    /// The handler `lane`'s requests go to (outermost layer first). `cell`: the cell's wait signal and park
    /// callback (subagent waits end with the cell); none for requests outside a cell.
    pub fn cell_handler(&self, lane: &str, cell: Option<(Signal, Park)>) -> Arc<dyn HostHandler> {
        let cell_signal = cell.as_ref().map(|(signal, _)| signal.clone());
        let delegation: Arc<dyn HostHandler> = match cell {
            Some((signal, park)) => Arc::new(self.delegation.cell_handler(lane, signal, park)),
            None => Arc::new(self.delegation.lane_handler(lane)),
        };
        let router = Router {
            delegation,
            host: self.host_of(lane),
            progress: (lane == session_journal::run::MAIN_LANE).then(|| self.cell_progress.clone()),
            lane: lane.to_string(),
            config: self.config.clone(),
            swarm: self.swarm.get().cloned(),
            cell: cell_signal.clone().map(CellShell::new),
        };
        let models = crate::frame_models::resolver(self.config.clone());
        let inference = Inference::new(
            Arc::new(router),
            self.root_model(),
            None,
            Arc::new(crate::lane::process_transport()),
            self.sessions_dir.join("rlm-context").join(&self.session_id),
        )
        .with_stream(self.frame_stream.clone())
        .with_models(Arc::new(move |spec: &str| models(spec).map(|(model, _)| (model, None))))
        .with_session(self.frames.clone(), lane);
        let settings = self.config.settings_reader();
        let models = move || {
            let settings = settings();
            let rlm = config::keys::rlm_model_settings(settings.global_settings());
            inference::ModelSettings {
                frame_model: rlm.frame_model,
                frame_thinking: rlm.frame_thinking,
                child_model: rlm.child_model,
                review_model: config::keys::review_model(settings.global_settings()),
                map_tokens: rlm.map_tokens,
            }
        };
        let frames = inference::Frames::new(inference, self.config.frame_env.clone(), models);
        let frames: Arc<dyn HostHandler> = match cell_signal {
            Some(cell) => Arc::new(FramesOfCell {
                inner: Arc::new(frames),
                cell,
            }),
            None => Arc::new(frames),
        };
        let typed: Arc<dyn HostHandler> = Arc::new(self.agents.handler(lane, frames));
        let mut apis = KernelApis::new(typed)
            .with_host(Arc::new(self.agents.clone()), lane)
            .with(self.schedules.clone() as Arc<dyn kernel_apis::ApiModule>)
            .with(self.goal.clone() as Arc<dyn kernel_apis::ApiModule>);
        if let Some(context) = self.context.get() {
            apis = apis.with(context.clone() as Arc<dyn kernel_apis::ApiModule>);
        }
        let apis = apis
            .with(self.apis.skills.clone())
            .with(self.apis.procedures.clone())
            .with(self.apis.memory.clone())
            .with(self.apis.hints.clone())
            .with(self.apis.images.clone())
            .with(self.apis.diagram.clone())
            .with(self.apis.judge.clone())
            .with(self.apis.knowledge.clone())
            .with(self.apis.schedule.clone())
            .with(self.apis.tools.clone())
            .with(self.apis.local.clone())
            .with(self.apis.gates.clone())
            .with(self.apis.instances.clone())
            .with(self.apis.progress.clone())
            .with(self.apis.sessions.clone())
            .with(Arc::new(crate::agent_hub::Family::new(
                self.apis.family.clone(),
                self.delegation.clone(),
                self.lane_steer.clone(),
            )) as Arc<dyn kernel_apis::ApiModule>);
        let observed: Arc<dyn HostHandler> = if self.cell_hints.active() {
            Arc::new(ObservedByHints {
                inner: Arc::new(apis),
                hints: self.cell_hints.clone(),
                lane: lane.to_string(),
            })
        } else {
            Arc::new(apis)
        };
        let viewed: Arc<dyn HostHandler> = Arc::new(crate::view_images::ViewImageLayer {
            inner: observed,
            images: self.images.clone(),
            lane: lane.to_string(),
        });
        Arc::new(crate::request_layer::Layered {
            inner: viewed,
            layer: self.request_layer.clone(),
            lane: lane.to_string(),
        })
    }

    /// `lane`'s kernel (not started): in the lane's working directory, inside the shell sandbox, with its snapshot
    /// under the profile and the continuity policy.
    fn kernel(&self, lane: &str) -> Result<Kernel, String> {
        let config = &self.config;
        let (cwd, extra) = match sync::lock(&self.workspaces).get(lane) {
            Some(workspace) => (workspace.cwd.clone(), workspace.env.clone()),
            None => (config.cwd.clone(), Vec::new()),
        };
        let spill_dir = config.session_dir("rlm-output", &self.session_id).join(lane_dir(lane));
        let mut options = KernelOptions::new(&cwd, &config.runtime_path);
        options.base_env = Some(config.child_env());
        options.env = vec![
            (
                kernel::env::CODE_SKILLS_DIR_ENV.into(),
                config.skills_dir.display().to_string(),
            ),
            (kernel::env::RLM_SPILL_DIR_ENV.into(), spill_dir.display().to_string()),
            (kernel::env::SESSION_ID_ENV.into(), self.session_id.clone()),
            (kernel::env::FILE_HOOKS_ENV.into(), "1".into()),
            (kernel::env::RLM_STREAM_OUTPUT_ENV.into(), "1".into()),
            ("HOME".into(), config.home.display().to_string()),
        ];
        options.env.extend(extra);
        let snapshots = config.session_dir("rlm-snapshots", &self.session_id);
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(&snapshots)
            .map_err(|e| format!("{}: {e}", snapshots.display()))?;
        options.snapshot_path = Some(snapshots.join(format!("{}.snapshot", lane_dir(lane))));
        options.snapshot_key = config.snapshot_key.clone();
        options.continuity = Some(ContinuityPolicy::default());
        options.sandbox = self.kernel_launcher(lane)?;
        options.tree_memory = Some(kernel::TreeMemoryOptions {
            backend: Some(
                (config.var)(kernel::env::RLM_TREE_MEMORY_BACKEND_ENV)
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| "watchdog".into()),
            ),
            ..kernel::TreeMemoryOptions::default()
        });
        Kernel::new(options, self.cell_handler(lane, None)).map_err(|e| e.to_string())
    }

    /// The sandbox a lane's kernel runs in: its host's shell policy plus the inference context directory, which
    /// `rlm.load` writes from inside the kernel (the kernel adds its snapshot and spill paths itself). The root
    /// lane's is built once.
    fn kernel_launcher(&self, lane: &str) -> Result<Option<Arc<sandbox::Launcher>>, String> {
        let host = self.host_of(lane);
        let workspace = sync::lock(&self.workspaces).contains_key(lane);
        if !workspace && let Some(launcher) = self.kernel_sandbox.get() {
            return Ok(launcher.clone());
        }
        let context = self.sessions_dir.join("rlm-context").join(&self.session_id);
        std::fs::create_dir_all(&context).map_err(|e| format!("{}: {e}", context.display()))?;
        let launcher = host.launcher().map(|launcher| {
            let mut policy = launcher.policy.clone();
            policy.writable.push(std::fs::canonicalize(&context).unwrap_or(context));
            Arc::new(sandbox::Launcher::new(launcher.mode, policy))
        });
        if !workspace {
            let _ = self.kernel_sandbox.set(launcher.clone());
        }
        Ok(launcher)
    }

    /// The `rlm` tool over `lane` (opened when needed).
    pub fn rlm(&self, lane: &str) -> Result<Rlm, String> {
        self.lanes.open(lane)?;
        let spill_dir = self
            .config
            .session_dir("rlm-output", &self.session_id)
            .join(lane_dir(lane));
        let hints = self.cell_hints.active().then(|| self.cell_hints.clone());
        Ok(rlm_over(
            &self.config,
            self.lanes.clone(),
            lane,
            spill_dir,
            hints,
            self.images.clone(),
            Some(self.host_of(lane)),
        )
        .with_budget_note(self.budget_note.clone())
        .with_stats(self.stats.clone())
        .with_context_reminders(self.context_reminders.clone())
        .with_wake(self.wake.clone()))
    }

    /// `listCommands`: `/mcp`, `/review`, prompt templates and skills (crate::commands).
    pub fn commands(&self) -> Vec<Value> {
        let bundled = self.config.bundled_skills();
        let commands = crate::commands::list_commands_with(
            &self.config.cwd.display().to_string(),
            &self.config.agent_dir.display().to_string(),
            bundled.as_deref(),
            &self.services.resources,
        );
        crate::session_flags::filter_commands(commands, &self.services.session)
    }

    /// The session's prompt templates, the ones [`Stack::commands`] lists as `/name` commands.
    pub fn prompt_templates(&self) -> Vec<resources::prompt_templates::PromptTemplate> {
        crate::session_resources::prompts(
            &self.config.cwd.display().to_string(),
            &self.config.agent_dir.display().to_string(),
            &self.services.resources,
        )
    }

    /// The model the session's lanes run on unless told otherwise.
    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn root_model(&self) -> Model {
        sync::lock(&self.root_model).clone()
    }

    /// The root lane switched to `model` (`set_model`, `cycle_model`): frames, the predict adapter and the root's
    /// `view_image` follow it.
    pub fn set_root_model(&self, model: &Model) {
        *sync::lock(&self.root_model) = model.clone();
        self.system.set_model(&model.id);
        self.images.set_model(None, model.clone());
    }

    /// The key of each request's provider, resolved when it is sent.
    pub fn keys(&self) -> &crate::request_keys::Keys {
        &self.keys
    }

    /// The root lane's system prompt, in the settings' current mode.
    pub fn system(&self) -> String {
        self.system.current()
    }

    /// The system prompt of a subagent lane on `model` (crate::wake: its runtime guide says to await before
    /// replying; crate::memory_guidance: a model cheaper than the root's is told how to use memory too).
    pub fn child_system(&self, model: &Model) -> String {
        self.system.child_current(
            ChildReader {
                in_memory: self.memory_children,
                memory: crate::memory_guidance::memory_guidance(model, &self.root_model(), true),
            },
            &model.id,
        )
    }

    /// As [`Stack::close`], for a worker whose session a successor worker takes over: running shell jobs keep
    /// running, journaled as running, for the successor to follow (`DurableWork::hand_over`), and subagents are not
    /// cancelled: their tasks stay unfinished in the journal, so the successor reattaches them as after a crash.
    pub fn hand_over(&self) {
        if let Some(work) = self.work.get() {
            work.hand_over();
        }
        self.apis.tools.close();
        self.mcp.close();
        self.lanes.close();
        self.stats.close();
    }

    /// Stop the session's lanes and subagents, and its durable work (the tick, goal jobs, shell jobs).
    pub fn close(&self) {
        if let Some(work) = self.work.get() {
            work.close();
        }
        self.apis.tools.close();
        self.mcp.close();
        self.agents.close();
        self.delegation.close();
        self.lanes.close();
        self.stats.close();
    }

    /// The session's situational hints (`ULTRON_HINTS`).
    pub fn cell_hints(&self) -> &Arc<kernel_apis::cell_hints::CellHints> {
        &self.cell_hints
    }

    /// The session's MCP gateway (`/mcp`, and the REPL's `mcp` namespace through `tools.*`).
    pub fn mcp(&self) -> &Arc<mcp::gateway::Gateway> {
        &self.mcp
    }

    /// The session's REPL tool calls (`tools.*`).
    pub fn tool_calls(&self) -> &Arc<kernel_apis::ToolCalls> {
        &self.apis.tools
    }

    /// The session's masking of text that reaches the model (see [`session_mask`]).
    pub fn mask(&self) -> crate::async_events::MaskText {
        session_mask(&self.config)
    }
}

/// How a session's subagents run, and where their task journal lives.
pub struct SubagentHost {
    pub store: Box<dyn kernel::store::DocumentStore>,
    /// Builds the runner from the stack being built (it may hold the stack weakly).
    pub runner: Box<dyn FnOnce(Weak<Stack>) -> delegation::host::ChildRunner>,
    /// The typed agents' task journal, definition registry and usage ledger stores, and the runner of their `rlm`
    /// lanes (none: in memory, and no `rlm` strategy).
    pub agents: Option<AgentStores>,
}

/// Where the typed-agent host keeps its records, and how its `rlm` lanes run.
pub struct AgentStores {
    pub tasks: Box<dyn kernel::store::DocumentStore>,
    pub definitions: Box<dyn kernel::store::DocumentStore>,
    pub usage: Box<dyn kernel::store::DocumentStore>,
    pub lanes: Box<dyn FnOnce(Weak<Stack>) -> delegation::agents::LaneRunner>,
    /// Where frame traces are kept (`ultron.rlm.frames`).
    pub frames: Arc<dyn inference::TraceStore>,
}

/// The typed-agent host of a session: its journal, definitions and usage ledger in `stores` (in memory without),
/// the predict adapter over the session's model, `rlm` lanes as `stores` runs them, root turns charged to roots
/// of their own. Returns the host, its ledger and its task-end channel.
#[expect(
    clippy::too_many_arguments,
    reason = "internal wiring helper; parameters match distinct caller inputs"
)]
fn typed_agents(
    config: &Config,
    stores: Option<AgentStores>,
    stack: Weak<Stack>,
    root_model: Arc<Mutex<Model>>,
    keys: crate::request_keys::Keys,
    goal: Arc<std::sync::OnceLock<Arc<GoalDriver>>>,
    refinements: Option<delegation::agents::RefinementsHook>,
    frames: delegation::agents::FrameRunner,
    stats: &Arc<SessionStats>,
) -> (Agents, UsageLedger, Receiver<delegation::agents::TaskEnd>) {
    let (tasks, definitions, usage_store, lanes) = match stores {
        Some(stores) => (
            stores.tasks,
            stores.definitions,
            stores.usage,
            Some(hinted_lanes((stores.lanes)(stack.clone()), {
                let stack = stack.clone();
                Arc::new(move |lane: &str| {
                    if let Some(stack) = stack.upgrade() {
                        stack.cell_hints.run_ended(lane);
                    }
                })
            })),
        ),
        None => (
            Box::new(MemoryDocumentStore::default()) as Box<dyn kernel::store::DocumentStore>,
            Box::new(MemoryDocumentStore::default()) as Box<dyn kernel::store::DocumentStore>,
            Box::new(MemoryDocumentStore::default()) as Box<dyn kernel::store::DocumentStore>,
            None,
        ),
    };
    let var = config.var.clone();
    let root_budget = delegation::usage::root_budget_settings(&config.settings["rootBudget"]);
    let limits = delegation::usage::limits_from_env(&|key| var(key), &root_budget);
    let refused = stats.clone();
    let usage = UsageLedger::new(
        usage_store,
        delegation::usage::UsageOptions {
            limits,
            on_refused: Some(Arc::new(move |_: &str| refused.usage_limit_block())),
            keep_turns: true,
            ..Default::default()
        },
    )
    .unwrap_or_else(|_| {
        UsageLedger::new(Box::new(MemoryDocumentStore::default()), Default::default()).expect("an empty ledger opens")
    });
    let mut options = AgentsOptions::new(tasks);
    options.definition_store = Some(definitions);
    options.usage = Some(usage.clone());
    options.lanes = lanes;
    options.frames = Some(frames);
    options.root_turns = true;
    options.refinements = refinements;
    options.pricing = Some(crate::budget_pricing::pricing(config, root_model.clone()));
    let context_stack = stack.clone();
    options.on_task_end = Some(Arc::new(move |task: &Value, cost, _| {
        if let (Some(goal), Some(id)) = (goal.get(), task["id"].as_str()) {
            let _ = goal.task_ended(id, task.get("result"));
        }
        if let Some(stack) = context_stack.upgrade()
            && let Some(context) = stack.context.get()
        {
            context.task_ended(task, cost);
        }
    }));
    let settings = config.settings_reader();
    options.predict = Some(predict_adapter(Arc::new(move |call: PredictCall<'_>| {
        let model = sync::lock(&root_model).clone();
        let idle = ai::http::http_idle_timeout(settings().settings().get("httpIdleTimeoutMs"));
        let provider = crate::request_keys::Keyed {
            inner: Provider::with_idle_timeout(idle),
            keys: keys.clone(),
        };
        complete_once(&model, &provider, &call)
    })));
    let (pin_stack, unpin_stack) = (stack.clone(), stack.clone());
    options.pin_lane = Some(Arc::new(move |lane: &str, holder: &str| {
        pin_stack.upgrade().is_some_and(|stack| stack.lanes.pin(lane, holder))
    }));
    options.unpin_lane = Some(Arc::new(move |lane: &str, holder: &str| {
        if let Some(stack) = unpin_stack.upgrade() {
            stack.lanes.unpin(lane, holder);
        }
    }));
    let extras_stack = stack.clone();
    options.status_extras = Some(Arc::new(move |lane: &str| match extras_stack.upgrade() {
        Some(stack) => crate::agents::status_extras(&stack.host, stack.tool_calls(), lane),
        None => Map::new(),
    }));
    let reuse_stack = stack;
    options.before_lane_reuse = Some(Arc::new(move |lane: &str| {
        let stack = reuse_stack.upgrade().ok_or("The session is closed")?;
        match stack.lanes.kernel(lane) {
            Some(kernel) => kernel.reset_scratch().map(drop).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }));
    let (agents, ends) = Agents::new(options);
    (agents, usage, ends)
}

/// A typed agent's `rlm` lane runner whose every run's end is reported to `ended` with its lane: the session's cell
/// hints start that lane's stuck-loop detection over (`cellHints.runEnded(lane)` on every lane's `run_end`), so a
/// retained instance's next run does not inherit the last one's failure streak.
pub fn hinted_lanes(
    runner: delegation::agents::LaneRunner,
    ended: Arc<dyn Fn(&str) + Send + Sync>,
) -> delegation::agents::LaneRunner {
    Arc::new(move |run: delegation::agents::LaneRun| {
        let lane = run.lane.clone();
        let result = runner(run);
        ended(&lane);
        result
    })
}

/// One model completion without tools (`completeSimple`), for the predict adapter.
fn complete_once(model: &Model, provider: &dyn StreamFn, call: &PredictCall<'_>) -> Result<Completion, String> {
    let context = ai::TranscriptContext {
        messages: vec![
            Message::System(ai::SystemMessage {
                content: ai::SystemContent::Text(call.system.to_string()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: jsrt::date::now_ms(),
            }),
            Message::User(UserMessage {
                content: UserMessageContent::Text(call.user.clone()),
                timestamp: jsrt::date::now_ms(),
            }),
        ],
    };
    let abort = ai::AbortSignal::new();
    let mut options = ai::SimpleStreamOptions::default();
    options.base.signal = Some(abort.clone());
    let (signal, done) = (call.signal.clone(), Signal::new());
    let watched = done.clone();
    let watcher = std::thread::spawn(move || {
        while !watched.is_aborted() {
            if signal.wait(Duration::from_millis(200)) {
                abort.abort();
                return;
            }
        }
    });
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string());
    let message = runtime.map(|runtime| runtime.block_on(provider.stream(model, &context, &options, &mut sink)));
    done.abort("done");
    let _ = watcher.join();
    let message = message?;
    let text = message
        .content
        .iter()
        .filter_map(|c| match c {
            ai::AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let stop = serde_json::to_value(message.stop_reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(Completion {
        text,
        stop_reason: stop,
        error_message: message.error_message,
    })
}

/// What a subagent runs with: its model, its system prompt (naming its working directory) and thinking level.
pub struct ChildSetup {
    pub model: Model,
    pub system: String,
    pub thinking: String,
}

/// A subagent's lane: in its worktree when it has one (its shell, kernel and prompt name that working directory, as
/// `session-worker.ts` rewrites it), on the session's model unless the spawn named one.
pub fn child_setup(stack: &Stack, run: &ChildRun) -> ChildSetup {
    let model = run
        .model
        .as_deref()
        .and_then(|spec| spec.split_once('/'))
        .and_then(|(provider, id)| stack.config.model(provider, id))
        .unwrap_or_else(|| stack.model.clone());
    let root_cwd = stack.config.cwd.display().to_string();
    let mut system = stack.child_system(&model);
    if let Some(cwd) = run.cwd.as_deref() {
        let worktree_runtime = Runtime {
            config: stack.config.clone(),
            services: stack.services.clone(),
            diagnostics: Vec::new(),
            system: SystemPrompt::fixed(""),
            sessions_dir: stack.sessions_dir.clone(),
            mode: String::new(),
            hindsight: None,
            memorantado: None,
            memory_notice: None,
            memory_explicit: false,
        };
        let worktree_observers: Vec<Arc<dyn host::file_hooks::CellObserver>> = stack
            .memory_pointers
            .clone()
            .into_iter()
            .chain(
                worktree_runtime
                    .situational_skills_in(Path::new(cwd))
                    .map(|observer| observer as Arc<dyn host::file_hooks::CellObserver>),
            )
            .chain(Some(stack.skill_runs.clone() as Arc<dyn host::file_hooks::CellObserver>))
            .collect();
        let host = worktree_runtime.host_in(
            Path::new(cwd),
            &stack.session_id,
            &lane_dir(&run.lane_name),
            Some(&stack.stats),
            worktree_observers,
        );
        match host {
            Ok(host) => {
                let host = host.with_env(run.env.clone()).sharing_jobs(&stack.host);
                sync::lock(&stack.workspaces).insert(
                    run.lane_name.clone(),
                    Workspace {
                        cwd: PathBuf::from(cwd),
                        env: run.env.clone(),
                        host: Arc::new(host),
                    },
                );
                system = system.split(&root_cwd).collect::<Vec<_>>().join(cwd);
            }
            Err(error) => system.push_str(&format!("\n\nThe worktree could not be prepared: {error}")),
        }
    }
    stack.images.set_model(Some(&run.lane_name), model.clone());
    stack.context_reminders.set_limit(
        &run.lane_name,
        crate::context_reminder::lane_limit(&model, true, harness::chat::CompactionPolicy::default().reserve_tokens),
    );
    stack
        .stats
        .child_model(&run.task_id, &format!("{}/{}", model.provider, model.id));
    ChildSetup {
        model,
        system,
        thinking: run.thinking.clone().unwrap_or_else(|| "off".into()),
    }
}

/// A subagent's agent (its lane per [`child_setup`]), the loop that runs it when it is not run durably.
fn child_agent(stack: &Weak<Stack>, defaults: &ChildDefaults, run: &ChildRun) -> (AgentOptions, Box<dyn StreamFn>) {
    let Some(stack) = stack.upgrade() else {
        let mut options = AgentOptions::new(defaults.model.clone());
        options.system_prompt = Some("The session ended before this subagent started.".into());
        let provider = crate::request_keys::Keyed {
            inner: Provider::new(),
            keys: defaults.keys.clone(),
        };
        return (options, Box::new(provider));
    };
    let setup = child_setup(&stack, run);
    let system = setup.system;
    let mut options = AgentOptions::new(setup.model);
    options.system_prompt = Some(system.clone());
    options.thinking_level = setup.thinking;
    match stack.rlm(&run.lane_name) {
        Ok(tool) => options.tools = vec![Arc::new(tool) as Arc<dyn AgentTool>],
        Err(error) => options.system_prompt = Some(format!("{system}\n\nThe REPL failed to start: {error}")),
    }
    let provider = crate::request_keys::Keyed {
        inner: Provider::new().with_live_settings(stack.config.settings_reader()),
        keys: stack.keys.clone(),
    };
    (options, Box::new(provider))
}

/// The model and keys a subagent falls back to.
struct ChildDefaults {
    model: Model,
    keys: crate::request_keys::Keys,
}
