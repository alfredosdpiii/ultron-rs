//! Stack assembly for [`super::Runtime::stack_with`].

use super::*;

type DelegationParts = (
    Option<Box<dyn kernel::store::DocumentStore>>,
    ChildRunner,
    Option<AgentStores>,
);

struct StackStores {
    values: Option<services::SessionValues>,
    config: Arc<Config>,
    session_id: String,
}

impl StackStores {
    fn module(&self, name: &str) -> Arc<dyn kernel::store::DocumentStore> {
        match &self.values {
            Some(values) => Arc::new(crate::session_modules::ModuleDocument(
                values.document(session_journal::ns::MODULE, name),
            )),
            None => Arc::new(FileDocumentStore::new(
                self.config
                    .session_dir("rlm-modules", &self.session_id)
                    .join(format!("{name}.json")),
            )),
        }
    }

    fn jobs(&self) -> Arc<dyn kernel::store::DocumentStore> {
        match &self.values {
            Some(values) => Arc::new(crate::session_modules::ModuleJobJournal(
                crate::session_modules::ModuleDocument(values.document(session_journal::ns::MODULE, "jobs")),
            )),
            None => Arc::new(kernel::store::FileDocumentStore::new(
                self.config
                    .session_dir("rlm-modules", &self.session_id)
                    .join("jobs.json"),
            )),
        }
    }

    fn memory_document(&self) -> Arc<dyn kernel::store::DocumentStore> {
        match &self.values {
            Some(values) => Arc::new(crate::session_modules::ModuleDocument(values.document(
                session_journal::ns::MEMORY_ROOT.namespace,
                session_journal::ns::MEMORY_ROOT.key,
            ))),
            None => self.module("memory"),
        }
    }

    fn local_services(&self) -> Arc<services::LocalServices> {
        Arc::new(services::LocalServices::worker(match &self.values {
            Some(values) => Box::new(values.keyed(session_journal::ns::LOCAL)),
            None => Box::new(kernel_apis::artifacts::FileKeyedDocuments::new(
                self.config
                    .session_dir("rlm-modules", &self.session_id)
                    .join("local.json"),
            )),
        }))
    }

    fn schedule_documents(&self) -> Box<dyn kernel_apis::artifacts::KeyedDocuments> {
        match &self.values {
            Some(values) => Box::new(values.keyed(kernel_apis::prompt_schedule::NAMESPACE)),
            None => Box::new(kernel_apis::artifacts::MemoryKeyedDocuments::default()),
        }
    }
}

struct PreparedApis {
    apis: ApiModules,
    gateway: Arc<mcp::gateway::Gateway>,
    images: Arc<crate::view_images::ViewImages>,
    keys: crate::request_keys::Keys,
    root_model: Arc<Mutex<Model>>,
    judge_ledger: Arc<std::sync::OnceLock<UsageLedger>>,
    judge_rows: Arc<std::sync::OnceLock<services::SessionValues>>,
    skill_checks: Arc<std::sync::OnceLock<delegation::spawn_contract::CheckRunner>>,
    refinements: services::local::InstructionHook,
    ledger: crate::auto_refine::Ledger,
    mined_procedures: Arc<delegation::procedure_miner::ProcedureStore>,
}

struct CyclicParts {
    agents: Agents,
    usage: UsageLedger,
    frames: Arc<inference::FrameSession>,
    schedules: Arc<SchedulesModule>,
    goal: Arc<GoalDriver>,
    delegation: Delegation,
    lanes: Lanes,
    events: Arc<RuntimeEvents>,
}

