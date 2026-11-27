//! Session opening and lane-configuration resolution for [`super::Core`].

use super::*;

/// [`super::lane_config`], and whether the model the stored lane names is unavailable (then the model is a stand-in
/// of that identity, see [`unavailable_model`]).
pub(super) fn lane_config_resolving(
    flags: &ModelFlags,
    env: &RunEnv,
    session: Option<&mut Session>,
) -> Result<(Value, Model, bool), String> {
    let stored = session
        .as_ref()
        .and_then(|s| s.value(session_journal::ns::LANE_CONFIG, "main").cloned());
    let (wanted, flag_model) = desired_lane_config(flags, env, stored)?;
    let config = match session {
        Some(session) => apply_session_lane_config(session, flags, env, &wanted, flag_model.as_ref().ok())?,
        None => wanted,
    };
    resolve_configured_model(config, flag_model, env)
}

fn desired_lane_config(
    flags: &ModelFlags,
    env: &RunEnv,
    stored: Option<Value>,
) -> Result<(Value, Result<Model, String>), String> {
    let choice = match (&flags.model, &stored) {
        (None, Some(_)) => Err("No model selected: pass --model <provider>/<id>".to_string()),
        _ => lane::resolve_choice(flags, env, stored.is_some()),
    };
    let thinking = match (&flags.thinking, &choice) {
        (Some(level), _) => level.clone(),
        (None, Ok(choice)) => choice
            .thinking
            .clone()
            .unwrap_or_else(|| lane::initial_thinking(&choice.model, env)),
        (None, Err(_)) => "medium".into(),
    };
    let flag_model = choice.map(|choice| choice.model);
    let wanted = match (&flag_model, stored) {
        (Ok(m), _) => run::lane_config(&m.provider, &m.id, &thinking, &["rlm"]),
        (Err(_), Some(mut stored)) => {
            stored["activeToolNames"] = json!(["rlm"]);
            stored
        }
        (Err(e), None) => return Err(e.clone()),
    };
    Ok((wanted, flag_model))
}

fn apply_session_lane_config(
    session: &mut Session,
    flags: &ModelFlags,
    env: &RunEnv,
    wanted: &Value,
    flag_model: Option<&Model>,
) -> Result<Value, String> {
    let mut config = run::ensure_lane(session, wanted)?;
    if let Some(level) = flags.thinking.as_ref().filter(|_| flags.model.is_none())
        && config["thinkingLevel"] != json!(level)
    {
        config["thinkingLevel"] = json!(level);
        session.commit(&[Write::set(LANE_CONFIG, MAIN_LANE, config.clone())])?;
    }
    if claude_root_requested(&*env.var)
        && let Some(m) = flag_model
        && config["model"]["provider"] != json!(m.provider)
    {
        config["model"] = json!({"provider": m.provider, "modelId": m.id});
        session.commit(&[Write::set(LANE_CONFIG, MAIN_LANE, config.clone())])?;
    }
    Ok(config)
}

fn resolve_configured_model(
    config: Value,
    flag_model: Result<Model, String>,
    env: &RunEnv,
) -> Result<(Value, Model, bool), String> {
    match (
        config["model"]["provider"].as_str(),
        config["model"]["modelId"].as_str(),
        flag_model,
    ) {
        (Some(provider), Some(id), Ok(m)) if (provider, id) == (m.provider.as_str(), m.id.as_str()) => {
            Ok((config, m, false))
        }
        (Some(provider), Some(id), _) => match lane::catalog_model(provider, id, env) {
            Ok(model) => Ok((config, model, false)),
            Err(_) => Ok((config.clone(), unavailable_model(provider, id), true)),
        },
        (_, _, m) => Ok((config, m?, false)),
    }
}

