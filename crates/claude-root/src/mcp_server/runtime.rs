use super::Server;
use super::args::{
    McpServerArgs, Resolved, claude_code_setting, default_frame_model, split_model, subscription_model,
    ultron_child_model,
};
use crate::external_root::ExternalRoot;
use crate::guide::{Audience, GuideOptions, claude_system_prompt};
use crate::settings::ChildrenMode;
use serde_json::{Map, Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services, SubagentHost};
use session_core::lane::Lane;
use session_journal::SessionRepo;
use session_journal::repo::Metadata;
use session_journal::run::MAIN_LANE;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn try_reopen_session(
    repo: &SessionRepo,
    agent_dir: &Path,
    cwd: &str,
    claude_session_id: Option<&str>,
) -> Option<(Metadata, Option<config::lock::LockGuard>, session_journal::Session)> {
    let id = claude_session_id?;
    let metadata = remembered_session(agent_dir, id)
        .as_ref()
        .and_then(metadata_from_json)?;
    if metadata.cwd != cwd {
        return None;
    }
    let session = repo.open(&metadata).ok()?;
    let lock = lock_session(&metadata.path)?;
    Some((metadata, Some(lock), session))
}

fn open_or_create_session(
    repo: &SessionRepo,
    args: &McpServerArgs,
    agent_dir: &Path,
    cwd: &str,
    claude_session_id: Option<&str>,
) -> Result<(Metadata, Option<config::lock::LockGuard>, session_journal::Session), String> {
    if !args.child
        && let Some(reopened) = try_reopen_session(repo, agent_dir, cwd, claude_session_id)
    {
        return Ok(reopened);
    }
    let mut session = repo.create(cwd, None, None)?;
    let name = if args.child {
        format!("claude subagent ({})", args.parent_lane.as_deref().unwrap_or(""))
    } else {
        "claude code".to_string()
    };
    let _ = session.set_name(Some(&name));
    let metadata = session.metadata().clone();
    if let Some(id) = claude_session_id.filter(|_| !args.child) {
        config::paths::write_private(
            &session_map_path(agent_dir, id),
            &format!("{}\n", jsrt::json::stringify(&metadata_json(&metadata))),
        )
        .map_err(|e| e.to_string())?;
    }
    let lock = lock_session(&metadata.path);
    Ok((metadata, lock, session))
}

fn configure_child_model(
    delegation: &delegation::host::Delegation,
    resolved: &Resolved,
    runtime: &Runtime,
    subscription: &str,
) {
    let settings = runtime.config.settings_reader();
    if resolved.children == ChildrenMode::Claude {
        let explicit = resolved.child_model_explicit;
        delegation.set_child_model(Arc::new(move || {
            if explicit {
                None
            } else {
                written_child_model(&*settings)
            }
        }));
    } else {
        let named = resolved
            .ultron_child_model
            .is_some()
            .then(|| ultron_child_model(resolved, None, subscription));
        let subscription = subscription.to_string();
        delegation.set_child_model(Arc::new(move || {
            named
                .clone()
                .or_else(|| Some(written_child_model(&*settings).unwrap_or_else(|| subscription.clone())))
        }));
    }
}