impl Runtime {
    /// [`Runtime::stack`] whose subagents run as `subagents` says (default: each on its agent loop, with an
    /// in-memory task journal).
    pub fn stack_with(
        &self,
        model: &Model,
        api_key: Option<String>,
        session_id: &str,
        mut subagents: Option<SubagentHost>,
    ) -> Result<Arc<Stack>, String> {
        let memory_children = subagents
            .is_none()
            .then(|| Arc::new(crate::memory_children::MemoryChildren::default()));
        let stores = StackStores {
            values: self.services.values.clone(),
            config: self.config.clone(),
            session_id: session_id.to_string(),
        };
        let stats = Arc::new(SessionStats::system("lane"));
        let memory_records = Arc::new(kernel_apis::memory_records::RecordIndex::for_agent_dir(
            &self.config.agent_dir,
        ));
        let skill_runs = Arc::new(delegation::skill_runs::RunBook::default());
        let memory_pointers = self.memory_pointers(session_id, &memory_records);
        let situational_skills = self.situational_skills_in(&self.config.cwd);
        let host = self.build_session_host(
            session_id,
            &stores,
            &stats,
            &memory_pointers,
            &situational_skills,
            &skill_runs,
        )?;
        crate::guard_log::loki_setup(&self.config);
        let prepared = self.prepare_apis(&stores, model, api_key, session_id, &memory_records, &skill_runs)?;
        let context_reminders = build_context_reminders(&self.config);
        let wake = crate::wake::RootWake::default();
        let cell_hints = build_cell_hints(&self.config, prepared.apis.hints.clone(), &wake);
        let mut built: Result<(), String> = Ok(());
        let stack = self.assemble_stack(
            &stores,
            model,
            &host,
            &stats,
            prepared,
            context_reminders,
            cell_hints,
            wake,
            situational_skills,
            memory_pointers,
            skill_runs,
            &mut subagents,
            &memory_children,
            &mut built,
        );
        built?;
        finalize_stack(&stack, memory_children)?;
        Ok(stack)
    }

    fn build_session_host(
        &self,
        session_id: &str,
        stores: &StackStores,
        stats: &Arc<SessionStats>,
        memory_pointers: &Option<Arc<dyn host::file_hooks::CellObserver>>,
        situational_skills: &Option<Arc<crate::situational_skills::SituationalSkills>>,
        skill_runs: &Arc<delegation::skill_runs::RunBook>,
    ) -> Result<Arc<Host>, String> {
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
        Ok(Arc::new(
            self.host_in(&self.config.cwd, session_id, "bash", Some(stats), observers)?
                .with_journal(stores.jobs())?,
        ))
    }

    fn resolve_memory_backend(&self, session_id: &str) -> Result<Option<Arc<dyn MemoryBackend>>, String> {
        let (cwd, home) = (
            self.config.cwd.display().to_string(),
            self.config.home.display().to_string(),
        );
        match (&self.services.memory, &self.memorantado, &self.hindsight) {
            (Some(backend), _, _) => Ok(Some(backend.clone())),
            (None, Some(endpoint), _) => Ok(Some(Arc::new(
                endpoint
                    .session_backend(session_id, &cwd, &home)
                    .map_err(|e| e.to_string())?,
            ))),
            (None, None, Some(endpoint)) => Ok(Some(Arc::new(
                endpoint
                    .session_backend(session_id, &cwd, &home)
                    .map_err(|e| e.to_string())?,
            ))),
            (None, None, None) => Ok(None),
        }
    }

    fn build_memory_service(
        &self,
        stores: &StackStores,
        session_id: &str,
        memory_records: &Arc<kernel_apis::memory_records::RecordIndex>,
    ) -> Result<Option<MemoryService>, String> {
        let Some(backend) = self.resolve_memory_backend(session_id)? else {
            return Ok(None);
        };
        let service = MemoryService::new(stores.memory_document(), backend, kernel_apis::worker_gate())
            .map_err(|e| format!("memory: {e:?}"))?
            .with_records(memory_records.clone())
            .with_root(self.config.cwd.clone());
        Ok(Some(service))
    }

    fn start_mcp_gateway(&self) -> Arc<mcp::gateway::Gateway> {
        let gateway = self.config.mcp_gateway();
        let notices = gateway.start(self.services.mcp_notices.clone());
        if let Some(notify) = &self.services.mcp_notices {
            for (message, level) in notices {
                notify(&message, &level);
            }
        }
        if let Some(on_mcp) = &self.services.on_mcp {
            on_mcp(&gateway);
        }
        gateway
    }

