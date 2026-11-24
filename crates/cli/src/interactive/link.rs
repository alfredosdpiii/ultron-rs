//! The link thread: owns the session [`Link`] and runs the TUI's commands one at a time.

use super::support::{
    apply_presentation_plugins, bash_result, catalog, credentials_saved, export_session, fork_to, image_values,
    import_pi, login_options, model_info, pi_agent_dir, pi_sessions, read_trust, reload_plugins, session_call,
    session_op_runner, share_session, start_call, usage_report, write_debug_log,
};
use super::{WORKER_RECOVERY_INTERVAL, mark_checkpoints, opened, user_keybindings};
use crate::Env;
use client::session::SessionLink as Link;
use interactive::lane::{AgentAction, CatalogRefresh, ForkMessage, LaneCommand, LaneEvent, SessionListing};
use interactive::run::UiEvent;
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::{Arc, mpsc};

/// What the link thread runs: the TUI's commands, and the client's own.
pub(super) enum Command {
    Lane(LaneCommand),
    /// The link saw the session's worker restart under it (its `connection` event `reconnecting`).
    WorkerStopped,
    /// The run ended: report the session's name (a model title may have replaced the automatic one).
    RefreshName,
    /// The session was re-attached after its Radius connection recovered: show it again (`#openLane`).
    Reopen,
    /// A subscription login saved its credential (provider, name): report it as `runLoginDialog` does.
    LoginSaved(String, String),
    Shutdown,
}

/// The flags the link thread's long commands watch, set by [`super::route`] without waiting on the link.
#[derive(Clone, Default)]
pub(super) struct Cancels {
    /// Aborted when the user cancels `/share`'s gist upload; a fresh one for each `/share`.
    pub(super) share: Arc<std::sync::Mutex<kernel::Signal>>,
    /// Set when the user cancels `/bug`'s upload.
    pub(super) bug: Arc<std::sync::atomic::AtomicBool>,
}

struct LinkSession {
    link: Link,
    ui: mpsc::Sender<UiEvent>,
    catalog_env: Env,
    reporter: crate::bug_report::Reporter,
    herdr: Option<Arc<crate::herdr_report::TuiReport>>,
    cancels: Cancels,
    name: Option<String>,
    last_recovery: Option<std::time::Instant>,
    terminal_theme: Option<String>,
}

impl LinkSession {
    fn send(&self, event: LaneEvent) {
        let _ = self.ui.send(UiEvent::Lane(event));
    }

    fn report_name(&mut self) {
        let current = self
            .link
            .op(&Op::Describe)
            .ok()
            .flatten()
            .and_then(|d| d["name"].as_str().map(str::to_string));
        if current != self.name {
            self.name = current.clone();
            if let Some(herdr) = &self.herdr {
                herdr.name(current.clone());
            }
            self.send(LaneEvent::Name(current));
        }
    }

    fn open(&mut self) {
        match opened(&self.link) {
            Ok((session, model, entries, thinking)) => {
                self.name = session.name.clone();
                let prepared = self.link.connection().prepare_session(&session.session_id);
                if let Ok(data) = prepared
                    && let Err(error) = apply_presentation_plugins(&data.unwrap_or(Value::Null))
                {
                    self.send(LaneEvent::Status(format!("Error: {error}")));
                }
                if let Some(herdr) = &self.herdr {
                    herdr.opened(&session, &model);
                }
                self.send(LaneEvent::Opened(session));
                self.send(LaneEvent::Entries(entries));
                self.send(LaneEvent::Model(model));
                if let Some(level) = thinking {
                    self.send(LaneEvent::Thinking(level));
                }
            }
            Err(error) => self.send(LaneEvent::Error(error)),
        }
    }

    fn run_op(&self, op: Op) {
        if let Err(error) = self.link.op(&op) {
            self.send(LaneEvent::Error(error));
        }
    }

    fn persona(&self, op: Op) {
        self.send(LaneEvent::Status(match self.link.op(&op) {
            Ok(reply) => reply
                .as_ref()
                .and_then(|r| r["text"].as_str())
                .unwrap_or("No persona reply")
                .to_string(),
            Err(error) => format!("Error: {error}"),
        }));
    }

