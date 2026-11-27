//! Command-group handlers for [`LinkSession`] (session lifecycle, tree/agents, auth/settings, tools/reports).

use super::link::LinkSession;
use super::support::{
    bash_result, catalog, credentials_saved, export_session, fork_to, import_pi, login_options, pi_agent_dir,
    pi_sessions, read_trust, reload_plugins, session_call, session_op_runner, share_session, start_call, usage_report,
    write_debug_log,
};
use super::{mark_checkpoints, user_keybindings};
use interactive::lane::{AgentAction, CatalogRefresh, ForkMessage, LaneCommand, LaneEvent, SessionListing};
use interactive::run::UiEvent;
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::Arc;

impl LinkSession {
    pub(super) fn handle_session_lifecycle(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        let command = self.handle_session_lists(command)?;
        self.handle_session_mutations(command)
    }

    fn handle_session_lists(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::ListSessions => self.send_listed_sessions(),
            LaneCommand::ListPiSessions => match pi_sessions(&self.catalog_env) {
                Ok(sessions) => self.send(LaneEvent::PiSessions(sessions)),
                Err(error) => self.send(LaneEvent::Error(error)),
            },
            LaneCommand::ListAllSessions => self.list_all_sessions(),
            LaneCommand::ListAllPiSessions => self.list_all_pi_sessions(),
            LaneCommand::ForkMessages => self.handle_fork_messages(),
            other => return Some(other),
        }
        None
    }

    fn handle_session_mutations(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::NewSession => self.handle_new_or_continue(None),
            LaneCommand::ContinueSession(id) => {
                let previous = self.link.session_id();
                self.handle_new_or_continue(Some(id.unwrap_or(previous)));
            }
            LaneCommand::Resume(path) => self.handle_resume(&path),
            LaneCommand::DeleteSession(path) => self.handle_delete_session(&path),
            LaneCommand::ImportPi(path) => self.send(import_pi(&self.link, &path)),
            LaneCommand::RenameSession { path, name } => self.handle_rename_session(&path, name),
            LaneCommand::Fork(entry_id) => self.handle_fork(entry_id),
            LaneCommand::CloneSession => self.handle_clone_session(),
            other => return Some(other),
        }
        None
    }

    fn send_listed_sessions(&self) {
        match self.link.op(&Op::ListSessions) {
            Ok(listed) => self.send(LaneEvent::Sessions(SessionListing::listed(&listed.unwrap_or_default()))),
            Err(error) => self.send(LaneEvent::Error(error)),
        }
    }

    fn refresh_listed_sessions(&self) {
        if let Ok(listed) = self.link.op(&Op::ListSessions) {
            self.send(LaneEvent::Sessions(SessionListing::listed(&listed.unwrap_or_default())));
        }
    }

    fn list_all_sessions(&self) {
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

    fn list_all_pi_sessions(&self) {
        let dir = config::paths::AgentPaths::new(pi_agent_dir(&self.catalog_env)).sessions_dir();
        let listed = session_file::list_all_sessions(&session_file::SystemEnv, Some(&dir.display().to_string()))
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

    fn handle_fork_messages(&self) {
        match self.link.op(&Op::ForkMessages) {
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
        }
    }

    fn handle_resume(&mut self, path: &str) {
        let id = session_journal::catalog::read_metadata(std::path::Path::new(path)).map(|m| m.id);
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

    fn handle_delete_session(&self, path: &str) {
        let id = session_journal::catalog::read_metadata(std::path::Path::new(path)).map(|m| m.id);
        let result = match id {
            Some(id) if id == self.link.session_id() => Err("Cannot delete the currently active session".into()),
            Some(id) => self.link.remove(&id).map_err(|e| format!("Failed to delete: {e}")),
            None => Ok(()),
        };
        self.send(LaneEvent::SessionDeleted(result));
        self.refresh_listed_sessions();
    }

    fn handle_rename_session(&self, path: &str, new_name: String) {
        let id = session_journal::catalog::read_metadata(std::path::Path::new(path)).map(|m| m.id);
        let renamed = match id {
            Some(id) if id == self.link.session_id() => self.link.op(&Op::SetName { name: new_name }).map(|_| ()),
            Some(id) => self.link.rename(&id, &new_name),
            None => Ok(()),
        };
        if let Err(error) = renamed {
            self.send(LaneEvent::Status(format!("Error: {error}")));
        }
        self.refresh_listed_sessions();
    }

    fn handle_fork(&mut self, entry_id: String) {
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

    fn handle_clone_session(&mut self) {
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

    pub(super) fn handle_tree_and_agents(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        let command = self.handle_tree_commands(command)?;
        self.handle_agent_commands(command)
    }

    fn handle_tree_commands(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::Tree => self.handle_tree(),
            LaneCommand::Navigate {
                target,
                summarize,
                instructions,
            } => self.handle_navigate(target, summarize, instructions),
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
            other => return Some(other),
        }
        None
    }

    fn handle_agent_commands(&self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
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
            other => return Some(other),
        }
        None
    }

    fn handle_tree(&self) {
        match self.link.op(&Op::Tree) {
            Ok(read) => {
                let read = read.unwrap_or_default();
                let checkpoints = self.link.op(&Op::Checkpoints).ok().flatten().unwrap_or_default();
                self.send(LaneEvent::Tree {
                    tree: mark_checkpoints(read["tree"].clone(), &checkpoints),
                    leaf: read["leafId"].as_str().map(str::to_string),
                });
            }
            Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
        }
    }

    fn handle_navigate(&mut self, target: Option<String>, summarize: bool, instructions: Option<String>) {
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

    pub(super) fn handle_auth_and_settings(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        let command = self.handle_auth_commands(command)?;
        self.handle_settings_commands(command)
    }

    fn handle_auth_commands(&self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::SaveCredential { provider, credential } => self.save_credential(&provider, credential),
            LaneCommand::LoginProviders(query) => {
                let auth = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
                self.send(LaneEvent::LoginProviders {
                    providers: login_options(&self.catalog_env, &auth),
                    query,
                });
            }
            LaneCommand::SaveApiKey { provider, key } => {
                self.save_credential(&provider, json!({"type": "api_key", "key": key}));
            }
            LaneCommand::RefreshCatalog => self.refresh_catalog(),
            LaneCommand::Credentials => self.list_credentials(),
            LaneCommand::Logout { provider, done } => self.logout_provider(&provider, done),
            other => return Some(other),
        }
        None
    }

    fn handle_settings_commands(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::ReadSettings => self.read_settings(),
            LaneCommand::ReadMode => self.read_mode(),
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
            LaneCommand::SetProjectTrust { trusted, updates } => self.set_project_trust(trusted, updates),
            other => return Some(other),
        }
        None
    }

    fn save_credential(&self, provider: &str, credential: Value) {
        let send_fn = |event: LaneEvent| {
            let _ = self.ui.send(UiEvent::Lane(event));
        };
        let auth = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
        let name = interactive::selectors::provider_name(provider);
        let saved = config::auth::AuthStorage::create(auth).modify(provider, |_| Some(credential));
        match saved {
            Err(error) => self.send(LaneEvent::Status(format!(
                "Error: Failed to save API key for {name}: {error}"
            ))),
            Ok(_) => credentials_saved(
                &self.link,
                &self.catalog_env,
                &send_fn,
                "Saved API key for",
                provider,
                &name,
            ),
        }
    }

    fn refresh_catalog(&self) {
        let (env, ui) = (self.catalog_env.clone(), self.ui.clone());
        std::thread::spawn(move || {
            let refresh = match session_core::model_refresh::refresh_catalogs(&env) {
                Ok(errors) => CatalogRefresh::Done(errors.into_iter().map(|(provider, _)| provider).collect()),
                Err(error) if error == session_core::model_refresh::REFRESH_TIMED_OUT => CatalogRefresh::TimedOut,
                Err(error) => CatalogRefresh::Failed(error),
            };
            let _ = ui.send(UiEvent::Lane(LaneEvent::Catalog(catalog(&env))));
            let _ = ui.send(UiEvent::Lane(LaneEvent::CatalogRefreshed(refresh)));
        });
    }

    fn list_credentials(&self) {
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

    fn logout_provider(&self, provider: &str, done: String) {
        let path = config::paths::AgentPaths::new(self.catalog_env.agent_dir()).auth();
        match config::auth::AuthStorage::create(path).delete(provider) {
            Ok(()) => {
                self.send(LaneEvent::Catalog(catalog(&self.catalog_env)));
                self.send(LaneEvent::Status(done));
            }
            Err(error) => self.send(LaneEvent::Status(format!("Error: Logout failed: {error}"))),
        }
    }

    fn read_mode(&self) {
        let (cwd, agent_dir) = (&self.catalog_env.cwd, &self.catalog_env.agent_dir());
        let settings = config::settings::SettingsManager::for_worker(cwd, agent_dir, &self.catalog_env.home);
        let mode = session_core::lane::PromptMode::from_settings(&settings);
        self.send(LaneEvent::Mode {
            engineering: mode.engineering,
            always_skills: mode.always_skills,
        });
    }

    fn set_project_trust(&self, trusted: bool, updates: Vec<(String, Option<bool>)>) {
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

    pub(super) fn handle_tools_and_reports(&self, command: LaneCommand) {
        let Some(command) = self.handle_tool_commands(command) else {
            return;
        };
        self.handle_report_and_plugin_commands(command);
    }

    fn handle_tool_commands(&self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::Bash { command, exclude } => self.run_bash(command, exclude),
            LaneCommand::Compact(instructions) => self.run_compact(instructions),
            LaneCommand::Export { path, theme } => self.run_export(path.as_deref(), theme.as_deref()),
            LaneCommand::Mcp(args) => self.run_mcp(args),
            LaneCommand::Usage => match usage_report(&self.link, &self.catalog_env) {
                Ok(text) => self.send(LaneEvent::Notice(text)),
                Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
            },
            LaneCommand::Observer(args) => self.run_observer(args),
            LaneCommand::Goal(args) => self.run_goal(args),
            LaneCommand::Changelog => self.run_changelog(),
            other => return Some(other),
        }
        None
    }

    fn handle_report_and_plugin_commands(&self, command: LaneCommand) {
        let send_fn = |event: LaneEvent| {
            let _ = self.ui.send(UiEvent::Lane(event));
        };
        match command {
            LaneCommand::Share { theme } => {
                let cancel = jsrt::sync::lock(&self.cancels.share).clone();
                share_session(&self.link, &send_fn, &cancel, theme.as_deref(), &self.catalog_env);
            }
            LaneCommand::BugReportSummary(hint) => self.run_bug_report_summary(hint),
            LaneCommand::AbortBugReportSummary => {
                let _ = self.link.op(&Op::AbortBugSummary);
            }
            LaneCommand::BugReport(request) => self.run_bug_report(request),
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
            LaneCommand::ReloadPlugins => self.run_reload_plugins(),
            _ => {}
        }
    }

    fn run_bash(&self, command: String, exclude: bool) {
        let started = start_call(
            &self.link.connection(),
            protocol::names::SESSION_CONTROL,
            "bash",
            json!([command, exclude]),
        );
        let ui = self.ui.clone();
        std::thread::spawn(move || {
            let result =
                started.and_then(|(connection, request)| connection.wait(&request).map_err(client::lost::message));
            let event = LaneEvent::BashDone(result.map(|value| bash_result(&value.unwrap_or_default())));
            let _ = ui.send(UiEvent::Lane(event));
        });
    }

    fn run_compact(&self, instructions: Option<String>) {
        let started = start_call(
            &self.link.connection(),
            protocol::names::AGENT_CONTROLLER,
            "compact",
            json!([{"customInstructions": instructions}]),
        );
        let ui = self.ui.clone();
        std::thread::spawn(move || {
            let result =
                started.and_then(|(connection, request)| connection.wait(&request).map_err(client::lost::message));
            if let Err(error) = result {
                let error = error
                    .strip_prefix("session_operation_failed: ")
                    .map(str::to_string)
                    .unwrap_or(error);
                let _ = ui.send(UiEvent::Lane(LaneEvent::Compacted(Err(error))));
            }
        });
    }

    fn run_export(&self, path: Option<&str>, theme: Option<&str>) {
        self.send(LaneEvent::Status(
            match export_session(&self.link, path, theme, &self.catalog_env) {
                Ok(path) => format!("Session exported to: {path}"),
                Err(error) => format!("Error: Failed to export session: {error}"),
            },
        ));
    }

    fn run_mcp(&self, args: String) {
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

    fn run_observer(&self, args: String) {
        match self.link.op(&Op::Observer { args }) {
            Ok(reply) => self.send(LaneEvent::Notice(
                reply.and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
            )),
            Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
        }
    }

    fn run_goal(&self, args: String) {
        match self.link.op(&Op::Goal { args }) {
            Ok(reply) => self.send(LaneEvent::GoalReply(
                reply
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or("No goal reply")
                    .to_string(),
            )),
            Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
        }
    }

    fn run_changelog(&self) {
        let path = self.catalog_env.package_file("CHANGELOG.md");
        let ui = self.ui.clone();
        std::thread::spawn(move || {
            let content = std::fs::read_to_string(path).ok();
            let tokens = interactive::changelog::changelog_tokens(content.as_deref());
            let _ = ui.send(UiEvent::Lane(LaneEvent::Changelog(tokens)));
        });
    }

    fn run_bug_report_summary(&self, hint: Option<String>) {
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

    fn run_bug_report(&self, request: interactive::lane::BugReportRequest) {
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

    fn run_reload_plugins(&self) {
        self.send(LaneEvent::Keybindings(user_keybindings(
            &self.catalog_env.agent_dir(),
            crate::terminal_settings::platform(&self.catalog_env),
        )));
        self.send(LaneEvent::Status(match reload_plugins(&self.link) {
            Ok(()) => "Reloaded plugins.".into(),
            Err(error) => format!("Error: {error}"),
        }));
    }
}