    fn prepare_apis(
        &self,
        stores: &StackStores,
        model: &Model,
        api_key: Option<String>,
        session_id: &str,
        memory_records: &Arc<kernel_apis::memory_records::RecordIndex>,
        skill_runs: &Arc<delegation::skill_runs::RunBook>,
    ) -> Result<PreparedApis, String> {
        let config = &self.config;
        let memory = self.build_memory_service(stores, session_id, memory_records)?;
        let clock = kernel_apis::util::system_clock();
        let local = stores.local_services();
        let branch = stores.values.as_ref().map(crate::session_modules::main_branch);
        let project_memory = memory.as_ref().map(|m| Arc::new(m.clone()));
        let refinements = services::instruction_hook_with_memory(local.clone(), branch.clone(), project_memory.clone());
        let ledger = crate::auto_refine::Ledger {
            services: local.clone(),
            branch: branch.clone(),
            store: Some(crate::auto_refine::ScopedStore::new(
                config.agent_dir.clone(),
                config.cwd.clone(),
            )),
        };
        let gateway = self.start_mcp_gateway();
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
        if let Some(values) = &stores.values {
            let _ = judge_rows.set(values.clone());
        }
        let judge = Arc::new(crate::session_judge::module(
            config,
            root_model.clone(),
            keys.clone(),
            judge_rows.clone(),
            judge_ledger.clone(),
        ));
        let skills = self.build_skills_module(stores, clock.clone());
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
            now: clock,
        });
        let _ = procedures.interrupt_orphans();
        let apis = self.build_api_modules(
            stores,
            session_id,
            local,
            branch,
            project_memory,
            memory,
            procedures,
            skills,
            &images,
            judge,
            &gateway,
        );
        Ok(PreparedApis {
            apis,
            gateway,
            images,
            keys,
            root_model,
            judge_ledger,
            judge_rows,
            skill_checks,
            refinements,
            ledger,
            mined_procedures,
        })
    }

    fn build_skills_module(&self, stores: &StackStores, clock: kernel_apis::util::Clock) -> Arc<SkillsModule> {
        let skills_config = self.config.clone();
        let no_skills = self.services.session.no_skills;
        let extra = self.services.resources.clone();
        let loader: kernel_apis::SkillLoader = Arc::new(move || Ok(skill_sources(&skills_config, &extra, no_skills)));
        let code = CodeSkills::new(CodeSkillsOptions::for_session(
            &self.config.skills_dir,
            &self.config.runtime_path,
            self.config.child_env(),
        ));
        Arc::new(SkillsModule::new(stores.module("skills"), loader, clock, Some(code)))
    }

    #[expect(clippy::too_many_arguments, reason = "internal stack assembly helper")]
    fn build_api_modules(
        &self,
        stores: &StackStores,
        session_id: &str,
        local: Arc<services::LocalServices>,
        branch: Option<services::BranchSource>,
        project_memory: Option<Arc<MemoryService>>,
        memory: Option<MemoryService>,
        procedures: Arc<delegation::skill_runs::SkillRuns>,
        skills: Arc<SkillsModule>,
        images: &Arc<crate::view_images::ViewImages>,
        judge: Arc<judge::JudgeModule>,
        gateway: &Arc<mcp::gateway::Gateway>,
    ) -> ApiModules {
        let config = &self.config;
        ApiModules {
            local: services::local_module(local, branch, project_memory),
            gates: Arc::new(services::ReleaseGateModule::new(stores.module("release-gates"), None)),
            procedures,
            skills,
            memory: Arc::new(MemoryApi {
                service: memory.map(|memory| memory.with_judge(crate::session_judge::decider(judge.clone()))),
            }),
            hints: Arc::new(HintsModule::new(stores.module("hints"))),
            images: Arc::new(crate::session_images::module(config, session_id)),
            diagram: Arc::new(crate::session_diagrams::module(config, session_id, images.clone())),
            knowledge: Arc::new(crate::session_knowledge::module(
                config,
                self.memorantado.clone(),
                judge.clone(),
            )),
            judge,
            schedule: Arc::new(
                kernel_apis::prompt_schedule::ScheduleModule::new(
                    stores.schedule_documents(),
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
                stores.module("tool-calls"),
            )),
            instances: Arc::new(kernel_apis::InstancesModule::new(stores.module("instances"), None)),
            progress: Arc::new(kernel_apis::ProgressModule::new(stores.module("progress"))),
            family: Arc::new(kernel_apis::FamilyModule::new(
                stores.module("family"),
                kernel_apis::FamilyOptions::default(),
            )),
            sessions: Arc::new(session_search(config, &self.sessions_dir, session_id)),
        }
    }

    #[expect(clippy::too_many_arguments, reason = "internal stack assembly helper")]
    fn assemble_stack(
        &self,
        stores: &StackStores,
        model: &Model,
        host: &Arc<Host>,
        stats: &Arc<SessionStats>,
        prepared: PreparedApis,
        context_reminders: Arc<crate::context_reminder::ContextReminders>,
        cell_hints: Arc<kernel_apis::cell_hints::CellHints>,
        wake: crate::wake::RootWake,
        situational_skills: Option<Arc<crate::situational_skills::SituationalSkills>>,
        memory_pointers: Option<Arc<dyn host::file_hooks::CellObserver>>,
        skill_runs: Arc<delegation::skill_runs::RunBook>,
        subagents: &mut Option<SubagentHost>,
        memory_children: &Option<Arc<crate::memory_children::MemoryChildren>>,
        built: &mut Result<(), String>,
    ) -> Arc<Stack> {
        let config = self.config.clone();
        let session_id = stores.session_id.clone();
        let frame_stream = crate::request_keys::lane_stream(config.settings_reader(), prepared.keys.clone());
        let child = ChildDefaults {
            model: model.clone(),
            keys: prepared.keys.clone(),
        };
        let (schedules_store, goal_store) = (stores.module("schedules"), stores.module("goal"));
        let memory_children_on = memory_children.is_some();
        let agent_lanes = self.services.agent_lanes.clone();
        Arc::new_cyclic(|me: &Weak<Stack>| {
            let cyclic = build_cyclic_parts(
                me,
                &config,
                &session_id,
                host,
                stats,
                &prepared,
                &wake,
                child,
                schedules_store,
                goal_store,
                subagents.take(),
                memory_children,
                &agent_lanes,
                built,
            );
            Stack {
                config,
                mcp: prepared.gateway,
                host: host.clone(),
                delegation: cyclic.delegation,
                apis: prepared.apis,
                model: model.clone(),
                root_model: prepared.root_model,
                keys: prepared.keys,
                frame_stream,
                session_id,
                sessions_dir: self.sessions_dir.clone(),
                system: self.system.clone(),
                services: self.services.clone(),
                lanes: cyclic.lanes,
                workspaces: Mutex::new(HashMap::new()),
                kernel_sandbox: std::sync::OnceLock::new(),
                events: cyclic.events,
                cell_progress: Arc::new(Mutex::new(None)),
                agents: cyclic.agents,
                usage: cyclic.usage,
                frames: cyclic.frames,
                schedules: cyclic.schedules,
                goal: cyclic.goal,
                work: std::sync::OnceLock::new(),
                cell_hints,
                context: std::sync::OnceLock::new(),
                images: prepared.images,
                swarm: std::sync::OnceLock::new(),
                budget_note: Arc::new(std::sync::OnceLock::new()),
                request_layer: Arc::new(std::sync::OnceLock::new()),
                context_reminders,
                situational_skills,
                memory_pointers,
                skill_runs,
                lane_steer: std::sync::OnceLock::new().into(),
                stats: stats.clone(),
                judge_rows: prepared.judge_rows,
                retired_guards: Mutex::new(Vec::new()),
                wake,
                memory_children: memory_children_on,
                ledger: prepared.ledger,
                mined_procedures: prepared.mined_procedures,
            }
        })
    }
}