impl Core {
    /// Open `options.metadata`'s session with its lane. `tx` is the actor's own input (runs report through it).
    pub fn open(options: OpenOptions, tx: Sender<Input>, sink: EventSink) -> Result<Core, String> {
        let OpenOptions {
            env,
            flags,
            session_root,
            metadata,
            mode,
            services,
            auto_name,
        } = options;
        let repo = SessionRepo::new(&session_root, SystemClock::shared());
        let mut session = repo.open(&metadata)?;
        crate::format_version::assert_session_formats_readable(&session)?;
        let (recovered, resume) = recover_or_prepare_resume(&mut session)?;
        let sessions_dir = session_root.join(session_journal::catalog::session_directory_name(&metadata.cwd));
        trace_startup(&env, "worker.harness");
        let mut runtime = Runtime::new(&env, &mode, sessions_dir, services)?;
        trace_startup(&env, "worker.resources-loaded");
        let loki_notice = crate::loki_notice::LokiNotice::start(&runtime.config);
        let (config, model, model_unavailable) = lane_config_resolving(&flags, &env, Some(&mut session))?;
        let keys = crate::request_keys::Keys::session(flags.api_key.clone(), &model.provider, &env);
        let announced = session.state().entries().len();
        let announced_usage = session.state().usage_rows().len();
        let session_id = session.id().to_string();
        let session: SharedSession = Arc::new(Mutex::new(session));
        let values = crate::session_modules::session_values(&session);
        runtime.services.values = Some(values.clone());
        let children = crate::subagents::DurableChildren::new();
        let unfinished = crate::subagents::unfinished(&session);
        trace_startup(&env, "worker.harness-create");
        let lane = Lane::open_with(
            &runtime,
            &model,
            flags.api_key,
            &session_id,
            Some(children.host(values)),
        )?;
        trace_startup(&env, "worker.host");
        let (branches, child_context, context) = wire_context_control(&lane, &session, &children, &env);
        let provider = Arc::new(Provider::new().with_live_settings(runtime.config.settings_reader()));
        let queues = build_initial_queues(&recovered);
        let durable = durable_turns::open_durable(
            &session,
            &tx,
            &lane,
            provider.clone(),
            &env,
            &keys,
            queues.clone(),
            children.clone(),
            child_context,
            runtime.config.context_images,
        )?;
        wire_lane_hooks(&lane, &durable, &children, &unfinished)?;
        let events = install_background_loops(&lane, &env, &tx, &durable, children, &session);
        let mut core = assemble_core(
            model_unavailable,
            loki_notice,
            &config,
            provider,
            env,
            keys,
            repo,
            runtime,
            model,
            session,
            lane,
            tx,
            sink,
            queues,
            announced,
            announced_usage,
            auto_name,
            recovered,
            durable,
            events,
            context,
            branches,
        );
        finish_open(&mut core, resume)?;
        Ok(core)
    }
}

fn trace_startup(env: &RunEnv, label: &str) {
    crate::startup_trace::trace_startup((env.var)(crate::startup_trace::TRACE_ENV).as_deref(), label);
}

fn recover_or_prepare_resume(session: &mut Session) -> Result<(crate::recovery::Recovered, Option<String>), String> {
    let open_operation = session
        .value(LANE_STATE, MAIN_LANE)
        .and_then(|state| state["currentOperationId"].as_str().map(str::to_string));
    let durable_records = crate::durable_store::has_records(session)
        && open_operation.as_ref().is_none_or(|operation| {
            session
                .value(session_journal::ns::OP_META, operation)
                .is_none_or(|meta| meta["intent"]["kind"].as_str().is_none_or(|kind| kind == "run"))
        });
    let recovered = match &open_operation {
        Some(_) if !durable_records => crate::recovery::settle(session)?,
        _ => crate::recovery::Recovered {
            queued: crate::recovery::inbox(session),
            ..Default::default()
        },
    };
    Ok((recovered, open_operation.filter(|_| durable_records)))
}

