use super::*;

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
pub(super) fn typed_agents(
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
    let root_budget = delegation::usage::root_budget_settings(config::keys::root_budget(&config.settings));
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
pub(super) fn child_agent(
    stack: &Weak<Stack>,
    defaults: &ChildDefaults,
    run: &ChildRun,
) -> (AgentOptions, Box<dyn StreamFn>) {
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
pub(super) struct ChildDefaults {
    pub(super) model: Model,
    pub(super) keys: crate::request_keys::Keys,
}
