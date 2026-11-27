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

mod agents_wiring;
mod host_dispatch;
mod kernel_factory;
mod stack_builder;
#[path = "config.rs"]
mod stack_config;
mod system_prompt;

pub use agents_wiring::{AgentStores, ChildSetup, SubagentHost, child_setup, hinted_lanes};
use agents_wiring::{ChildDefaults, child_agent, typed_agents};
pub use host_dispatch::{CELL_CANCELLED_SHELL, RuntimeEvents, only_rlm, session_mask, task_event_summary};
use host_dispatch::{CellShell, FramesOfCell, ObservedByHints, Router, add_guard_stats};
use kernel_factory::{lane_dir, lane_snapshot_path, max_live_kernels};
pub use stack_config::{
    Config, TURN_GUARDS_ENV, hints, mcp_runtime_guide, session_environment, ts_guide, turn_guards,
    workspace_snapshotter,
};
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
            config::keys::engineering(config::settings::SettingsManager::for_worker(&cwd, &agent_dir, &home).settings())
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