fn configure_delegation(
    lane: &Arc<Lane>,
    args: &McpServerArgs,
    resolved: &Resolved,
    runtime: &Runtime,
    subscription: &str,
    root_finish: Option<delegation::host::RootFinish>,
) {
    let delegation = &lane.stack.delegation;
    if args.child {
        let allowance =
            (args.allowance != crate::guide::UNLIMITED).then(|| i64::try_from(args.allowance).unwrap_or(i64::MAX));
        delegation.set_root_spawn(u32::try_from(args.level).unwrap_or(u32::MAX), allowance);
    }
    if let Some(finish) = root_finish {
        delegation.set_root_finish(finish);
    }
    configure_child_model(delegation, resolved, runtime, subscription);
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal runtime wiring helper taking distinct components"
)]
fn wire_durable_and_schedules(
    server: &Arc<Server>,
    children: Option<Arc<session_core::subagents::DurableChildren>>,
    timer: &Option<Arc<crate::schedules::ScheduleTimer>>,
    schedules: &Option<Arc<kernel_apis::prompt_schedule::ScheduleModule>>,
    session: &session_core::durable_store::SharedSession,
    lane: &Arc<Lane>,
    runtime: &Runtime,
    env: &RunEnv,
    settings_child: Option<&str>,
    subscription: &str,
    sink: &Arc<crate::root_sink::RootSink>,
) -> Result<Option<Arc<crate::durable_apis::DurableApis>>, String> {
    let durable = match (children, schedules) {
        (Some(children), Some(schedules)) => {
            let keep_model = crate::durable_apis::keep_model(&server.resolved, settings_child, subscription);
            let apis = crate::durable_apis::open(children, session, lane, runtime, env, keep_model, sink.clone())?;
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
    if let (Some(timer), Some(schedules)) = (timer, schedules) {
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
    Ok(durable)
}

fn loki_session_context(args: &McpServerArgs, runtime: &Runtime) -> Option<String> {
    let note = runtime.config.loki_root.as_deref().and_then(loki::session_note);
    if !args.instructions && note.is_some() {
        None
    } else {
        note
    }
}

fn resolve_typed_model(resolved: &Resolved, settings_child: Option<&str>, subscription: &str) -> String {
    if resolved.children == ChildrenMode::Claude {
        let id = resolved.child_model.trim();
        crate::claude_code_ref(crate::claude_code_id(id).unwrap_or(id))
    } else {
        ultron_child_model(resolved, settings_child, subscription)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal runtime wiring helper taking distinct components"
)]
fn build_external_root(
    args: &McpServerArgs,
    env: &RunEnv,
    runtime: &Runtime,
    lane: &Arc<Lane>,
    session: &session_core::durable_store::SharedSession,
    sink: &Arc<crate::root_sink::RootSink>,
    roots: Arc<crate::child_usage::ChildRoots>,
    loki_notice: session_core::loki_notice::LokiNotice,
) -> ExternalRoot {
    let checkpoints = session_core::turn_checkpoints::TurnCheckpoints::of_lane(
        session.clone(),
        env.cwd.clone(),
        &runtime.config.settings,
        lane,
        (env.var)("PATH").as_deref(),
    );
    let host = Arc::new(crate::external_lane::LaneRootHost::new(lane.stack.clone()).with_checkpoints(checkpoints));
    let spawn_host = Arc::downgrade(&host);
    lane.stack
        .delegation
        .on_spawn(Arc::new(move |task: &str, parent: Option<&str>, lane: &str| {
            let turn_root = spawn_host
                .upgrade()
                .and_then(|host| host.current_root())
                .filter(|_| lane == MAIN_LANE);
            roots.spawned(task, parent, turn_root);
        }));
    crate::external_lane::wire(lane, session.clone(), &host, sink.clone());
    let root = ExternalRoot::over_lane(
        lane.clone(),
        loki_session_context(args, runtime),
        sink.clone(),
        host,
        &runtime.config,
    )
    .with_loki_setup(loki_notice);
    if args.wake { root.waking() } else { root }
}

/// Open (or reopen, for a remembered Claude Code session) the Ultron session and its root lane. With `claude` (the
/// `claude` binary, `--children claude`), subagents run as Claude Code processes ([`claude_subagents`]).
pub(super) fn open_runtime(
    server: &Arc<Server>,
    claude: Option<String>,
    root_finish: Option<delegation::host::RootFinish>,
) -> Result<Opened, String> {
    let (args, resolved) = (&server.args, &server.resolved);
    let env = &frames_env(&server.env, resolved);
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
    let (metadata, lock, session) =
        open_or_create_session(&repo, args, &agent_dir, &cwd, server.claude_session_id.as_deref())?;
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
    let runtime = Runtime::new(
        env,
        "print",
        sessions_dir,
        Services {
            agent_lanes: typed_lanes.clone(),
            ..Services::default()
        },
    )?;
    for diagnostic in &runtime.diagnostics {
        eprintln!("Warning: {diagnostic}");
    }
    let loki_notice = session_core::loki_notice::LokiNotice::start(&runtime.config);
    let settings_child = runtime.config.settings["rlm"]["childModel"]
        .as_str()
        .map(str::to_string);
    let subscription = subscription_model(resolved.root_model.as_deref(), &|id| {
        session_core::lane::catalog_model(ai::claude_code::PROVIDER, id, env).is_ok()
    });
    let _ = typed_model.set(resolve_typed_model(resolved, settings_child.as_deref(), &subscription));
    let roots = Arc::new(crate::child_usage::ChildRoots::default());
    let subagents = claude
        .map(|claude| claude_subagents(server, claude, &runtime, session.clone(), roots.clone(), typed_lanes))
        .transpose()?;
    let lane = Arc::new(Lane::open_with(&runtime, &model, None, &metadata.id, subagents)?);
    lane.stack.wake.set(args.wake && runtime.config.async_events);
    let _ = lane.stack.agents.load_tasks();
    configure_delegation(&lane, args, resolved, &runtime, &subscription, root_finish);
    lane.stack.record_judge_rows_in(session.clone());
    lane.stack.stats.set_root("external");
    lane.stack
        .stats
        .attach(Arc::new(session_core::session_stats::SessionStore(session.clone())));
    let sink = Arc::new(crate::root_sink::RootSink::system());
    let durable = wire_durable_and_schedules(
        server,
        children,
        &timer,
        &schedules,
        &session,
        &lane,
        &runtime,
        env,
        settings_child.as_deref(),
        &subscription,
        &sink,
    )?;
    let root = build_external_root(args, env, &runtime, &lane, &session, &sink, roots, loki_notice);
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
pub(super) fn frame_model(frame: &str, env: &RunEnv) -> Result<ai::Model, String> {
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