fn build_context_reminders(config: &Config) -> Arc<crate::context_reminder::ContextReminders> {
    let var = |key: &str| (config.var)(key);
    Arc::new(crate::context_reminder::ContextReminders::new(
        crate::context_reminder::ReminderSettings::from_vars(
            var(CONTEXT_REMINDER_ENV).as_deref(),
            var(CONTEXT_REMINDER_TOKENS_ENV).as_deref(),
        ),
    ))
}

fn build_cell_hints(
    config: &Config,
    hints: Arc<HintsModule>,
    wake: &crate::wake::RootWake,
) -> Arc<kernel_apis::cell_hints::CellHints> {
    let var = |key: &str| (config.var)(key);
    let cell_hints = Arc::new(kernel_apis::cell_hints::CellHints::new(
        hints,
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
    let hints_wake = wake.clone();
    cell_hints.set_lane_woken(Arc::new(move |lane: &str| hints_wake.woken(lane)));
    cell_hints
}

fn resolve_delegation_parts(
    me: &Weak<Stack>,
    subagents: Option<SubagentHost>,
    child: ChildDefaults,
    memory_children: &Option<Arc<crate::memory_children::MemoryChildren>>,
    agent_lanes: &Option<AgentLanes>,
) -> DelegationParts {
    match subagents {
        Some(host) => (Some(host.store), (host.runner)(me.clone()), host.agents),
        None => {
            let runner_stack = me.clone();
            let factory: delegation::child::ChildAgentFactory =
                Arc::new(move |run: &ChildRun| child_agent(&runner_stack, &child, run));
            let runner = match memory_children {
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
    }
}

#[expect(clippy::too_many_arguments, reason = "internal stack assembly helper")]
fn build_cyclic_parts(
    me: &Weak<Stack>,
    config: &Arc<Config>,
    session_id: &str,
    host: &Arc<Host>,
    stats: &Arc<SessionStats>,
    prepared: &PreparedApis,
    wake: &crate::wake::RootWake,
    child: ChildDefaults,
    schedules_store: Arc<dyn kernel::store::DocumentStore>,
    goal_store: Arc<dyn kernel::store::DocumentStore>,
    subagents: Option<SubagentHost>,
    memory_children: &Option<Arc<crate::memory_children::MemoryChildren>>,
    agent_lanes: &Option<AgentLanes>,
    built: &mut Result<(), String>,
) -> CyclicParts {
    let (store, inner, agent_stores) = resolve_delegation_parts(me, subagents, child, memory_children, agent_lanes);
    let goal_slot: Arc<std::sync::OnceLock<Arc<GoalDriver>>> = Arc::new(std::sync::OnceLock::new());
    let frames = inference::FrameSession::new(match &agent_stores {
        Some(stores) => stores.frames.clone(),
        None => Arc::new(inference::MemoryTraceStore::default()),
    });
    let (agents, usage, task_ends) = typed_agents(
        config,
        agent_stores,
        me.clone(),
        prepared.root_model.clone(),
        prepared.keys.clone(),
        goal_slot.clone(),
        Some(prepared.refinements.clone()),
        crate::frames::frame_runner(frames.clone()),
        stats,
    );
    let _ = prepared.judge_ledger.set(usage.clone());
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
    let (delegation, child_ends) = build_delegation(
        me,
        config,
        session_id,
        stats,
        wake,
        store,
        inner,
        &prepared.skill_checks,
    );
    agents.set_worktree_children(delegation.clone());
    let (lanes, cell_ends) = build_lanes(me, config, session_id, stats, built);
    let events = Arc::new(RuntimeEvents::new(
        (task_ends, child_ends, cell_ends),
        config.async_events,
        session_mask(config),
    ));
    CyclicParts {
        agents,
        usage,
        frames,
        schedules,
        goal,
        delegation,
        lanes,
        events,
    }
}

#[expect(clippy::too_many_arguments, reason = "internal stack assembly helper")]
fn build_delegation(
    me: &Weak<Stack>,
    config: &Arc<Config>,
    session_id: &str,
    stats: &Arc<SessionStats>,
    wake: &crate::wake::RootWake,
    store: Option<Box<dyn kernel::store::DocumentStore>>,
    inner: ChildRunner,
    skill_checks: &Arc<std::sync::OnceLock<delegation::spawn_contract::CheckRunner>>,
) -> (Delegation, Receiver<DetachedEnd>) {
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
    options.session_id = session_id.to_string();
    options.workspace = Some(config.cwd.clone());
    options.now = Box::new(|| jsrt::date::now_ms() as f64);
    options.keep_worktrees =
        delegation::worktrees::keep_worktrees((config.var)(delegation::worktrees::KEEP_WORKTREES_ENV).as_deref());
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
    (delegation, child_ends)
}

fn build_lanes(
    me: &Weak<Stack>,
    config: &Arc<Config>,
    session_id: &str,
    stats: &Arc<SessionStats>,
    built: &mut Result<(), String>,
) -> (Lanes, Receiver<CellEnd>) {
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
            Some(lane_snapshot_path(config, session_id)),
        )),
    });
    match lanes {
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
            *built = Err(error);
            Lanes::new(LanesOptions {
                create: Arc::new(|_| Err("no lanes".into())),
                handlers: Arc::new(|_, _, _| Arc::new(Closed) as Arc<dyn HostHandler>),
                max_lanes: 1,
                cancel_grace: Duration::ZERO,
                pool: None,
            })
            .expect("one lane is a valid limit")
        }
    }
}

fn finalize_stack(
    stack: &Arc<Stack>,
    memory_children: Option<Arc<crate::memory_children::MemoryChildren>>,
) -> Result<(), String> {
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
    Ok(())
}
