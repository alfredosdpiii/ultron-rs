use super::*;

/// Lane names as file names (`laneDir` of session-worker.ts).
pub(super) fn lane_dir(lane: &str) -> String {
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
pub(super) fn max_live_kernels(settings: &Value) -> usize {
    config::keys::max_live_kernels(settings)
}

/// Where each lane's kernel snapshot lives (the path its kernel restores at startup): the pool snapshots an idle
/// kernel there before evicting it.
pub(super) fn lane_snapshot_path(config: &Arc<Config>, session_id: &str) -> lanes::SnapshotPathFn {
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

impl Stack {
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
    pub(super) fn retire_workspace(&self, lane: &str) {
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
    pub(super) fn kernel(&self, lane: &str) -> Result<Kernel, String> {
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
}
