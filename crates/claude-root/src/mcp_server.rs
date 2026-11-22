//! `ultron mcp` (mcp-server.ts): Ultron's REPL as an MCP server for Claude Code. One server process is one Claude
//! Code session (or one Claude Code subagent, `--child`), and it runs Ultron's own session runtime on an Ultron
//! session of its own: the same host stack, kernel, file hooks (Loki), secret masking, subagents and frames as
//! Ultron's root lane. Claude Code is the root agent; the server exposes:
//!
//! - tool `rlm` (and nothing else): run a Python cell in the persistent kernel ([`ExternalRoot::run_cell`]);
//! - prompt `ultron-guide` and resource `ultron://guide`: the runtime guide (also the server instructions, unless
//!   `--no-instructions`, which `ultron claude` passes because the guide is its system prompt);
//! - a control socket for hooks (`ultron hook`), the viewer (`ultron watch`) and subagent servers.
//!
//! The Ultron session of a Claude Code session is remembered by Claude Code's session id, so `claude --resume`
//! reopens the same session.

use crate::claude_cli::resolve_claude_binary;
use crate::control::{
    ControlClient, ControlHandler, ServerRecord, control_dir, op, socket_path_for, start_control_server,
};
use crate::external_root::ExternalRoot;
use crate::guide::{Audience, GuideOptions, UNLIMITED, claude_runtime_guide_for, claude_system_prompt};
use crate::hooks::HookEvent;
use crate::mcp_protocol::{McpHandlers, McpPrompt, McpResource, serve};
use crate::settings::ChildrenMode;
use crate::{Ctx, UsageError};
use ai::AbortSignal;
use config::providers::CLAUDE_CODE_DEFAULT_MODEL;
use serde_json::{Map, Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services, SubagentHost};
use session_core::lane::Lane;
use session_journal::SessionRepo;
use session_journal::repo::Metadata;
use session_journal::run::MAIN_LANE;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub const CLAUDE_CHILDREN_ENV: &str = "ULTRON_CLAUDE_CHILDREN";

/// How often a `wait-events` waiter looks at the root ([`Server::wait_events`]).
const WAIT_EVENTS_POLL: Duration = Duration::from_millis(250);
/// How long nothing may run and nothing wait before a `wait-events` waiter gives up.
const WAIT_EVENTS_GRACE: Duration = Duration::from_secs(2);

pub fn default_frame_model() -> String {
    crate::claude_code_ref(CLAUDE_CODE_DEFAULT_MODEL)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerArgs {
    pub child: bool,
    pub parent_socket: Option<String>,
    pub parent_name: Option<String>,
    pub parent_lane: Option<String>,
    pub level: u64,
    /// [`UNLIMITED`] for a root server.
    pub allowance: u64,
    pub frame_model: Option<String>,
    pub children: Option<ChildrenMode>,
    pub child_model: Option<String>,
    pub ultron_child_model: Option<String>,
    /// `--root-model`: the Claude Code model the root runs on, as the launcher passed it to `claude --model` (only
    /// when it is not the default, [`config::providers::CLAUDE_CODE_DEFAULT_MODEL`]).
    pub root_model: Option<String>,
    pub socket: Option<String>,
    pub instructions: bool,
    pub no_context_files: bool,
    pub no_skills: bool,
    /// `--wake`: the launcher's wait-events hook wakes the idle root when its work ends (crate::hooks::HookEvent::WaitEvents).
    pub wake: bool,
}

/// Parse `ultron mcp` flags; unknown flags are an error.
pub fn parse_mcp_args(args: &[String]) -> Result<McpServerArgs, UsageError> {
    let mut parsed = McpServerArgs {
        child: false,
        parent_socket: None,
        parent_name: None,
        parent_lane: None,
        level: 0,
        allowance: UNLIMITED,
        frame_model: None,
        children: None,
        child_model: None,
        ultron_child_model: None,
        root_model: None,
        socket: None,
        instructions: true,
        no_context_files: false,
        no_skills: false,
        wake: false,
    };
    let count = |text: &str, flag: &str| -> Result<u64, UsageError> {
        let number = jsrt::number::parse(text);
        if !(jsrt::number::is_safe_integer(number) && number >= 0.0) {
            return Err(UsageError(format!("{flag} must be a non-negative integer")));
        }
        Ok(number as u64)
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut value = || -> Result<String, UsageError> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| UsageError(format!("{arg} needs a value")))
        };
        match arg {
            "--child" => parsed.child = true,
            "--no-instructions" => parsed.instructions = false,
            "--wake" => parsed.wake = true,
            "--parent-socket" => parsed.parent_socket = Some(value()?),
            "--parent-name" => parsed.parent_name = Some(value()?),
            "--parent-lane" => parsed.parent_lane = Some(value()?),
            "--level" => parsed.level = count(&value()?, arg)?,
            "--allowance" => parsed.allowance = count(&value()?, arg)?,
            "--frame-model" => parsed.frame_model = Some(value()?),
            "--children" => {
                let mode = ChildrenMode::parse(&value()?);
                parsed.children = Some(mode.ok_or_else(|| UsageError("--children must be claude or ultron".into()))?);
            }
            "--child-model" => parsed.child_model = Some(value()?),
            "--ultron-child-model" => parsed.ultron_child_model = Some(value()?),
            "--root-model" => parsed.root_model = Some(value()?),
            "--socket" => parsed.socket = Some(value()?),
            "--no-context-files" => parsed.no_context_files = true,
            "--no-skills" => parsed.no_skills = true,
            other => return Err(UsageError(format!("unknown option for ultron mcp: {other}"))),
        }
        index += 1;
    }
    if parsed.child && (parsed.parent_socket.is_none() || parsed.parent_lane.is_none()) {
        return Err(UsageError(
            "ultron mcp --child needs --parent-socket and --parent-lane".into(),
        ));
    }
    Ok(parsed)
}

pub fn split_model(model: &str) -> Option<(String, String)> {
    let slash = model.find('/')?;
    (slash > 0 && slash < model.len() - 1).then(|| (model[..slash].to_string(), model[slash + 1..].to_string()))
}

/// The models and modes a server runs with (env, settings and flags resolved).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub frame_model: String,
    /// The frame model was named (`--frame-model`, ULTRON_CLAUDE_FRAME_MODEL, `claudeCode.frameModel`), not the
    /// default: it beats `rlm.frameModel` and ULTRON_RLM_FRAME_MODEL.
    pub frame_model_explicit: bool,
    pub children: ChildrenMode,
    pub child_model: String,
    /// The Claude children's model was named (`--child-model`, ULTRON_CLAUDE_CHILD_MODEL, `claudeCode.childModel`):
    /// `rlm.childModel` never overrides it.
    pub child_model_explicit: bool,
    pub ultron_child_model: Option<String>,
    /// The Claude Code model the root runs on, as named: `--root-model` (from the launcher), else a Claude Code
    /// subagent's server's `--child-model` (its root is that subagent). [`subscription_model`] makes it a model.
    pub root_model: Option<String>,
}

/// The Claude root's own subscription model as `claude-code/<id>`: the root's model when `known` (the model
/// registry has `claude-code/<id>`), else the default frame model ([`default_frame_model`]). Every Ultron child of a
/// Claude root that names no model runs on it ([`ultron_child_model`]): a `claude-code` request with tools runs on
/// the Claude subscription only (ai::claude_subscription::claude_code_lane), never on API credits.
pub fn subscription_model(root_model: Option<&str>, known: &dyn Fn(&str) -> bool) -> String {
    root_model
        .map(str::trim)
        .map(|model| crate::claude_code_id(model).unwrap_or(model))
        .filter(|id| !id.is_empty() && known(id))
        .map_or_else(default_frame_model, crate::claude_code_ref)
}