fn wire_context_control(
    lane: &Lane,
    session: &SharedSession,
    children: &Arc<crate::subagents::DurableChildren>,
    env: &RunEnv,
) -> (
    Arc<context_lane::RootBranches>,
    Arc<context_children::ChildContext>,
    Arc<kernel_apis::ContextControl>,
) {
    lane.stack
        .stats
        .attach(Arc::new(crate::session_stats::SessionStore(session.clone())));
    let branches = Arc::new(context_lane::RootBranches::new(session.clone()));
    let child_context = Arc::new(context_children::ChildContext::new(children.clone(), session.clone()));
    let context = Arc::new(
        kernel_apis::ContextControl::new(Arc::new(context_children::LaneBranches {
            root: branches.clone(),
            children: child_context.clone(),
        }))
        .with_trim(!env.flag_off(kernel_apis::context_control::COMPACTION_TRIM_ENV)),
    );
    child_context.observe(&context);
    let _ = lane.stack.context.set(context.clone());
    (branches, child_context, context)
}

fn build_initial_queues(recovered: &crate::recovery::Recovered) -> Arc<Mutex<Queues>> {
    Arc::new(Mutex::new(Queues {
        items: recovered
            .queued
            .iter()
            .filter_map(|q| {
                Some(Queued {
                    kind: QueueKind::parse(&q.kind)?,
                    message: q.message.clone(),
                    entry_id: q.entry_id.clone(),
                    turn: 0,
                    submission: None,
                })
            })
            .collect(),
        delivered: Vec::new(),
        steering_all: config::keys::QUEUE_DEFAULT == config::keys::QUEUE_ALL,
        follow_up_all: config::keys::QUEUE_DEFAULT == config::keys::QUEUE_ALL,
    }))
}

fn wire_lane_hooks(
    lane: &Lane,
    durable: &durable_turns::Durable,
    children: &Arc<crate::subagents::DurableChildren>,
    unfinished: &[String],
) -> Result<(), String> {
    children.attach(durable.chat.clone());
    let steer_children = children.clone();
    let _ = lane
        .stack
        .lane_steer
        .set(Arc::new(move |child_lane: &str, message: &str| {
            steer_child_lane(&steer_children, child_lane, message)
        }));
    let _ = lane.stack.budget_note.set(crate::budget_note::budget_note(
        children.clone(),
        lane.stack.agents.clone(),
        lane.stack.usage.clone(),
    ));
    let meter_children = children.clone();
    lane.stack.context_reminders.set_meter(Arc::new(move |lane_name: &str| {
        measure_lane_context(&meter_children, lane_name)
    }));
    let _ = lane.stack.swarm.set(Arc::new(
        crate::swarm_ops::SwarmOps::new(children.clone()).with_contracts(Arc::downgrade(&lane.stack)),
    ));
    lane.stack.delegation.set_fork_point(children.fork_point_hook());
    let transcripts = durable.chat.clone();
    lane.stack.delegation.set_transcripts(Arc::new(move |lane: &str| {
        crate::agent_hub::child_transcript(&transcripts, lane).map(|(messages, _)| messages)
    }));
    children.restore(&lane.stack, unfinished).map(drop)
}

fn steer_child_lane(children: &crate::subagents::DurableChildren, child_lane: &str, message: &str) -> bool {
    let (Some(chat), Some(conversation)) = (children.chat(), children.conversation_of_lane(child_lane)) else {
        return false;
    };
    if chat.live(conversation).map_or(true, |live| live["run"].is_null()) {
        return false;
    }
    chat.conversation(conversation).ok().flatten().is_some_and(|handle| {
        handle
            .submit(harness::chat::SubmissionDraft::Input {
                content: json!(message),
                when_busy: harness::chat::WhenBusy::Steer,
                request_id: None,
            })
            .is_ok()
    })
}

fn measure_lane_context(
    children: &crate::subagents::DurableChildren,
    lane_name: &str,
) -> Option<crate::context_reminder::Measure> {
    let chat = children.chat()?;
    let conversation = if lane_name == MAIN_LANE {
        durable::ROOT_CONVERSATION_ID
    } else {
        children.conversation_of_lane(lane_name)?
    };
    let view = chat.conversation(conversation).ok().flatten()?.context(None).ok()?;
    Some(crate::context_reminder::Measure {
        tokens: harness::chat::compaction::estimate_context(&view, &[]),
        epoch: view.head.as_ref().map(|head| head.id.0),
    })
}