    fn handle_worker_stopped(&mut self) {
        if self
            .link
            .remote_route()
            .is_some_and(|route| matches!(route, client::session::RemoteRoute::Radius { .. }))
        {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .last_recovery
            .is_some_and(|t| now.duration_since(t) < WORKER_RECOVERY_INTERVAL)
        {
            self.send(LaneEvent::Status(format!(
                "Error: The session worker stopped again; {}",
                client::lost::RESUME_HINT
            )));
            return;
        }
        self.last_recovery = Some(now);
        self.send(LaneEvent::Status(
            "The session worker stopped unexpectedly; reattaching the Session…".into(),
        ));
        let id = self.link.session_id();
        match self.link.attach(&id) {
            Ok(()) => {
                self.open();
                self.send(LaneEvent::Status(
                    "The session worker stopped unexpectedly and was restarted; the Session is intact, but work running in the old worker (such as background jobs) was lost.".into(),
                ));
            }
            Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
        }
    }

    fn handle_new_or_continue(&mut self, continued: Option<String>) {
        let previous = self.link.session_id();
        let created = self
            .link
            .create(json!({}))
            .and_then(|id| self.link.attach(&id).map(|()| id));
        let created = match (created, &continued) {
            (Ok(id), Some(old)) => self.link.op(&Op::ContinueFrom { session: old.clone() }).map(|_| id),
            (created, _) => created,
        };
        match created {
            Ok(id) => {
                if let Err(error) = crate::durable_client::follow_home(&previous, &id) {
                    self.send(LaneEvent::Status(format!(
                        "Error: the home session was not moved: {error}"
                    )));
                }
                if let Some(herdr) = &self.herdr {
                    herdr.new_session();
                }
                self.open();
                self.send(LaneEvent::Status(match &continued {
                    Some(old) => format!("✓ New session started; it searches session {old}"),
                    None => "✓ New session started".into(),
                }));
            }
            Err(error) => self.send(LaneEvent::Error(error)),
        }
    }

    fn handle_session_lifecycle(&mut self, command: LaneCommand) {
        match command {
            LaneCommand::NewSession => self.handle_new_or_continue(None),
            LaneCommand::ContinueSession(id) => {
                let previous = self.link.session_id();
                self.handle_new_or_continue(Some(id.unwrap_or(previous)));
            }
            LaneCommand::Resume(path) => {
                let id = session_journal::catalog::read_metadata(std::path::Path::new(&path)).map(|m| m.id);
                match id
                    .ok_or_else(|| format!("Not a session file: {path}"))
                    .and_then(|id| self.link.attach(&id))
                {
                    Ok(()) => {
                        self.open();
                        self.send(LaneEvent::Status("Resumed session".into()));
                    }
                    Err(error) => self.send(LaneEvent::Error(error)),
                }
            }
            LaneCommand::ListSessions => match self.link.op(&Op::ListSessions) {
                Ok(listed) => self.send(LaneEvent::Sessions(SessionListing::listed(&listed.unwrap_or_default()))),
                Err(error) => self.send(LaneEvent::Error(error)),
            },
            LaneCommand::ListPiSessions => match pi_sessions(&self.catalog_env) {
                Ok(sessions) => self.send(LaneEvent::PiSessions(sessions)),
                Err(error) => self.send(LaneEvent::Error(error)),
            },
            LaneCommand::ListAllSessions => {
                let dir = crate::session::session_directory(None, &self.catalog_env);
                let repo = session_journal::SessionRepo::new(&dir, session_journal::SystemClock::shared());
                let listed = session_journal::names::list_all_sessions(&repo)
                    .into_iter()
                    .map(|s| SessionListing {
                        path: s.metadata.path.display().to_string(),
                        id: s.metadata.id,
                        name: s.name,
                        modified: s.metadata.modified_at as i64,
                        message_count: s.message_count,
                        first_message: s.first_message,
                        all_messages_text: s.all_messages_text,
                        cwd: s.metadata.cwd,
                    })
                    .collect();
                self.send(LaneEvent::AllSessions(listed));
            }
            LaneCommand::ListAllPiSessions => {
                let dir = config::paths::AgentPaths::new(pi_agent_dir(&self.catalog_env)).sessions_dir();
                let listed =
                    session_file::list_all_sessions(&session_file::SystemEnv, Some(&dir.display().to_string()))
                        .into_iter()
                        .map(|s| SessionListing {
                            path: s.path,
                            id: s.id,
                            name: s.name,
                            modified: s.modified.unwrap_or(0),
                            message_count: s.message_count,
                            first_message: s.first_message,
                            all_messages_text: s.all_messages_text,
                            cwd: s.cwd,
                        })
                        .collect();
                self.send(LaneEvent::PiAllSessions(listed));
            }
            LaneCommand::DeleteSession(path) => {
                let id = session_journal::catalog::read_metadata(std::path::Path::new(&path)).map(|m| m.id);
                let result = match id {
                    Some(id) if id == self.link.session_id() => {
                        Err("Cannot delete the currently active session".into())
                    }
                    Some(id) => self.link.remove(&id).map_err(|e| format!("Failed to delete: {e}")),
                    None => Ok(()),
                };
                self.send(LaneEvent::SessionDeleted(result));
                if let Ok(listed) = self.link.op(&Op::ListSessions) {
                    self.send(LaneEvent::Sessions(SessionListing::listed(&listed.unwrap_or_default())));
                }
            }
            LaneCommand::ImportPi(path) => self.send(import_pi(&self.link, &path)),
            LaneCommand::RenameSession { path, name: new_name } => {
                let id = session_journal::catalog::read_metadata(std::path::Path::new(&path)).map(|m| m.id);
                let renamed = match id {
                    Some(id) if id == self.link.session_id() => {
                        self.link.op(&Op::SetName { name: new_name }).map(|_| ())
                    }
                    Some(id) => self.link.rename(&id, &new_name),
                    None => Ok(()),
                };
                if let Err(error) = renamed {
                    self.send(LaneEvent::Status(format!("Error: {error}")));
                }
                if let Ok(listed) = self.link.op(&Op::ListSessions) {
                    self.send(LaneEvent::Sessions(SessionListing::listed(&listed.unwrap_or_default())));
                }
            }
            LaneCommand::ForkMessages => match self.link.op(&Op::ForkMessages) {
                Ok(listed) => self.send(LaneEvent::ForkMessages(
                    listed
                        .unwrap_or_default()
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|m| ForkMessage {
                            entry_id: m["entryId"].as_str().unwrap_or_default().to_string(),
                            text: m["text"].as_str().unwrap_or_default().to_string(),
                        })
                        .collect(),
                )),
                Err(error) => self.send(LaneEvent::Error(error)),
            },
            LaneCommand::Fork(entry_id) => {
                let text = self
                    .link
                    .op(&Op::ForkSource {
                        entry_id: entry_id.clone(),
                    })
                    .map(|t| t.and_then(|t| t.as_str().map(str::to_string)).unwrap_or_default());
                let forked = text.and_then(|text| {
                    fork_to(&mut self.link, json!({"entryId": entry_id, "position": "before"})).map(|()| text)
                });
                match forked {
                    Ok(text) => {
                        self.open();
                        self.send(LaneEvent::Forked {
                            editor_text: text,
                            status: "Forked to new session".into(),
                        });
                    }
                    Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
                }
            }
            LaneCommand::CloneSession => {
                let leaf = self
                    .link
                    .op(&Op::Entries { since: None })
                    .map(|e| e.unwrap_or_default()["leafId"].clone());
                match leaf {
                    Ok(Value::Null) => self.send(LaneEvent::Status("Nothing to clone yet".into())),
                    Ok(_) => match fork_to(&mut self.link, json!({"position": "at"})) {
                        Ok(()) => {
                            self.open();
                            self.send(LaneEvent::Forked {
                                editor_text: String::new(),
                                status: "Cloned to new session".into(),
                            });
                        }
                        Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
                    },
                    Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
                }
            }
            _ => unreachable!(),
        }
    }

