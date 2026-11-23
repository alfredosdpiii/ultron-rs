use super::*;

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
    pub(super) fn model_check(self: &Arc<Self>) -> delegation::host::ModelCheck {
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
    pub(super) fn file_hooks_in(&self, cwd: &Path, record: Option<OnRecord>) -> FileHooks {
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
    pub(super) fn session_dir(&self, kind: &str, session_id: &str) -> PathBuf {
        self.agent_dir.join(kind).join(session_id)
    }
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