fn install_background_loops(
    lane: &Lane,
    env: &RunEnv,
    tx: &Sender<Input>,
    durable: &durable_turns::Durable,
    children: Arc<crate::subagents::DurableChildren>,
    session: &SharedSession,
) -> Arc<crate::async_events::Dispatcher> {
    let events = event_lanes::install(
        lane,
        env,
        tx.clone(),
        durable.chat.clone(),
        children,
        durable.stop.clone(),
        session.clone(),
    );
    event_lanes::tick_activity(tx.clone(), durable.stop.clone());
    let sweep_stop = durable.stop.clone();
    lane.stack
        .lanes
        .sweep_every(lanes::IDLE_SWEEP_INTERVAL, move |interval| {
            sweep_stop.wait_timeout(interval).is_some()
        });
    events
}

#[expect(clippy::too_many_arguments, reason = "internal core assembly helper")]
fn assemble_core(
    model_unavailable: bool,
    loki_notice: crate::loki_notice::LokiNotice,
    config: &Value,
    provider: Arc<Provider>,
    env: RunEnv,
    keys: crate::request_keys::Keys,
    repo: SessionRepo,
    runtime: Runtime,
    model: Model,
    session: SharedSession,
    lane: Lane,
    tx: Sender<Input>,
    sink: EventSink,
    queues: Arc<Mutex<Queues>>,
    announced: usize,
    announced_usage: usize,
    auto_name: bool,
    recovered: crate::recovery::Recovered,
    durable: durable_turns::Durable,
    events: Arc<crate::async_events::Dispatcher>,
    context: Arc<kernel_apis::ContextControl>,
    branches: Arc<context_lane::RootBranches>,
) -> Core {
    let auto_compaction = runtime.config.settings["compaction"]["enabled"] != false;
    let nudger = crate::nudge::ToolRoundNudger::new(runtime.config.tool_rounds_nudge, runtime.config.async_events);
    let skill_nudger = crate::nudge::SkillExtractionNudger::new(runtime.config.skill_nudge);
    let auto_memory = crate::auto_memory::AutoMemory::new(&runtime.config.settings, runtime.memory_explicit);
    let auto_refine =
        crate::auto_refine::AutoRefine::new(&runtime.config.settings, (env.var)(AUTO_REFINE_ENV).as_deref());
    let observer = observer::Observer::new(runtime.config.settings["observer"]["enabled"].as_bool().unwrap_or(true));
    Core {
        model_unavailable,
        loki_notice,
        memory_notice: runtime.memory_notice.clone(),
        auto_memory,
        auto_refine,
        thinking: config["thinkingLevel"].as_str().unwrap_or("medium").to_string(),
        models: lane::available_models(&env),
        provider,
        env,
        keys,
        repo,
        runtime,
        model,
        session,
        lane,
        tx,
        sink,
        run: None,
        queues,
        auto_compaction,
        auto_retry: true,
        bash: None,
        bug_summary: None,
        announced_usage,
        announced,
        taken: Vec::new(),
        auto_name,
        naming: None,
        recovered: Some(recovered),
        durable,
        resumed: None,
        events,
        context,
        branches,
        inputs: None,
        deferred: std::collections::VecDeque::new(),
        compacting: None,
        navigating: false,
        after_run: Vec::new(),
        nudger,
        skill_nudger,
        persona: Default::default(),
        persona_session: Default::default(),
        observer,
        turn_guard: Default::default(),
    }
}

fn finish_open(core: &mut Core, resume: Option<String>) -> Result<(), String> {
    core.persona_load();
    core.turn_guard_load();
    if let Some(operation) = resume {
        core.resumed = Some(core.reopen_run(&operation)?);
    } else {
        core.branches.place(true)?;
    }
    core.durable.chat.resume().map_err(|e| e.to_string())?;
    let forked = lock_session(&core.session)
        .value(SESSION_START.0, SESSION_START.1)
        .is_some();
    if forked {
        lock_session(&core.session).commit(&[Write::delete(SESSION_START.0, SESSION_START.1)])?;
    }
    Ok(())
}