/// `rlm.childModel` as written when it names a Claude Code model; under `ultron claude` another provider's model in
/// that setting (a native session's choice) is not used.
pub fn claude_code_setting(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|model| crate::claude_code_id(model).is_some_and(|id| !id.is_empty()))
        .map(str::to_string)
}

/// The model an Ultron child of a Claude root runs on when nothing it was started with names one (an in-memory
/// `--children ultron` subagent, a persistent child, a typed agent's `rlm` lane): `--ultron-child-model` /
/// `claudeCode.ultronChildModel`, else `rlm.childModel` when it names a Claude Code model ([`claude_code_setting`]),
/// else the root's subscription model ([`subscription_model`]). Never the settings' default model: TypeScript falls
/// back to it, but under a Claude root that put children on API credits the user never chose for them.
pub fn ultron_child_model(resolved: &Resolved, rlm_child_model: Option<&str>, subscription: &str) -> String {
    resolved
        .ultron_child_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .or_else(|| claude_code_setting(rlm_child_model))
        .unwrap_or_else(|| subscription.to_string())
}

/// `runMcpServer`'s model and mode resolution. An environment variable that is set wins even when blank (JS `??`).
pub fn resolve(args: &McpServerArgs, var: &dyn Fn(&str) -> Option<String>, cwd: &Path, agent_dir: &Path) -> Resolved {
    let settings = crate::settings::ClaudeCodeSettings::load(cwd, agent_dir);
    let trimmed = |key: &str| var(key).map(|v| v.trim().to_string());
    let named_frame_model = args
        .frame_model
        .clone()
        .or_else(|| trimmed(config::env::CLAUDE_FRAME_MODEL_ENV))
        .or(settings.frame_model.clone());
    let frame_model_explicit = named_frame_model.is_some();
    let frame_model = named_frame_model.unwrap_or_else(default_frame_model);
    let children = args
        .children
        .or_else(|| var(CLAUDE_CHILDREN_ENV).as_deref().and_then(ChildrenMode::parse))
        .or(settings.children)
        .unwrap_or(if resolve_claude_binary(var).is_none() {
            ChildrenMode::Ultron
        } else {
            ChildrenMode::Claude
        });
    let named_child_model = args
        .child_model
        .clone()
        .or_else(|| trimmed(config::env::CLAUDE_CHILD_MODEL_ENV))
        .or(settings.child_model.clone());
    let child_model_explicit = named_child_model.is_some();
    let child_model = named_child_model.unwrap_or_else(|| CLAUDE_CODE_DEFAULT_MODEL.into());
    let ultron_child_model = args.ultron_child_model.clone().or(settings.ultron_child_model);
    Resolved {
        frame_model,
        frame_model_explicit,
        children,
        child_model,
        child_model_explicit,
        ultron_child_model,
        root_model: args
            .root_model
            .clone()
            .or_else(|| args.child_model.clone().filter(|_| args.child)),
    }
}

/// Where the Ultron session of each Claude Code session is remembered.
fn session_map_path(agent_dir: &Path, claude_session_id: &str) -> PathBuf {
    let path = config::paths::AgentPaths::new(agent_dir).claude_session_map(claude_session_id);
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    }
    path
}