    fn handle_tree_and_agents(&mut self, command: LaneCommand) {
        match command {
            LaneCommand::Tree => match self.link.op(&Op::Tree) {
                Ok(read) => {
                    let read = read.unwrap_or_default();
                    let checkpoints = self.link.op(&Op::Checkpoints).ok().flatten().unwrap_or_default();
                    self.send(LaneEvent::Tree {
                        tree: mark_checkpoints(read["tree"].clone(), &checkpoints),
                        leaf: read["leafId"].as_str().map(str::to_string),
                    });
                }
                Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
            },
            LaneCommand::Navigate {
                target,
                summarize,
                instructions,
            } => {
                let started = start_call(
                    &self.link.connection(),
                    protocol::names::AGENT_CONTROLLER,
                    "navigate",
                    json!([{"targetId": target, "summarize": summarize, "label": null, "customInstructions": instructions}]),
                );
                let result = started
                    .and_then(|(connection, request)| connection.wait(&request).map_err(client::lost::message))
                    .map_err(|error| {
                        error
                            .strip_prefix("session_operation_failed: ")
                            .map(str::to_string)
                            .unwrap_or(error)
                    });
                if result.is_ok() {
                    self.open();
                }
                self.send(LaneEvent::Navigated(result.map(|answer| {
                    answer.and_then(|a| a["editorText"].as_str().map(str::to_string))
                })));
            }
            LaneCommand::RestoreFiles(entry_id) => {
                let restored = self.link.op(&Op::RestoreFiles {
                    entry_id: Some(entry_id),
                    checkpoint: None,
                });
                self.send(LaneEvent::FilesRestored(restored.map(|answer| {
                    answer
                        .and_then(|a| a["message"].as_str().map(str::to_string))
                        .unwrap_or_default()
                })));
            }
            LaneCommand::SetLabel { entry_id, label } => {
                if let Err(error) = self.link.op(&Op::SetLabel { entry_id, label }) {
                    self.send(LaneEvent::Status(format!("Error: {error}")));
                }
            }
            LaneCommand::RlmStatus => {
                if let Ok(listed) = self.link.op(&Op::Agents) {
                    self.send(LaneEvent::RlmStatus(listed.unwrap_or_default()));
                }
            }
            LaneCommand::Agents => match self.link.op(&Op::Agents) {
                Ok(listed) => self.send(LaneEvent::Agents(listed.unwrap_or_default())),
                Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
            },
            LaneCommand::KillAgent(id) => {
                if let Err(error) = self.link.op(&Op::KillAgent { id }) {
                    self.send(LaneEvent::Status(format!("Error: {error}")));
                }
            }
            LaneCommand::SteerAgent { id, text } => {
                let answer = self.link.op(&Op::SteerAgent {
                    id: id.clone(),
                    message: text,
                });
                self.send(LaneEvent::AgentAnswered {
                    id,
                    action: AgentAction::Steer,
                    answer,
                });
            }
            LaneCommand::ReviveAgent(id) => {
                let answer = self.link.op(&Op::ReviveAgent { id: id.clone() });
                self.send(LaneEvent::AgentAnswered {
                    id,
                    action: AgentAction::Revive,
                    answer,
                });
            }
            LaneCommand::AgentTranscript(id) => {
                let read = self.link.op(&Op::AgentTranscript { id: id.clone() });
                self.send(LaneEvent::AgentTranscript {
                    id,
                    messages: read.map(|read| {
                        read.unwrap_or_default()["messages"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                    }),
                });
            }
            LaneCommand::RlmInspect { tag, request, payload } => self.send(LaneEvent::RlmInspected {
                tag,
                result: self
                    .link
                    .op(&Op::Inspect { request, payload })
                    .map(Option::unwrap_or_default),
            }),
            LaneCommand::Inspect {
                title,
                request,
                payload,
            } => self.send(LaneEvent::Inspected {
                title,
                result: self
                    .link
                    .op(&Op::Inspect { request, payload })
                    .map(|value| value.unwrap_or(Value::Null)),
            }),
            _ => unreachable!(),
        }
    }

    fn handle_auth_and_settings(&mut self, command: LaneCommand) {
        let send_fn = |event: LaneEvent| {
            let _ = self.ui.send(UiEvent::Lane(event));
        };
        match command {
            LaneCommand::SaveCredential { provider, credential } => {
                let auth = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
                let name = interactive::selectors::provider_name(&provider);
                let saved = config::auth::AuthStorage::create(auth).modify(&provider, |_| Some(credential));
                match saved {
                    Err(error) => self.send(LaneEvent::Status(format!(
                        "Error: Failed to save API key for {name}: {error}"
                    ))),
                    Ok(_) => credentials_saved(
                        &self.link,
                        &self.catalog_env,
                        &send_fn,
                        "Saved API key for",
                        &provider,
                        &name,
                    ),
                }
            }
            LaneCommand::LoginProviders(query) => {
                let auth = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
                self.send(LaneEvent::LoginProviders {
                    providers: login_options(&self.catalog_env, &auth),
                    query,
                });
            }
            LaneCommand::SaveApiKey { provider, key } => {
                let auth = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
                let name = interactive::selectors::provider_name(&provider);
                let saved = config::auth::AuthStorage::create(auth)
                    .modify(&provider, |_| Some(json!({"type": "api_key", "key": key})));
                match saved {
                    Err(error) => self.send(LaneEvent::Status(format!(
                        "Error: Failed to save API key for {name}: {error}"
                    ))),
                    Ok(_) => credentials_saved(
                        &self.link,
                        &self.catalog_env,
                        &send_fn,
                        "Saved API key for",
                        &provider,
                        &name,
                    ),
                }
            }
            LaneCommand::RefreshCatalog => {
                let (env, ui) = (self.catalog_env.clone(), self.ui.clone());
                std::thread::spawn(move || {
                    let refresh = match session_core::model_refresh::refresh_catalogs(&env) {
                        Ok(errors) => CatalogRefresh::Done(errors.into_iter().map(|(provider, _)| provider).collect()),
                        Err(error) if error == session_core::model_refresh::REFRESH_TIMED_OUT => {
                            CatalogRefresh::TimedOut
                        }
                        Err(error) => CatalogRefresh::Failed(error),
                    };
                    let _ = ui.send(UiEvent::Lane(LaneEvent::Catalog(catalog(&env))));
                    let _ = ui.send(UiEvent::Lane(LaneEvent::CatalogRefreshed(refresh)));
                });
            }
            LaneCommand::Credentials => {
                let path = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
                match config::auth::AuthStorage::create(path).list() {
                    Ok(stored) => self.send(LaneEvent::Credentials(
                        stored
                            .into_iter()
                            .map(|(id, c)| (id, c["type"].as_str().unwrap_or("api_key").to_string()))
                            .collect(),
                    )),
                    Err(error) => self.send(LaneEvent::Status(format!(
                        "Error: Could not read stored credentials: {error}"
                    ))),
                }
            }
            LaneCommand::Logout { provider, done } => {
                let path = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
                match config::auth::AuthStorage::create(path).delete(&provider) {
                    Ok(()) => {
                        self.send(LaneEvent::Catalog(catalog(&self.catalog_env)));
                        self.send(LaneEvent::Status(done));
                    }
                    Err(error) => self.send(LaneEvent::Status(format!("Error: Logout failed: {error}"))),
                }
            }
            LaneCommand::ReadSettings => self.read_settings(),
            LaneCommand::ReadMode => {
                let (cwd, agent_dir) = (&self.catalog_env.cwd, &self.catalog_env.agent_dir());
                let settings = config::settings::SettingsManager::for_worker(cwd, agent_dir, &self.catalog_env.home);
                let mode = session_core::lane::PromptMode::from_settings(&settings);
                self.send(LaneEvent::Mode {
                    engineering: mode.engineering,
                    always_skills: mode.always_skills,
                });
            }
            LaneCommand::TerminalTheme(theme) => {
                if let Err(error) = self.link.keep_op(&Op::SetTerminalTheme { theme: theme.clone() }) {
                    self.send(LaneEvent::Status(format!("Error: {error}")));
                }
                self.terminal_theme = Some(theme);
            }
            LaneCommand::SaveSetting { key, value } => {
                if let Err(error) = session_call(
                    &self.link,
                    protocol::names::SESSION_CONTROL,
                    "setSetting",
                    json!([key, value]),
                ) {
                    self.send(LaneEvent::Status(format!("Error: {error}")));
                }
            }
            LaneCommand::ReadTrust => self.send(match read_trust(&self.link) {
                Ok(view) => LaneEvent::Trust(view),
                Err(error) => LaneEvent::Status(format!("Error: {error}")),
            }),
            LaneCommand::SetProjectTrust { trusted, updates } => {
                let updates: Vec<Value> = updates
                    .into_iter()
                    .map(|(path, decision)| json!({"path": path, "decision": decision}))
                    .collect();
                self.send(LaneEvent::Status(
                    match session_call(
                        &self.link,
                        protocol::names::SESSION_CONTROL,
                        "setProjectTrust",
                        json!([updates]),
                    ) {
                        Ok(_) => interactive::trust_selector::saved_status(trusted),
                        Err(error) => format!("Error: {error}"),
                    },
                ));
            }
            _ => unreachable!(),
        }
    }

    fn read_settings(&self) {
        let (cwd, agent_dir) = (&self.catalog_env.cwd, &self.catalog_env.agent_dir());
        let settings = config::settings::SettingsManager::for_worker(cwd, agent_dir, &self.catalog_env.home);
        let env_overrides = config::env::model_env_overrides(&|name| std::env::var(name).ok())
            .into_iter()
            .map(|(key, name, value)| (key.to_string(), (name.to_string(), value)))
            .collect();
        let mut themes = vec!["dark".to_string(), "light".to_string()];
        if let Ok(dir) = std::fs::read_dir(config::paths::AgentPaths::new(agent_dir).custom_themes_dir()) {
            let mut custom: Vec<String> = dir
                .flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .strip_suffix(".json")
                        .map(str::to_string)
                })
                .filter(|n| !themes.contains(n))
                .collect();
            custom.sort();
            themes.extend(custom);
        }
        let terminal_theme = self.terminal_theme.clone().unwrap_or_else(|| {
            theme::detect_terminal_background_from_env(std::env::var("COLORFGBG").ok().as_deref())
                .theme
                .name()
                .to_string()
        });
        let mut values = settings.settings().clone();
        if session_core::lane::is_durable(&self.catalog_env) && !values.get("durable").is_some_and(Value::is_object) {
            values.insert("durable".into(), json!({}));
        }
        self.send(LaneEvent::Settings {
            settings: Value::Object(values),
            env_overrides,
            themes,
            terminal_theme,
        });
    }

    fn handle_tools_and_reports(&self, command: LaneCommand) {
        let send_fn = |event: LaneEvent| {
            let _ = self.ui.send(UiEvent::Lane(event));
        };
        match command {
            LaneCommand::Bash { command, exclude } => {
                let started = start_call(
                    &self.link.connection(),
                    protocol::names::SESSION_CONTROL,
                    "bash",
                    json!([command, exclude]),
                );
                let ui = self.ui.clone();
                std::thread::spawn(move || {
                    let result = started
                        .and_then(|(connection, request)| connection.wait(&request).map_err(client::lost::message));
                    let event = LaneEvent::BashDone(result.map(|value| bash_result(&value.unwrap_or_default())));
                    let _ = ui.send(UiEvent::Lane(event));
                });
            }
            LaneCommand::Compact(instructions) => {
                let started = start_call(
                    &self.link.connection(),
                    protocol::names::AGENT_CONTROLLER,
                    "compact",
                    json!([{"customInstructions": instructions}]),
                );
                let ui = self.ui.clone();
                std::thread::spawn(move || {
                    let result = started
                        .and_then(|(connection, request)| connection.wait(&request).map_err(client::lost::message));
                    if let Err(error) = result {
                        let error = error
                            .strip_prefix("session_operation_failed: ")
                            .map(str::to_string)
                            .unwrap_or(error);
                        let _ = ui.send(UiEvent::Lane(LaneEvent::Compacted(Err(error))));
                    }
                });
            }
            LaneCommand::Export { path, theme } => self.send(LaneEvent::Status(
                match export_session(&self.link, path.as_deref(), theme.as_deref(), &self.catalog_env) {
                    Ok(path) => format!("Session exported to: {path}"),
                    Err(error) => format!("Error: Failed to export session: {error}"),
                },
            )),
            LaneCommand::Mcp(args) => {
                let (connection, ui) = (self.link.connection(), self.ui.clone());
                std::thread::spawn(move || {
                    let send = |event| {
                        let _ = ui.send(UiEvent::Lane(event));
                    };
                    let run = connection.session_target().and_then(|target| {
                        connection.call(
                            target,
                            protocol::names::LEGACY_EXTENSION_COMMANDS,
                            "run",
                            json!(["mcp", args]),
                        )
                    });
                    match run {
                        Ok(result) => {
                            let result = result.unwrap_or_default();
                            for notification in result["notifications"].as_array().into_iter().flatten() {
                                send(LaneEvent::Status(notification.as_str().unwrap_or_default().to_string()));
                            }
                        }
                        Err(error) => send(LaneEvent::Status(format!("Error: {}", client::lost::message(error)))),
                    }
                });
            }
            LaneCommand::Usage => match usage_report(&self.link, &self.catalog_env) {
                Ok(text) => self.send(LaneEvent::Notice(text)),
                Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
            },
            LaneCommand::Observer(args) => match self.link.op(&Op::Observer { args }) {
                Ok(reply) => self.send(LaneEvent::Notice(
                    reply.and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
                )),
                Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
            },
            LaneCommand::Goal(args) => match self.link.op(&Op::Goal { args }) {
                Ok(reply) => self.send(LaneEvent::GoalReply(
                    reply
                        .as_ref()
                        .and_then(Value::as_str)
                        .unwrap_or("No goal reply")
                        .to_string(),
                )),
                Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
            },
            LaneCommand::Changelog => {
                let path = self.catalog_env.package_file("CHANGELOG.md");
                let ui = self.ui.clone();
                std::thread::spawn(move || {
                    let content = std::fs::read_to_string(path).ok();
                    let tokens = interactive::changelog::changelog_tokens(content.as_deref());
                    let _ = ui.send(UiEvent::Lane(LaneEvent::Changelog(tokens)));
                });
            }
            LaneCommand::Share { theme } => {
                let cancel = jsrt::sync::lock(&self.cancels.share).clone();
                share_session(&self.link, &send_fn, &cancel, theme.as_deref(), &self.catalog_env);
            }
            LaneCommand::BugReportSummary(hint) => {
                let op = Op::BugSummary { hint };
                let started = start_call(
                    &self.link.connection(),
                    protocol::names::SESSION_OPS,
                    "op",
                    json!([op.to_json()]),
                );
                let ui = self.ui.clone();
                std::thread::spawn(move || {
                    let result = started
                        .and_then(|(connection, request)| connection.wait(&request).map_err(client::lost::message))
                        .map(|text| text.and_then(|t| t.as_str().map(str::to_string)).unwrap_or_default())
                        .map_err(|e| {
                            e.strip_prefix("session_operation_failed: ")
                                .map_or(e.clone(), str::to_string)
                        });
                    let _ = ui.send(UiEvent::Lane(LaneEvent::BugSummary(result)));
                });
            }
            LaneCommand::AbortBugReportSummary => {
                let _ = self.link.op(&Op::AbortBugSummary);
            }
            LaneCommand::BugReport(request) => {
                let answer_ui = self.ui.clone();
                let answer: Arc<dyn Fn(interactive::lane::BugReportOutcome) + Send + Sync> = Arc::new(move |outcome| {
                    let _ = answer_ui.send(UiEvent::Lane(LaneEvent::BugReport(outcome)));
                });
                self.reporter.deliver(
                    &self.link,
                    &self.catalog_env,
                    request,
                    session_op_runner(&self.link),
                    answer,
                );
            }
            LaneCommand::BugReportExport => {
                let outcome = self.reporter.export_last(&self.catalog_env, &|op| self.link.op(&op));
                self.send(LaneEvent::BugReport(outcome));
            }
            LaneCommand::Autoreview(request) => self.send(crate::autoreview_toggle::autoreview(
                request,
                &self.catalog_env,
                jsrt::process::platform(),
                &crate::autoreview_toggle::run_command,
            )),
            LaneCommand::WriteDebugLog(capture) => {
                self.send(write_debug_log(
                    &self.link,
                    &self.catalog_env,
                    &self.catalog_env.agent_dir(),
                    &capture,
                ));
            }
            LaneCommand::ReloadPlugins => {
                self.send(LaneEvent::Keybindings(user_keybindings(
                    &self.catalog_env.agent_dir(),
                    crate::terminal_settings::platform(&self.catalog_env),
                )));
                self.send(LaneEvent::Status(match reload_plugins(&self.link) {
                    Ok(()) => "Reloaded plugins.".into(),
                    Err(error) => format!("Error: {error}"),
                }));
            }
            _ => unreachable!(),
        }
    }

    fn handle_lane(&mut self, command: LaneCommand) {
        match command {
            LaneCommand::Enqueue {
                kind,
                text,
                images,
                entry_id,
            } => self.run_op(Op::Enqueue {
                kind,
                message: text,
                images: image_values(images),
                entry_id: Some(entry_id),
            }),
            LaneCommand::CancelQueued(entry_id) => self.run_op(Op::CancelQueued { entry_id }),
            LaneCommand::SetName(name) => self.run_op(Op::SetName { name }),
            LaneCommand::Prompt { text, images } => match self.link.op(&Op::Prompt {
                message: text,
                images: image_values(images),
                queue: None,
            }) {
                Ok(reply) => {
                    if let Some(notice) = reply.as_ref().and_then(|r| r["keepGoing"].as_str()) {
                        self.send(LaneEvent::Status(notice.to_string()));
                    }
                    self.report_name();
                }
                Err(error) => self.send(LaneEvent::Error(error)),
            },
            LaneCommand::SelectModel { provider, id: model_id } => {
                match self.link.op(&Op::SetModel { provider, model_id }) {
                    Ok(model) => {
                        let model = model_info(&model.unwrap_or_default());
                        if let Some(herdr) = &self.herdr {
                            herdr.model(&model);
                        }
                        self.send(LaneEvent::Model(model));
                    }
                    Err(error) => self.send(LaneEvent::Error(error)),
                }
            }
            LaneCommand::SetThinking(level) => self.run_op(Op::SetThinking { level }),
            LaneCommand::ExtensionUiRespond { id, answer } => {
                let connection = self.link.connection();
                let _ = connection.session_target().and_then(|target| {
                    connection.call(target, protocol::names::EXTENSION_UI, "respond", json!([id, answer]))
                });
            }
            LaneCommand::Persona(args) => self.persona(Op::Persona { args }),
            LaneCommand::KeepGoing(args) => self.persona(Op::KeepGoing { args }),
            LaneCommand::ReadPersona => self.persona(Op::Persona { args: String::new() }),
            cmd @ (LaneCommand::NewSession
            | LaneCommand::ContinueSession(_)
            | LaneCommand::Resume(_)
            | LaneCommand::ListSessions
            | LaneCommand::ListPiSessions
            | LaneCommand::ListAllSessions
            | LaneCommand::ListAllPiSessions
            | LaneCommand::DeleteSession(_)
            | LaneCommand::ImportPi(_)
            | LaneCommand::RenameSession { .. }
            | LaneCommand::ForkMessages
            | LaneCommand::Fork(_)
            | LaneCommand::CloneSession) => self.handle_session_lifecycle(cmd),
            cmd @ (LaneCommand::Tree
            | LaneCommand::Navigate { .. }
            | LaneCommand::RestoreFiles(_)
            | LaneCommand::SetLabel { .. }
            | LaneCommand::RlmStatus
            | LaneCommand::Agents
            | LaneCommand::KillAgent(_)
            | LaneCommand::SteerAgent { .. }
            | LaneCommand::ReviveAgent(_)
            | LaneCommand::AgentTranscript(_)
            | LaneCommand::RlmInspect { .. }
            | LaneCommand::Inspect { .. }) => self.handle_tree_and_agents(cmd),
            cmd @ (LaneCommand::SaveCredential { .. }
            | LaneCommand::LoginProviders(_)
            | LaneCommand::SaveApiKey { .. }
            | LaneCommand::RefreshCatalog
            | LaneCommand::Credentials
            | LaneCommand::Logout { .. }
            | LaneCommand::ReadSettings
            | LaneCommand::ReadMode
            | LaneCommand::TerminalTheme(_)
            | LaneCommand::SaveSetting { .. }
            | LaneCommand::ReadTrust
            | LaneCommand::SetProjectTrust { .. }) => self.handle_auth_and_settings(cmd),
            cmd @ (LaneCommand::Bash { .. }
            | LaneCommand::Compact(_)
            | LaneCommand::Export { .. }
            | LaneCommand::Mcp(_)
            | LaneCommand::Usage
            | LaneCommand::Observer(_)
            | LaneCommand::Goal(_)
            | LaneCommand::Changelog
            | LaneCommand::Share { .. }
            | LaneCommand::BugReportSummary(_)
            | LaneCommand::AbortBugReportSummary
            | LaneCommand::BugReport(_)
            | LaneCommand::BugReportExport
            | LaneCommand::Autoreview(_)
            | LaneCommand::WriteDebugLog(_)
            | LaneCommand::ReloadPlugins) => self.handle_tools_and_reports(cmd),
            LaneCommand::StartupChangelog
            | LaneCommand::SubscriptionAuthWarning(_)
            | LaneCommand::OAuthLogin { .. }
            | LaneCommand::LoginReply { .. }
            | LaneCommand::Abort
            | LaneCommand::AbortRetry
            | LaneCommand::AbortBash
            | LaneCommand::CancelShare
            | LaneCommand::CancelBugReport => {}
        }
    }
}