fn remembered_session(agent_dir: &Path, claude_session_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(session_map_path(agent_dir, claude_session_id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The metadata JSON `JsonlSessionMetadata` remembers (TypeScript's field order).
fn metadata_json(metadata: &Metadata) -> Value {
    let mut value = Map::new();
    value.insert("id".into(), metadata.id.clone().into());
    value.insert("createdAt".into(), metadata.created_at.into());
    value.insert("storageVersion".into(), metadata.storage_version.into());
    value.insert("cwd".into(), metadata.cwd.clone().into());
    value.insert("path".into(), metadata.path.display().to_string().into());
    value.insert("modifiedAt".into(), json!(metadata.modified_at));
    if let Some(parent) = &metadata.parent_session_id {
        value.insert("parentSessionId".into(), parent.clone().into());
    }
    Value::Object(value)
}

fn metadata_from_json(value: &Value) -> Option<Metadata> {
    Some(Metadata {
        id: value["id"].as_str()?.to_string(),
        created_at: value["createdAt"].as_i64()?,
        storage_version: value["storageVersion"].as_i64().unwrap_or(1),
        cwd: value["cwd"].as_str()?.to_string(),
        path: PathBuf::from(value["path"].as_str()?),
        modified_at: value["modifiedAt"].as_f64().unwrap_or(0.0),
        parent_session_id: value["parentSessionId"].as_str().map(str::to_string),
        legacy_parent_session_path: None,
    })
}

/// A held session lock: `proper-lockfile`'s `<file>.lock` directory, its mtime refreshed every second.
fn lock_session(path: &Path) -> Option<config::lock::LockGuard> {
    let lock_path = config::lock::lock_path_for(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    let options = config::lock::Options {
        stale: Duration::from_secs(2),
        retry: config::lock::Retry {
            until: config::lock::Until::Tries(41),
            ..config::lock::SYNC.retry
        },
        update: Some(Duration::from_secs(1)),
    };
    config::lock::acquire(&lock_path, options).ok()
}

/// The runtime a server's cells run on, once opened.
pub struct Opened {
    pub lane: Arc<Lane>,
    pub root: ExternalRoot,
    /// The root lane's events on their way to `root` (the dispatcher's coalescing window).
    pub sink: Arc<crate::root_sink::RootSink>,
    pub session_id: String,
    /// The session file (`usage.report` reads it).
    pub session_path: PathBuf,
    pub model: String,
    /// The Ultron session (its values keep the persona toggle and keep-going mode, crate::persona_hook).
    pub session: session_core::durable_store::SharedSession,
    /// The durable runtime of a root server's persistent children, swarm, board and notes (crate::durable_apis).
    pub durable: Option<Arc<crate::durable_apis::DurableApis>>,
    /// A root server's timer of scheduled prompts and the root's schedules, kept in the session (crate::schedules).
    timer: Option<Arc<crate::schedules::ScheduleTimer>>,
    pub schedules: Option<Arc<kernel_apis::prompt_schedule::ScheduleModule>>,
    /// Released when the runtime closes, so the next server for the same Claude Code session reopens it.
    lock: Mutex<Option<config::lock::LockGuard>>,
}

impl Opened {
    /// The root has a schedule that will fire while this server runs (a waiter stays for it).
    pub fn schedules_pending(&self) -> bool {
        self.timer.is_some()
            && self
                .schedules
                .as_ref()
                .is_some_and(|schedules| schedules.entries().is_ok_and(|active| !active.is_empty()))
    }

    pub fn close(&self) {
        if let Some(timer) = &self.timer {
            timer.stop();
        }
        if let Some(durable) = &self.durable {
            durable.close();
        }
        self.lane.shutdown();
        jsrt::sync::lock(&self.lock).take();
    }
}

/// Open (or reopen, for a remembered Claude Code session) the Ultron session and its root lane. With `claude` (the
/// `claude` binary, `--children claude`), subagents run as Claude Code processes ([`claude_subagents`]).
fn open_runtime(
    server: &Arc<Server>,
    claude: Option<String>,
    root_finish: Option<delegation::host::RootFinish>,
) -> Result<Opened, String> {
    let (args, resolved) = (&server.args, &server.resolved);
    let env = &frames_env(&server.env, resolved);
    let claude_session_id = server.claude_session_id.as_deref();
    let agent_dir = env.agent_dir();
    let cwd = env.cwd.display().to_string();
    let session_root = config::paths::session_root(
        None,
        &*env.var,
        &config::paths::AgentPaths::new(&agent_dir).experimental_sessions_dir(),
        &env.cwd,
        &env.home,
    );
    let repo = SessionRepo::new(&session_root, session_journal::SystemClock::shared());
    let mut opened: Option<(Metadata, Option<config::lock::LockGuard>, session_journal::Session)> = None;
    if !args.child
        && let Some(id) = claude_session_id
        && let Some(metadata) = remembered_session(&agent_dir, id).as_ref().and_then(metadata_from_json)
        && metadata.cwd == cwd
        && let Ok(session) = repo.open(&metadata)
        && let Some(lock) = lock_session(&metadata.path)
    {
        opened = Some((metadata, Some(lock), session));
    }
    let (metadata, lock, session) = match opened {
        Some(found) => found,
        None => {
            let mut session = repo.create(&cwd, None, None)?;
            let name = if args.child {
                format!("claude subagent ({})", args.parent_lane.as_deref().unwrap_or(""))
            } else {
                "claude code".to_string()
            };
            let _ = session.set_name(Some(&name));
            let metadata = session.metadata().clone();
            if let Some(id) = claude_session_id.filter(|_| !args.child) {
                config::paths::write_private(
                    &session_map_path(&agent_dir, id),
                    &format!("{}\n", jsrt::json::stringify(&metadata_json(&metadata))),
                )
                .map_err(|e| e.to_string())?;
            }
            let lock = lock_session(&metadata.path);
            (metadata, lock, session)
        }
    };
    let session: session_core::durable_store::SharedSession = Arc::new(Mutex::new(session));
    let model = frame_model(&resolved.frame_model, env)?;
    let sessions_dir = session_root.join(session_journal::catalog::session_directory_name(&metadata.cwd));
    let timer = (!args.child).then(crate::schedules::ScheduleTimer::new);
    let schedules = timer.as_ref().map(|timer| {
        crate::schedules::module(
            &session,
            &metadata.id,
            kernel_apis::prompt_schedule::clock_for(&*env.var),
            timer.listener(),
        )
    });
    let children = (!args.child).then(session_core::subagents::DurableChildren::new);
    let typed_model: crate::typed_agents::ChildModel = Arc::default();
    let typed_lanes = children
        .as_ref()
        .map(|children| crate::typed_agents::lanes(children.clone(), typed_model.clone()));
    let services = Services {
        agent_lanes: typed_lanes.clone(),
        ..Services::default()
    };
    let runtime = Runtime::new(env, "print", sessions_dir, services)?;
    for diagnostic in &runtime.diagnostics {
        eprintln!("Warning: {diagnostic}");
    }
    let loki_in_prompt = !args.instructions
        && runtime
            .config
            .loki_root
            .as_deref()
            .and_then(loki::session_note)
            .is_some();
    let loki_notice = session_core::loki_notice::LokiNotice::start(&runtime.config);
    let settings_child = runtime.config.settings["rlm"]["childModel"]
        .as_str()
        .map(str::to_string);
    let subscription = subscription_model(resolved.root_model.as_deref(), &|id| {
        session_core::lane::catalog_model(ai::claude_code::PROVIDER, id, env).is_ok()
    });
    let _ = typed_model.set(if resolved.children == ChildrenMode::Claude {
        let id = resolved.child_model.trim();
        crate::claude_code_ref(crate::claude_code_id(id).unwrap_or(id))
    } else {
        ultron_child_model(resolved, settings_child.as_deref(), &subscription)
    });
    let roots = Arc::new(crate::child_usage::ChildRoots::default());
    let subagents = match claude {
        Some(claude) => Some(claude_subagents(
            server,
            claude,
            &runtime,
            session.clone(),
            roots.clone(),
            typed_lanes,
        )?),
        None => None,
    };
    let lane = Arc::new(Lane::open_with(&runtime, &model, None, &metadata.id, subagents)?);
    lane.stack.wake.set(args.wake && runtime.config.async_events);
    let _ = lane.stack.agents.load_tasks();
    let delegation = &lane.stack.delegation;
    if args.child {
        let allowance =
            (args.allowance != crate::guide::UNLIMITED).then(|| i64::try_from(args.allowance).unwrap_or(i64::MAX));
        delegation.set_root_spawn(u32::try_from(args.level).unwrap_or(u32::MAX), allowance);
    }
    if let Some(finish) = root_finish {
        delegation.set_root_finish(finish);
    }
    if resolved.children == ChildrenMode::Claude {
        let explicit = resolved.child_model_explicit;
        let settings = runtime.config.settings_reader();
        delegation.set_child_model(Arc::new(move || {
            if explicit {
                return None;
            }
            written_child_model(&*settings)
        }));
    } else {
        let named = resolved
            .ultron_child_model
            .is_some()
            .then(|| ultron_child_model(resolved, None, &subscription));
        let (settings, subscription) = (runtime.config.settings_reader(), subscription.clone());
        delegation.set_child_model(Arc::new(move || {
            named
                .clone()
                .or_else(|| Some(written_child_model(&*settings).unwrap_or_else(|| subscription.clone())))
        }));
    }
    let loki_context = if loki_in_prompt {
        None
    } else {
        runtime.config.loki_root.as_deref().and_then(loki::session_note)
    };
    lane.stack.record_judge_rows_in(session.clone());
    lane.stack.stats.set_root("external");
    lane.stack
        .stats
        .attach(Arc::new(session_core::session_stats::SessionStore(session.clone())));
    let sink = Arc::new(crate::root_sink::RootSink::system());
    let durable = match (children, &schedules) {
        (Some(children), Some(schedules)) => {
            let keep_model = crate::durable_apis::keep_model(resolved, settings_child.as_deref(), &subscription);
            let apis = crate::durable_apis::open(children, &session, &lane, &runtime, env, keep_model, sink.clone())?;
            server.world.attach(apis.clone());
            let _ = lane.stack.request_layer.set(crate::unavailable::root_layer(
                apis.clone(),
                server.world.clone(),
                schedules.clone(),
                Arc::downgrade(&lane.stack),
            ));
            Some(apis)
        }
        _ => {
            let _ = lane
                .stack
                .request_layer
                .set(crate::unavailable::child_layer(server.world.clone()));
            None
        }
    };
    if let (Some(timer), Some(schedules)) = (&timer, &schedules) {
        let deliver = sink.clone();
        timer.start(
            schedules.clone(),
            kernel_apis::prompt_schedule::clock_for(&*env.var),
            Arc::new(move |message: &Value| {
                deliver.publish(crate::schedules::prompt_line(message));
                Ok(())
            }),
        );
    }
    let checkpoints = session_core::turn_checkpoints::TurnCheckpoints::of_lane(
        session.clone(),
        env.cwd.clone(),
        &runtime.config.settings,
        &lane,
        (env.var)("PATH").as_deref(),
    );
    let host = Arc::new(crate::external_lane::LaneRootHost::new(lane.stack.clone()).with_checkpoints(checkpoints));
    let spawn_host = Arc::downgrade(&host);
    delegation.on_spawn(Arc::new(move |task: &str, parent: Option<&str>, lane: &str| {
        let turn_root = spawn_host
            .upgrade()
            .and_then(|host| host.current_root())
            .filter(|_| lane == MAIN_LANE);
        roots.spawned(task, parent, turn_root);
    }));
    crate::external_lane::wire(&lane, session.clone(), &host, sink.clone());
    let root = ExternalRoot::over_lane(lane.clone(), loki_context, sink.clone(), host, &runtime.config)
        .with_loki_setup(loki_notice);
    let root = if args.wake { root.waking() } else { root };
    Ok(Opened {
        lane,
        root,
        sink,
        session_path: metadata.path.clone(),
        session_id: metadata.id,
        model: format!("{}/{}", model.provider, model.id),
        session,
        durable,
        timer,
        schedules,
        lock: Mutex::new(lock),
    })
}

/// The global `rlm.childModel` as written when it names a Claude Code model ([`claude_code_setting`]; the native
/// reading drops Claude Code models).
fn written_child_model(settings: &dyn Fn() -> config::settings::SettingsManager) -> Option<String> {
    let settings = settings();
    claude_code_setting(
        settings
            .global_settings()
            .get("rlm")
            .and_then(|rlm| rlm.get("childModel"))
            .and_then(Value::as_str),
    )
}

/// The environment the session runs with: a named frame model (`--frame-model`, ULTRON_CLAUDE_FRAME_MODEL,
/// `claudeCode.frameModel`) is what frames that name no model run on, ahead of `rlm.frameModel` (it takes
/// ULTRON_RLM_FRAME_MODEL's place, which `rlm.models` then reports as the source).
fn frames_env(env: &RunEnv, resolved: &Resolved) -> RunEnv {
    if !resolved.frame_model_explicit {
        return env.clone();
    }
    let (inner, frame) = (env.var.clone(), resolved.frame_model.clone());
    RunEnv {
        var: Arc::new(move |key: &str| {
            if key == config::env::RLM_FRAME_MODEL_ENV {
                Some(frame.clone())
            } else {
                inner(key)
            }
        }),
        ..env.clone()
    }
}

/// The frames' model (also the root lane's): `provider/id` from the catalog (the Claude Code CLI's models included),
/// else the default frame model ([`default_frame_model`], always in the catalog). TypeScript falls back to the
/// settings' default model; under a Claude root that silently moved frames (and the lane's children) onto API
/// credits, so a frame model that does not resolve is reported and the Claude default is used.
fn frame_model(frame: &str, env: &RunEnv) -> Result<ai::Model, String> {
    let resolve = |frame: &str| {
        let (provider, id) = split_model(frame).ok_or_else(|| format!("Model \"{frame}\" not found"))?;
        session_core::lane::catalog_model(&provider, &id, env)
    };
    resolve(frame).or_else(|error| {
        let fallback = default_frame_model();
        eprintln!("Warning: frame model {frame}: {error}; frames run on {fallback}");
        resolve(&fallback)
    })
}

/// A root cell as the viewer shows it.
#[derive(Clone, Debug)]
struct CellRecord {
    n: u64,
    started_at: i64,
    ended_at: Option<i64>,
    code: String,
    status: &'static str,
    output: Option<String>,
}

impl CellRecord {
    fn to_json(&self) -> Value {
        let mut cell = Map::new();
        cell.insert("n".into(), self.n.into());
        cell.insert("startedAt".into(), self.started_at.into());
        cell.insert("code".into(), self.code.clone().into());
        cell.insert("status".into(), self.status.into());
        if let Some(ended) = self.ended_at {
            cell.insert("endedAt".into(), ended.into());
        }
        if let Some(output) = &self.output {
            cell.insert("output".into(), output.clone().into());
        }
        Value::Object(cell)
    }
}

/// The first `n` UTF-16 units of `text` (JS `slice(0, n)`), never splitting a character.
fn js_head(text: &str, n: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= n
        })
        .collect()
}

/// The last `n` UTF-16 units of `text` (JS `slice(-n)`).
fn js_tail(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut tail: Vec<char> = text
        .chars()
        .rev()
        .take_while(|c| {
            units += c.len_utf16();
            units <= n
        })
        .collect();
    tail.reverse();
    tail.into_iter().collect()
}

/// The runtime once it opened, or why it failed.
#[derive(Default)]
struct Startup {
    done: bool,
    opened: Option<Arc<Opened>>,
    error: Option<String>,
}

pub struct Server {
    args: McpServerArgs,
    env: RunEnv,
    name: String,
    record: ServerRecord,
    resolved: Resolved,
    claude_session_id: Option<String>,
    startup: Mutex<Startup>,
    ready: Condvar,
    cells: Mutex<(u64, Vec<CellRecord>)>,
    hints: bool,
    /// Tokens of running Claude Code subagents: a verdict is accepted only from the child holding its token.
    child_tokens: ChildTokens,
    /// The persona's nudges since the user's last prompt (crate::persona_hook).
    persona_nudges: Mutex<u32>,
    /// The latest `wait-events` waiter's number ([`Server::wait_events`]).
    waiters: std::sync::atomic::AtomicU64,
    /// The server is shutting down: waiters return.
    closed: std::sync::atomic::AtomicBool,
    /// The session's persona toggle differs from the launch prompt's and the model was told once (this server).
    persona_told: Mutex<Option<bool>>,
    /// Rigor playbooks already injected into this external-root session (reset on SessionStart).
    shown_playbooks: Mutex<Vec<String>>,
    /// herdr's agents panel (crate::herdr_hook).
    herdr: crate::herdr_hook::HerdrHook,
    /// The kernel APIs the guide names (crate::guide::runtime_apis).
    apis: resources::runtime_guide::RuntimeGuideOptions,
    /// Where this server's subagents reach the session's swarm, board and notes (crate::shared_world).
    world: Arc<crate::shared_world::SharedWorld>,
}

impl Server {
    /// Wait for the runtime; the startup error when it failed.
    fn runtime(&self) -> Result<Arc<Opened>, String> {
        let mut startup = jsrt::sync::lock(&self.startup);
        while !startup.done {
            startup = self.ready.wait(startup).unwrap_or_else(PoisonError::into_inner);
        }
        match (&startup.opened, &startup.error) {
            (Some(opened), _) => Ok(opened.clone()),
            (None, error) => Err(error.clone().unwrap_or_else(|| "the runtime did not start".into())),
        }
    }

    fn opened_now(&self) -> (Option<Arc<Opened>>, Option<String>) {
        let startup = jsrt::sync::lock(&self.startup);
        (startup.opened.clone(), startup.error.clone())
    }

    fn status(&self) -> Value {
        let (opened, error) = self.opened_now();
        let mut status = Map::new();
        status.insert("name".into(), self.name.clone().into());
        status.insert("pid".into(), std::process::id().into());
        status.insert("cwd".into(), self.record.cwd.clone().into());
        status.insert("child".into(), self.args.child.into());
        status.insert("startedAt".into(), self.record.started_at.into());
        status.insert("ready".into(), opened.is_some().into());
        if let Some(error) = error {
            status.insert("error".into(), error.into());
        }
        if let Some(id) = &self.claude_session_id {
            status.insert("claudeSessionId".into(), id.clone().into());
        }
        if let Some(opened) = opened {
            status.insert("sessionId".into(), opened.session_id.clone().into());
            status.insert("frameModel".into(), opened.model.clone().into());
            status.insert("children".into(), self.resolved.children.as_str().into());
            status.insert("turn".into(), opened.root.turn().map_or(Value::Null, Value::from));
            status.insert("cells".into(), opened.root.cells().into());
            status.insert("pendingEvents".into(), opened.root.pending_events().into());
        }
        Value::Object(status)
    }

    pub fn control(&self, request: Map<String, Value>) -> Result<Value, String> {
        let requested = request.get("op").and_then(Value::as_str).unwrap_or("");
        match requested {
            op::STATUS => Ok(self.status()),
            op::CELLS => {
                let cells = jsrt::sync::lock(&self.cells);
                let start = cells.1.len().saturating_sub(50);
                Ok(json!({"cells": cells.1[start..].iter().map(CellRecord::to_json).collect::<Vec<_>>()}))
            }
            op::INSPECT => {
                let inspect = match request.get("request") {
                    None | Some(Value::Null) => String::new(),
                    other => jsrt::value::string(other),
                };
                if !session_core::inspect::VIEWER_INSPECTIONS.contains(&inspect.as_str()) {
                    return Err(format!("not a read-only inspection: {inspect}"));
                }
                let payload = match request.get("payload") {
                    Some(Value::Object(payload)) => payload.clone(),
                    _ => Map::new(),
                };
                let opened = self.runtime()?;
                let stack = &opened.lane.stack;
                match inspect.as_str() {
                    "rlm.pool" => Ok(stack.pool_stats()),
                    "async.pending" => Ok(crate::external_lane::async_pending(stack, &opened.sink)),
                    "usage.report" => {
                        stack.stats.flush();
                        usage::session_report(
                            &opened.session_path,
                            &session_core::lane::subscription_providers(&self.env),
                        )
                    }
                    _ => stack
                        .cell_handler(MAIN_LANE, None)
                        .handle(&inspect, payload, &kernel::Signal::new()),
                }
            }
            op::HOOK => {
                let event = match request.get("event") {
                    None | Some(Value::Null) => String::new(),
                    other => jsrt::value::string(other),
                };
                let event = HookEvent::parse(&event).ok_or_else(|| format!("unknown hook event: {event}"))?;
                let input = match request.get("input") {
                    Some(Value::Object(input)) => input.clone(),
                    _ => Map::new(),
                };
                self.hook(event, &input)
            }
            op::CHILD_FINISH => {
                let lane = request
                    .get("token")
                    .and_then(Value::as_str)
                    .and_then(|token| jsrt::sync::lock(&self.child_tokens).get(token).cloned())
                    .ok_or("rlm.finish: unknown subagent")?;
                let payload = match request.get("payload") {
                    Some(Value::Object(payload)) => payload.clone(),
                    _ => Map::new(),
                };
                let opened = self.runtime()?;
                opened
                    .lane
                    .stack
                    .cell_handler(&lane, None)
                    .handle("rlm.finish", payload, &kernel::Signal::new())
            }
            op::SHARED => {
                let lane = request
                    .get("token")
                    .and_then(Value::as_str)
                    .and_then(|token| jsrt::sync::lock(&self.child_tokens).get(token).cloned())
                    .ok_or("the session's shared world: unknown subagent")?;
                let kind = request.get("kind").and_then(Value::as_str).unwrap_or_default();
                let payload = match request.get("payload") {
                    Some(Value::Object(payload)) => payload.clone(),
                    _ => Map::new(),
                };
                self.runtime()?;
                let name = match request.get("name").and_then(Value::as_str) {
                    Some(name) => name.to_string(),
                    None => self.world.name_of_lane(&lane),
                };
                self.world.serve(Some(&name), kind, &payload)
            }
            op::WAIT_EVENTS => {
                let opened = self.runtime()?;
                Ok(self.wait_events(&opened))
            }
            "persona" | "keepgoing" => {
                let args = request.get("args").and_then(Value::as_str).unwrap_or("").to_string();
                let opened = self.runtime()?;
                let global = self.persona_on();
                let text = self.persona_change(&opened, |state, now, limits| {
                    state.command(requested, &args, now, global, limits)
                })?;
                Ok(json!({"text": text}))
            }
            other => Err(format!("unknown control op: {other}")),
        }
    }

    /// Wait until the root's turn has ended with events waiting (`{"events": text}`), or until nothing it started
    /// is running and no event waits, or a newer waiter takes over (`{}`). One waiter at a time: every rlm call starts
    /// one (PostToolUse), and only the latest may wake the model.
    fn wait_events(&self, opened: &Opened) -> Value {
        let root = &opened.root;
        let me = self.waiters.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let mut idle_since: Option<Instant> = None;
        loop {
            if self.waiters.load(std::sync::atomic::Ordering::SeqCst) != me
                || self.closed.load(std::sync::atomic::Ordering::SeqCst)
            {
                return json!({});
            }
            let events = root.pending_events() > 0;
            if events
                && root.turn().is_none()
                && let Some(text) = root.take_events()
            {
                return json!({"events": text});
            }
            let pending = crate::external_lane::async_pending(&opened.lane.stack, &opened.sink)["pending"] == true
                || opened.schedules_pending();
            if !events && !pending {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= WAIT_EVENTS_GRACE {
                    return json!({});
                }
            } else {
                idle_since = None;
            }
            std::thread::sleep(WAIT_EVENTS_POLL);
        }
    }

    /// Claude Code hook events: the hook command forwards its input here and prints what this returns.
    fn hook(&self, event: HookEvent, input: &Map<String, Value>) -> Result<Value, String> {
        if let Some(answer) = self.herdr.before(event, input) {
            return Ok(answer);
        }
        let opened = self.runtime()?;
        let root = &opened.root;
        let context = |name: &str, context: Option<String>| match context {
            Some(text) => json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": text}}),
            None => json!({}),
        };
        match event {
            HookEvent::SessionStart => {
                jsrt::sync::lock(&self.shown_playbooks).clear();
                let mut parts: Vec<String> = root.session_context().into_iter().collect();
                parts.extend(self.session_skills_context());
                Ok(context(
                    event.claude_event(),
                    (!parts.is_empty()).then(|| parts.join("\n\n")),
                ))
            }
            HookEvent::UserPrompt => {
                let prompt = input.get("prompt").and_then(Value::as_str).unwrap_or("");
                if !self.args.child {
                    let global = self.persona_on();
                    let answer = self.persona_change(&opened, |state, now, limits| {
                        crate::persona_hook::prompt_toggle(state, prompt, now, global, limits)
                    });
                    if let Some(answer) = answer {
                        return Ok(answer);
                    }
                }
                *jsrt::sync::lock(&self.persona_nudges) = 0;
                let mut contexts: Vec<String> = root.begin_turn().into_iter().collect();
                let mut notice = None;
                if !self.args.child {
                    let (message, start) = self.persona_change(&opened, |state, now, _| {
                        crate::persona_hook::prompt_input(state, prompt, now)
                    });
                    notice = message;
                    contexts.extend(self.persona_context(&opened));
                    contexts.extend(start);
                    contexts.extend(self.routed_playbook(prompt));
                }
                let mut answer = context(
                    event.claude_event(),
                    (!contexts.is_empty()).then(|| contexts.join("\n\n")),
                );
                if let Some(notice) = notice {
                    answer["systemMessage"] = Value::String(notice);
                }
                Ok(answer)
            }
            HookEvent::Stop => {
                root.end_turn();
                if self.args.child {
                    return Ok(json!({}));
                }
                let mut nudges = jsrt::sync::lock(&self.persona_nudges);
                let global = self.persona_on();
                let transcript = crate::persona_hook::transcript_of(input);
                let answer = self.persona_change(&opened, |state, now, limits| {
                    crate::persona_hook::stop_answer_in(state, &mut nudges, global, &transcript, now, limits)
                });
                self.herdr.stopped(input, &answer);
                Ok(answer)
            }
            HookEvent::Notification | HookEvent::SessionEnd | HookEvent::WaitEvents => {
                Err(format!("unknown hook event: {}", event.command()))
            }
        }
    }

    /// In engineering mode, route `prompt` to its fitting rigor playbook once per session (native parity).
    fn routed_playbook(&self, prompt: &str) -> Option<String> {
        session_core::playbook_route::route(prompt)?;
        let settings =
            config::settings::SettingsManager::for_worker(&self.env.cwd, &self.env.agent_dir(), &self.env.home);
        if settings.settings().get("engineering") != Some(&Value::Bool(true)) {
            return None;
        }
        let bundled = session_core::session_resources::bundled_skills(&self.env.skills(), &*self.env.var)?;
        let rigor = PathBuf::from(bundled).join("pstack/rigor");
        let mut shown = jsrt::sync::lock(&self.shown_playbooks);
        let message =
            session_core::playbook_route::playbook_message(prompt, &rigor, &shown, jsrt::date::now_ms() as u64)?;
        if let Some(name) = message["details"]["playbook"].as_str() {
            shown.push(name.to_string());
        }
        message["content"].as_str().map(str::to_string)
    }

    /// For external roots that do not receive `--system-prompt` (e.g. `ultron agy`, where `args.instructions` is
    /// true), deliver engineering-mode `alwaysSkills` and the bundled-skills index via `SessionStart`.
    fn session_skills_context(&self) -> Option<String> {
        if !self.args.instructions || self.args.child || self.args.no_skills {
            return None;
        }
        let bundled =
            session_core::session_resources::bundled_skills(&self.env.skills(), &*self.env.var).map(PathBuf::from);
        let resources = crate::guide::load_prompt_resources(
            &self.env,
            bundled.as_deref(),
            crate::guide::ResourceFlags {
                no_context_files: true,
                no_skills: false,
            },
        );
        if !resources.engineering {
            return None;
        }
        let mut parts = Vec::new();
        let always = resources::skills::format_always_skills_for_prompt(&resources.skills);
        if !always.is_empty() {
            parts.push(always);
        }
        let bundled_str = bundled.as_deref().map(|p| p.display().to_string());
        let note = resources::skills::bundled_skills_note(
            &resources.skills,
            bundled_str.as_deref(),
            resources.proactive_skills,
        );
        if !note.is_empty() {
            parts.push(note);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Change the session's persona state (the session value `ultron.module/persona`) with `change`, written back
    /// when it changed.
    fn persona_change<T>(
        &self,
        opened: &Opened,
        change: impl FnOnce(&mut resources::keep_going::SessionPersona, i64, resources::keep_going::Limits) -> T,
    ) -> T {
        use resources::keep_going::{STATE_KEY, SessionPersona};
        use session_journal::ns::MODULE;
        let mut session = session_core::durable_store::lock(&opened.session);
        let mut state = SessionPersona::from_json(session.value(MODULE, STATE_KEY));
        let before = state.clone();
        let limits = resources::keep_going::Limits::from_env(&*self.env.var);
        let out = change(&mut state, jsrt::date::now_ms(), limits);
        if state != before {
            let _ = session.commit(&[session_journal::Write::set(MODULE, STATE_KEY, state.to_json())]);
        }
        out
    }

    /// When the session's toggle differs from the persona the launch prompt has (the settings'), the model is told
    /// once per change: the persona's section when it is on, that the section is off when it is off.
    fn persona_context(&self, opened: &Opened) -> Option<String> {
        use resources::keep_going::{STATE_KEY, SessionPersona};
        use session_journal::ns::MODULE;
        let state =
            SessionPersona::from_json(session_core::durable_store::lock(&opened.session).value(MODULE, STATE_KEY));
        let launched = self.persona_on();
        let wanted = state.effective(launched);
        let mut told = jsrt::sync::lock(&self.persona_told);
        if wanted == launched {
            *told = None;
            return None;
        }
        if *told == Some(wanted) {
            return None;
        }
        *told = Some(wanted);
        if wanted {
            let personality = resources::persona::read_personality(&self.env.agent_dir())
                .ok()
                .flatten()?;
            Some(format!(
                "<persona>\n{}\n</persona>",
                resources::persona::section_text(&personality)
            ))
        } else {
            Some("The persona is off for this session: ignore the <persona> section of your system prompt.".into())
        }
    }

    /// The root's settings name the persona (read per Stop; a subagent never has it).
    fn persona_on(&self) -> bool {
        if self.args.child {
            return false;
        }
        let agent_dir = self.env.agent_dir();
        let trusted = config::trust::worker_project_trusted(&self.env.cwd, &agent_dir, &self.env.home);
        let settings = config::settings::SettingsManager::create(&self.env.cwd, &agent_dir, trusted);
        resources::persona::enabled(settings.settings())
    }

    fn guide_text(&self) -> String {
        if self.args.child {
            claude_system_prompt(
                Audience::ClaudeChild,
                &GuideOptions {
                    allowance: Some(self.args.allowance),
                    apis: Some(&self.apis),
                    ..GuideOptions::default()
                },
                self.hints,
            )
        } else {
            claude_runtime_guide_for(Some(&self.apis), self.hints, self.args.wake)
        }
    }

    fn tool_description(&self) -> String {
        let config = session_core::compose::Config::from_env(&self.env);
        let listing = kernel_apis::CodeSkills::new(kernel_apis::CodeSkillsOptions::new(
            &config.skills_dir,
            &config.runtime_path,
        ))
        .describe();
        let section = if listing.is_empty() {
            String::new()
        } else {
            format!("\n{listing}")
        };
        format!("{}{section}", crate::guide::rlm_tool_description())
    }

    fn run_cell(&self, code: &str, signal: &AbortSignal) -> Value {
        let opened = match self.runtime() {
            Ok(opened) => opened,
            Err(error) => {
                return json!({"content": [{"type": "text", "text": format!("Ultron's runtime failed to start: {error}")}], "isError": true});
            }
        };
        let n = {
            let mut cells = jsrt::sync::lock(&self.cells);
            cells.0 += 1;
            let n = cells.0;
            cells.1.push(CellRecord {
                n,
                started_at: jsrt::date::now_ms(),
                ended_at: None,
                code: js_head(code, 400),
                status: "running",
                output: None,
            });
            let excess = cells.1.len().saturating_sub(200);
            cells.1.drain(..excess);
            n
        };
        let result = opened.root.run_cell(code, signal);
        if let Some(cell) = jsrt::sync::lock(&self.cells).1.iter_mut().find(|c| c.n == n) {
            cell.ended_at = Some(jsrt::date::now_ms());
            cell.status = if result.is_error { "error" } else { "ok" };
            cell.output = result
                .content
                .first()
                .and_then(|p| p["text"].as_str())
                .map(|t| js_tail(t, 400));
        }
        let mut reply = Map::new();
        reply.insert("content".into(), Value::Array(result.content));
        if result.is_error {
            reply.insert("isError".into(), true.into());
        }
        Value::Object(reply)
    }
}

/// The MCP face of a [`Server`].
pub struct Handlers(Arc<Server>);

impl McpHandlers for Handlers {
    fn name(&self) -> String {
        "ultron".into()
    }
    fn version(&self) -> String {
        config::version().into()
    }
    fn instructions(&self) -> Option<String> {
        self.0
            .args
            .instructions
            .then(|| claude_runtime_guide_for(Some(&self.0.apis), self.0.hints, self.0.args.wake))
    }
    fn tools(&self) -> Vec<Value> {
        let mut input_schema = session_core::lane::rlm_tool().parameters;
        input_schema["additionalProperties"] = json!(false);
        vec![json!({
            "name": "rlm",
            "title": "Ultron REPL",
            "description": self.0.tool_description(),
            "inputSchema": input_schema,
        })]
    }
    fn call_tool(
        &self,
        _name: &str,
        args: Map<String, Value>,
        signal: &AbortSignal,
        _meta: Map<String, Value>,
    ) -> Result<Value, String> {
        Ok(match args.get("code") {
            Some(Value::String(code)) => self.0.run_cell(code, signal),
            _ => json!({"content": [{"type": "text", "text": "rlm needs `code` (a string)"}], "isError": true}),
        })
    }
    fn prompts(&self) -> Option<Vec<McpPrompt>> {
        Some(vec![McpPrompt {
            name: "ultron-guide".into(),
            title: Some("Ultron runtime guide".into()),
            description: "How to work in Ultron's REPL".into(),
            text: self.0.guide_text(),
        }])
    }
    fn resources(&self) -> Option<Vec<McpResource>> {
        Some(vec![McpResource {
            uri: "ultron://guide".into(),
            name: "guide".into(),
            title: Some("Ultron runtime guide".into()),
            description: "How to work in Ultron's REPL (the rlm tool)".into(),
            mime_type: "text/markdown".into(),
            text: self.0.guide_text(),
        }])
    }
}

/// What a running server holds: shut down once, by Claude Code disconnecting or by a signal.
pub struct Running {
    pub server: Arc<Server>,
    control: Mutex<Option<crate::control::ControlServer>>,
    parent: Arc<Mutex<Option<ControlClient>>>,
    done: std::sync::atomic::AtomicBool,
}

impl Running {
    /// Shut down like a disconnect (TypeScript's end of `runMcpServer`): stop the control server and unregister,
    /// drop the parent connection, and close the runtime once it opened (kernels are snapshotted, running subagents
    /// and jobs stop). A second call does nothing.
    pub fn shutdown(&self) {
        if self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.server.herdr.release();
        self.server.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(control) = jsrt::sync::lock(&self.control).take() {
            control.close();
        }
        jsrt::sync::lock(&self.parent).take();
        if let Ok(opened) = self.server.runtime() {
            opened.close();
        }
    }

    /// How many `wait-events` waiters have started ([`Server::wait_events`]).
    pub fn event_waiters(&self) -> u64 {
        self.server.waiters.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn socket(&self) -> String {
        self.server.record.socket.clone()
    }

    /// The runtime once it opened (waits for it), or why it failed.
    pub fn opened(&self) -> Result<Arc<Opened>, String> {
        self.server.runtime()
    }
}

/// Run `ultron mcp` on stdio until Claude Code disconnects; returns the exit code. SIGTERM, SIGINT and SIGHUP of
/// this process shut it down the same way and exit 0.
pub fn run(
    argv: &[String],
    ctx: &Ctx,
    stdin: &mut dyn std::io::BufRead,
    stdout: Box<dyn std::io::Write + Send>,
) -> Result<i32, String> {
    let (running, handlers) = start(argv, ctx)?;
    let signalled = running.clone();
    signals::set_handler(Arc::new(move |_| {
        signalled.shutdown();
        std::process::exit(0);
    }));
    serve(handlers, stdin, stdout);
    running.shutdown();
    signals::clear_handler();
    Ok(0)
}

/// Start a server: its runtime opening in the background, its control socket listening. [`serve`] the returned
/// handlers on the MCP connection, then [`Running::shutdown`].
pub fn start(argv: &[String], ctx: &Ctx) -> Result<(Arc<Running>, Arc<Handlers>), String> {
    let args = parse_mcp_args(argv).map_err(|e| e.0)?;
    let mut env = ctx.env.clone();
    let parent_token = (ctx.env.var)(host::PARENT_TOKEN_ENV);
    let var = env.var.clone();
    if args.child
        && let Some(dir) = var(crate::child::CHILD_CWD_ENV)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    {
        std::env::set_current_dir(&dir).map_err(|e| format!("{dir}: {e}"))?;
        env.cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    }
    let cwd = env.cwd.display().to_string();
    let agent_dir = env.agent_dir();
    let claude_session_id = var("CLAUDE_CODE_SESSION_ID")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let claude_pid = var("CLAUDE_PID")
        .map(|v| jsrt::number::parse(&v))
        .filter(|n| *n != 0.0 && n.is_finite() && n.fract() == 0.0 && *n > 0.0)
        .map(|n| n as u64);
    let resolved = resolve(&args, &*var, &env.cwd, &agent_dir);
    let name = if args.child {
        format!("child-{}-{}", std::process::id(), jsrt::random::random_hex(3))
    } else {
        claude_session_id.clone().unwrap_or_else(|| {
            format!(
                "mcp-{}-{}",
                std::process::id(),
                crate::launcher::base36(jsrt::date::now_ms() as u64)
            )
        })
    };
    let dir = control_dir(&*var)?;
    let socket = args
        .socket
        .clone()
        .unwrap_or_else(|| socket_path_for(&name, &dir).display().to_string());
    let record = ServerRecord {
        name: name.clone(),
        pid: std::process::id(),
        socket,
        cwd,
        started_at: jsrt::date::now_ms(),
        claude_session_id: claude_session_id.clone(),
        claude_pid,
        session_id: None,
        parent: args.parent_name.clone().filter(|_| args.child),
    };
    let parent = Arc::new(Mutex::new(if args.child {
        Some(ControlClient::connect(
            Path::new(args.parent_socket.as_deref().unwrap_or_default()),
            Duration::from_secs(10),
        )?)
    } else {
        None
    }));
    let world = if args.child {
        crate::shared_world::SharedWorld::forwarding(upstream(parent.clone(), parent_token.clone()))
    } else {
        crate::shared_world::SharedWorld::root()
    };
    let root_finish = args.child.then(|| root_finish(parent.clone(), parent_token));
    let server = Arc::new(Server {
        hints: session_core::compose::hints(&*var),
        args: args.clone(),
        env: env.clone(),
        name,
        record: record.clone(),
        resolved: resolved.clone(),
        claude_session_id,
        startup: Mutex::default(),
        ready: Condvar::new(),
        cells: Mutex::default(),
        child_tokens: ChildTokens::default(),
        persona_nudges: Mutex::new(0),
        waiters: Default::default(),
        closed: Default::default(),
        persona_told: Mutex::new(None),
        shown_playbooks: Mutex::new(Vec::new()),
        herdr: crate::herdr_hook::HerdrHook::start(&env, args.child),
        apis: crate::guide::runtime_apis(&env),
        world,
    });
    let claude = resolve_claude_binary(&*var).filter(|_| resolved.children == ChildrenMode::Claude);
    let starter = server.clone();
    std::thread::spawn(move || {
        let outcome = open_runtime(&starter, claude, root_finish);
        let mut startup = jsrt::sync::lock(&starter.startup);
        match outcome {
            Ok(opened) => startup.opened = Some(Arc::new(opened)),
            Err(error) => {
                eprintln!("ultron mcp: runtime failed to start: {error}");
                startup.error = Some(error);
            }
        }
        startup.done = true;
        starter.ready.notify_all();
    });
    let handler_server = server.clone();
    let handler: ControlHandler = Arc::new(move |request| handler_server.control(request));
    let control = start_control_server(&record, handler, &dir)?;
    let handlers = Arc::new(Handlers(server.clone()));
    let running = Arc::new(Running {
        server,
        control: Mutex::new(Some(control)),
        parent,
        done: std::sync::atomic::AtomicBool::new(false),
    });
    Ok((running, handlers))
}

/// `rootFinish`: the verdict goes to the parent server as `child.finish` with this server's token.
fn root_finish(parent: Arc<Mutex<Option<ControlClient>>>, token: Option<String>) -> delegation::host::RootFinish {
    Arc::new(move |payload: Map<String, Value>| {
        let mut parent = jsrt::sync::lock(&parent);
        let client = parent
            .as_mut()
            .ok_or("rlm.finish: the parent server is not reachable")?;
        let mut fields = Map::new();
        if let Some(token) = &token {
            fields.insert("token".into(), token.clone().into());
        }
        fields.insert("payload".into(), Value::Object(payload));
        client.request(op::CHILD_FINISH, fields, Duration::from_secs(30))
    })
}

/// A subagent server's shared-world requests go to its parent server with its token (crate::shared_world).
fn upstream(parent: Arc<Mutex<Option<ControlClient>>>, token: Option<String>) -> crate::shared_world::Upstream {
    Arc::new(move |name: Option<&str>, kind: &str, payload: &Map<String, Value>| {
        let mut parent = jsrt::sync::lock(&parent);
        let client = parent
            .as_mut()
            .ok_or_else(|| format!("{kind}: the parent server is not reachable"))?;
        let mut fields = Map::new();
        if let Some(token) = &token {
            fields.insert("token".into(), token.clone().into());
        }
        if let Some(name) = name {
            fields.insert("name".into(), name.into());
        }
        fields.insert("kind".into(), kind.into());
        fields.insert("payload".into(), Value::Object(payload.clone()));
        client.request(op::SHARED, fields, Duration::from_secs(60))
    })
}

/// Child tokens of running Claude Code subagents, by token: their lane.
type ChildTokens = Arc<Mutex<std::collections::HashMap<String, String>>>;

/// The subagent host of a `--children claude` server: each child runs as `claude -p` on its own `ultron mcp
/// --child` server (child.ts `runClaudeChild`), its verdict relayed back over this server's control socket
/// (`child.finish`, checked against its one-time token), its usage charged to the root its spawn belongs to
/// (crate::child_usage). The task journal, the typed agents' records and the usage ledger are values of `session`.
fn claude_subagents(
    server: &Arc<Server>,
    claude: String,
    runtime: &Runtime,
    session: session_core::durable_store::SharedSession,
    roots: Arc<crate::child_usage::ChildRoots>,
    typed_lanes: Option<session_core::compose::AgentLanes>,
) -> Result<SubagentHost, String> {
    use session_core::subagents::{SessionUsageStore, value_store};
    use session_journal::ns::{self, DEFINITIONS_ROOT, TASKS_ROOT};
    let var = server.env.var.clone();
    let self_cmd = crate::self_cmd::self_command(&*var)?;
    let settings = crate::settings::ClaudeCodeSettings::load(&server.env.cwd, &server.env.agent_dir());
    let skip = crate::child::child_skip_permissions(&*var, settings.skip_permissions);
    let flags = crate::guide::ResourceFlags {
        no_context_files: server.args.no_context_files,
        no_skills: server.args.no_skills,
    };
    let ctx = crate::Ctx {
        env: server.env.clone(),
    };
    let bundled = ctx.bundled_skills();
    let resources = crate::guide::load_prompt_resources(&ctx.env, bundled.as_deref(), flags);
    let apis = server.apis.clone();
    let loki_root = runtime.config.loki_root.clone();
    let server = server.clone();
    let value = |namespace: &str, key: &str| value_store(&session, namespace, key);
    Ok(SubagentHost {
        store: value(TASKS_ROOT.namespace, TASKS_ROOT.key),
        runner: Box::new(move |stack: std::sync::Weak<session_core::compose::Stack>| {
            Arc::new(move |run: delegation::host::ChildRun| {
                let cwd = run.cwd.clone().unwrap_or_else(|| server.env.cwd.display().to_string());
                let resources = crate::guide::PromptResources {
                    loki: loki_root.as_deref().and_then(loki::session_note),
                    ..resources.clone()
                };
                let system_prompt = claude_system_prompt(
                    Audience::ClaudeChild,
                    &GuideOptions {
                        cwd: Some(cwd.clone()),
                        platform: Some(jsrt::process::platform().into()),
                        date: Some(crate::guide::utc_date()),
                        allowance: Some(u64::from(run.allowance)),
                        resources: Some(&resources),
                        bundled_skills: bundled.as_deref(),
                        wake: false,
                        apis: Some(&apis),
                    },
                    server.hints,
                );
                let model = crate::child::child_claude_model(run.model.as_deref(), &server.resolved.child_model)?;
                let stack = stack.upgrade().ok_or("The session is closed")?;
                let charge = crate::child_usage::Charge {
                    usage: &stack.usage,
                    stats: &stack.stats,
                    root: roots.root_of(&run.task_id),
                    task_id: &run.task_id,
                    timeout_ms: run.timeout_ms,
                    model: crate::claude_code_ref(&model),
                };
                let token = jsrt::random::random_hex(24);
                jsrt::sync::lock(&server.child_tokens).insert(token.clone(), run.lane_name.clone());
                let options = crate::child::ChildOptions {
                    claude: &claude,
                    server: crate::child::ChildServer {
                        self_cmd: &self_cmd,
                        parent_socket: &server.record.socket,
                        parent_name: &server.name,
                        frame_model: Some(&server.resolved.frame_model),
                        no_context_files: flags.no_context_files,
                        no_skills: flags.no_skills,
                    },
                    cwd: &cwd,
                    model: &server.resolved.child_model,
                    system_prompt,
                    skip_permissions: skip.clone(),
                    var: &*var,
                };
                let child = crate::child::ChildRun {
                    lane_name: run.lane_name.clone(),
                    prompt: run.prompt.clone(),
                    level: u64::from(run.level),
                    allowance: u64::from(run.allowance),
                    model: Some(model),
                    cwd: run.cwd.clone(),
                    env: run.env.clone(),
                };
                let result = crate::child_usage::charged(&charge, &|| run.signal.is_aborted(), || {
                    crate::child::run_claude_child(&options, &child, &token, &run.signal, &|_| {})
                });
                jsrt::sync::lock(&server.child_tokens).remove(&token);
                result.map(|done| done.text)
            }) as delegation::host::ChildRunner
        }),
        agents: Some(session_core::compose::AgentStores {
            tasks: value(ns::TASKS, "agents"),
            definitions: value(DEFINITIONS_ROOT.namespace, DEFINITIONS_ROOT.key),
            usage: Box::new(SessionUsageStore::new(session.clone())),
            lanes: match typed_lanes {
                Some(lanes) => Box::new(move |stack| lanes(stack)),
                None => Box::new(|_| {
                    Arc::new(|_| {
                        Err("Ultron rlm definitions run on the root's durable runtime in ultron claude, which a Claude Code subagent's server has not; ask the root to run it".to_string())
                    }) as delegation::agents::LaneRunner
                }),
            },
            frames: Arc::new(session_core::frames::SessionTraceStore(session.clone())),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn env(root: &std::path::Path) -> RunEnv {
        let agent = root.join("agent");
        let vars = [
            (config::paths::ENV_AGENT_DIR.to_string(), agent.display().to_string()),
            (config::env::OFFLINE_ENV.to_string(), "1".to_string()),
        ];
        RunEnv {
            cwd: root.to_path_buf(),
            home: root.to_path_buf(),
            var: Arc::new(move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
            package: root.to_path_buf(),
        }
    }

    #[test]
    fn claude_root_frame_model_names_the_claude_code_providers_own_models() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        for id in ["claude-opus-5-5", "claude-haiku-5-5", "opus", "sonnet", "haiku"] {
            let model = frame_model(&format!("claude-code/{id}"), &env).unwrap();
            assert_eq!((model.provider.as_str(), model.id.as_str()), ("claude-code", id));
        }
        let sonnet = frame_model("claude-code/sonnet", &env).unwrap();
        assert_eq!(
            (
                sonnet.name.as_str(),
                sonnet.api.as_str(),
                sonnet.context_window,
                sonnet.max_tokens
            ),
            ("Claude Sonnet (Claude Code CLI)", "claude-code-cli", 1_000_000, 128_000)
        );
        std::fs::create_dir_all(tmp.path().join("agent")).unwrap();
        std::fs::write(
            tmp.path().join("agent/settings.json"),
            r#"{"defaultProvider": "groq", "defaultModel": "llama-3.3-70b-versatile"}"#,
        )
        .unwrap();
        for unknown in ["claude-code/claude-sonnet-9", "groq/no-such-model", "nonsense"] {
            let model = frame_model(unknown, &env).unwrap();
            assert_eq!(
                (model.provider.as_str(), model.id.as_str()),
                ("claude-code", "claude-opus-5-5"),
                "{unknown}"
            );
        }
    }
}