/// The link thread: owns the link, runs the TUI's commands one at a time.
#[expect(
    clippy::too_many_arguments,
    reason = "internal wiring helper; parameters match distinct caller inputs"
)]
pub(super) fn link_thread(
    link: Link,
    commands: mpsc::Receiver<Command>,
    ui: mpsc::Sender<UiEvent>,
    no_session: bool,
    signalled: Arc<std::sync::atomic::AtomicI32>,
    catalog_env: Env,
    reporter: crate::bug_report::Reporter,
    herdr: Option<Arc<crate::herdr_report::TuiReport>>,
    cancels: Cancels,
) {
    let mut session = LinkSession {
        link,
        ui,
        catalog_env,
        reporter,
        herdr,
        cancels,
        name: None,
        last_recovery: None,
        terminal_theme: None,
    };
    session.open();
    let mut extension_ui = crate::link::ExtensionUiPoll::default();
    loop {
        for request in extension_ui.poll(&session.link.session_id(), &session.link.connection()) {
            session.send(LaneEvent::ExtensionUi(request));
        }
        let command = match commands.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        };
        match command {
            Command::WorkerStopped => session.handle_worker_stopped(),
            Command::RefreshName => session.report_name(),
            Command::Reopen => session.open(),
            Command::LoginSaved(provider, name) => {
                let _ = session_core::model_refresh::refresh_catalogs(&session.catalog_env);
                let send_fn = |event: LaneEvent| {
                    let _ = session.ui.send(UiEvent::Lane(event));
                };
                credentials_saved(
                    &session.link,
                    &session.catalog_env,
                    &send_fn,
                    "Logged in to",
                    &provider,
                    &name,
                );
            }
            Command::Lane(cmd) => session.handle_lane(cmd),
            Command::Shutdown => break,
        }
    }
    if signalled.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        let _ = session.link.op(&Op::Abort);
        session.link.close(no_session);
    } else {
        session.link.close(false);
    }
}
