//! [`LaneEvent`] dispatch for [`crate::app::App`].

use super::super::*;

impl App {
    pub fn apply(&mut self, event: LaneEvent) {
        let mut event = Some(event);
        event = event.and_then(|e| self.apply_session_event(e));
        event = event.and_then(|e| self.apply_turn_event(e));
        event = event.and_then(|e| self.apply_compaction_event(e));
        event = event.and_then(|e| self.apply_retry_and_tool_event(e));
        event = event.and_then(|e| self.apply_settings_and_auth_event(e));
        event = event.and_then(|e| self.apply_mode_and_notice_event(e));
        event = event.and_then(|e| self.apply_agent_and_inspect_event(e));
        event = event.and_then(|e| self.apply_session_list_event(e));
        event = event.and_then(|e| self.apply_navigation_and_fork_event(e));
        if let Some(e) = event {
            self.apply_status_and_ui_event(e);
        }
    }

    fn apply_session_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::Keybindings(user) => self.set_user_keybindings(user),
            LaneEvent::Opened(opened) => self.apply_opened(opened),
            LaneEvent::Entries(entries) => self.load_entries(&entries),
            LaneEvent::Name(name) => self.apply_name(name),
            LaneEvent::Model(model) => self.apply_model(model),
            LaneEvent::Thinking(level) => {
                self.thinking_level = level;
                self.update_editor_border();
            }
            other => return Some(other),
        }
        None
    }

    fn apply_turn_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::OperationStarted(id) => self.apply_operation_started(id),
            LaneEvent::Streaming(message) => self.chat.set_streaming(&message),
            LaneEvent::Message(message) => self.apply_message(message),
            LaneEvent::ToolStarted { id, name, args } => {
                self.chat.tool_started(&name, &id, args);
                self.refresh_rlm();
            }
            LaneEvent::ToolEnded { .. } => self.refresh_rlm(),
            LaneEvent::Queues(queues) => self.chat.set_queues(queues),
            LaneEvent::OperationEnded { error } => self.apply_operation_ended(error),
            other => return Some(other),
        }
        None
    }

    fn apply_compaction_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::CompactionStarted { reason } => {
                self.compacting = true;
                self.chat.set_compacting_for(&reason);
            }
            LaneEvent::CompactionEnded { status, error } => self.auto_compaction_ended(&status, error),
            LaneEvent::Compacted(result) => self.compacted(result),
            LaneEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error,
            } => {
                self.notice(&self.theme.fg(Fg::Error, &format!("Error: {error}")));
                self.chat.set_retry_indicator(attempt, max_attempts, delay_ms);
            }
            LaneEvent::SummarizationRetryStarted { branch_summary, reason } => {
                self.apply_summarization_retry_started(branch_summary, &reason);
            }
            LaneEvent::SummarizationRetryFinished => self.chat.clear_retry_indicator(),
            other => return Some(other),
        }
        None
    }

    fn apply_retry_and_tool_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
            } => {
                self.retrying = true;
                self.chat.set_retry_indicator(attempt, max_attempts, delay_ms);
            }
            LaneEvent::AutoRetryEnd { attempt, failed } => self.apply_auto_retry_end(attempt, failed),
            LaneEvent::BashDone(result) => self.bash_done(result),
            LaneEvent::BugSummary(result) => self.bug_summary_done(result),
            LaneEvent::BugReport(outcome) => self.bug_report_done(outcome),
            LaneEvent::RlmStatus(listed) => {
                self.rlm_agents = listed;
                self.auto_open_rlm_pane();
                self.wave_active = !self.wave_summary(200).is_empty();
                self.update_work_active();
            }
            LaneEvent::RlmInspected { tag, result } => self.rlm_inspected(&tag, result),
            other => return Some(other),
        }
        None
    }

    fn apply_settings_and_auth_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::Settings {
                settings,
                env_overrides,
                themes,
                terminal_theme,
            } => self.apply_settings(settings, env_overrides, themes, terminal_theme),
            LaneEvent::LoginProviders { providers, query } => self.login(providers, &query),
            LaneEvent::Login(id, step) => self.login_step(id, step),
            LaneEvent::Credentials(stored) => self.apply_credentials(stored),
            LaneEvent::Catalog(models) => self.apply_catalog(models),
            LaneEvent::CatalogRefreshed(failed) => self.catalog_refreshed(failed),
            LaneEvent::Trust(view) => {
                let selector =
                    crate::trust_selector::TrustSelector::new(Rc::clone(&self.theme), self.keys.clone(), view);
                self.set_panel(Some(Panel::Trust(Box::new(selector))));
                self.editor.focused = false;
            }
            other => return Some(other),
        }
        None
    }

    fn apply_mode_and_notice_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::Mode {
                engineering,
                always_skills,
            } => self.mode_read(engineering, always_skills),
            LaneEvent::Persona(persona) => self.show_status(&crate::modes::persona_status(persona.as_deref())),
            LaneEvent::AutoreviewAccounts(accounts) => self.autoreview_accounts(accounts),
            LaneEvent::Notice(text) => self.notice(&text),
            LaneEvent::GoalReply(text) => self.notice(&self.theme.fg(Fg::Dim, &text)),
            LaneEvent::Changelog(tokens) => {
                let notice = crate::commands::LexedNotice {
                    theme: Rc::clone(&self.theme),
                    title: "What's New".into(),
                    tokens,
                };
                self.chat.append_notice(Box::new(notice));
            }
            LaneEvent::StartupNotice(notice) => self.apply_startup_notice(notice),
            other => return Some(other),
        }
        None
    }

    fn apply_agent_and_inspect_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::Agents(listed) => self.apply_agents(listed),
            LaneEvent::AgentTranscript { id, messages } => self.apply_agent_transcript(id, messages),
            LaneEvent::AgentAnswered { id, action, answer } => {
                self.agent_notice(crate::agent_hub::agent_notice(&id, action, answer));
            }
            LaneEvent::Inspected { title, result } => self.apply_inspected(&title, result),
            LaneEvent::ShareLoader(active) => self.apply_share_loader(active),
            other => return Some(other),
        }
        None
    }

    fn apply_session_list_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::AllSessions(sessions) => {
                if let Some(Panel::Sessions(selector)) = self.panel.as_mut() {
                    selector.set_all_sessions(sessions);
                }
            }
            LaneEvent::PiAllSessions(sessions) => {
                if let Some(Panel::PiSessions(selector)) = self.panel.as_mut() {
                    selector.set_all_sessions(sessions);
                }
            }
            LaneEvent::Sessions(sessions) => self.show_resume(sessions),
            LaneEvent::PiSessions(sessions) => self.show_pi_sessions(sessions),
            LaneEvent::SessionDeleted(result) => self.apply_session_deleted(result),
            LaneEvent::PiImported {
                imported,
                source,
                session_file,
                current,
            } => self.apply_pi_imported(imported, source, session_file, current),
            other => return Some(other),
        }
        None
    }

    fn apply_navigation_and_fork_event(&mut self, event: LaneEvent) -> Option<LaneEvent> {
        match event {
            LaneEvent::Tree { tree, leaf } => self.show_tree(tree, leaf, None),
            LaneEvent::FilesRestored(result) => self.apply_files_restored(result),
            LaneEvent::Navigated(result) => self.apply_navigated(result),
            LaneEvent::ForkMessages(messages) => self.apply_fork_messages(messages),
            LaneEvent::Forked { editor_text, status } => {
                self.editor.set_text(&editor_text);
                self.update_editor_border();
                self.show_status(&status);
            }
            other => return Some(other),
        }
        None
    }

    fn apply_status_and_ui_event(&mut self, event: LaneEvent) {
        match event {
            LaneEvent::Status(text) => self.show_status(&text),
            LaneEvent::Recovery { busy, status } => {
                self.recovering = busy;
                self.show_status(&status);
            }
            LaneEvent::Error(text) => self.show_status(&format!("Error: {text}")),
            LaneEvent::ExtensionUi(request) => self.extension_ui(request),
            LaneEvent::DebugLogWritten { path, summary } => {
                let notice = crate::debug_log::DebugNotice {
                    theme: Rc::clone(&self.theme),
                    path,
                    summary,
                };
                self.chat.append_notice(Box::new(notice));
            }
            _ => {}
        }
    }

    fn apply_operation_started(&mut self, id: String) {
        self.operation_started_ms = Some(jsrt::date::now_ms() as f64);
        self.operation = Some(id);
        self.aborting = false;
        self.turn_started = Some(self.now());
        self.rlm_polled = None;
        self.chat.set_working(true);
    }

    fn apply_message(&mut self, message: Value) {
        self.chat.add_message(&message);
        if let Some(notice) = crate::lane::route_notice(&message) {
            self.notice(&self.theme.fg(Fg::Warning, &notice));
        }
        self.transcript.push(message);
    }

    fn apply_operation_ended(&mut self, error: Option<String>) {
        if std::mem::take(&mut self.retrying) {
            self.chat.clear_retry_indicator();
        }
        self.turn_ended(error.is_some());
        self.operation = None;
        self.rlm_polled = None;
        self.chat.set_working(false);
        self.chat.set_queues(Vec::new());
        self.status = error.map_or_default(|e| format!("Operation failed: {e}"));
    }

    fn apply_name(&mut self, name: Option<String>) {
        let confirmed = name.is_some() && self.pending_name == name;
        self.session_name = name;
        self.update_title();
        if let Some(name) = confirmed.then(|| self.pending_name.take()).flatten() {
            self.notice(&self.theme.fg(Fg::Dim, &format!("Session name set: {name}")));
        }
    }

    fn apply_model(&mut self, model: ModelInfo) {
        self.model = model;
        if self.session_id.is_some() {
            self.check_subscription_auth();
        }
    }

    fn apply_summarization_retry_started(&mut self, branch_summary: bool, reason: &str) {
        self.chat.clear_retry_indicator();
        if branch_summary {
            self.chat.set_branch_summary_indicator();
        } else {
            self.chat.set_compacting_for(reason);
        }
    }

    fn apply_auto_retry_end(&mut self, attempt: u64, failed: Option<String>) {
        self.retrying = false;
        self.chat.clear_retry_indicator();
        if let Some(error) = failed {
            let text = format!("Error: Retry failed after {attempt} attempts: {error}");
            self.notice(&self.theme.fg(Fg::Error, &text));
        }
    }

    fn apply_share_loader(&mut self, active: bool) {
        if active {
            let loader = crate::extension_dialogs::BorderedLoader::new(
                Rc::clone(&self.theme),
                self.keys.clone(),
                "Creating gist...",
            );
            self.editor.focused = false;
            self.set_panel(Some(Panel::ShareLoader(Box::new(loader))));
        } else if matches!(self.panel, Some(Panel::ShareLoader(_))) {
            self.set_panel(None);
        }
    }

    fn apply_catalog(&mut self, models: Vec<ModelInfo>) {
        if let Some(current) = models
            .iter()
            .find(|m| m.provider == self.model.provider && m.id == self.model.id)
        {
            self.model = current.clone();
        }
        self.models = models;
    }

    fn apply_startup_notice(&mut self, notice: crate::lane::StartupNotice) {
        match self.session_id {
            Some(_) => self.show_startup_notice(notice),
            None => self.early_startup_notices.push(notice),
        }
    }

    fn apply_session_deleted(&mut self, result: Result<(), String>) {
        if let Some(Panel::Sessions(selector)) = self.panel.as_mut() {
            match result {
                Ok(()) => selector.set_status(false, "Session moved to trash", Duration::from_secs(2)),
                Err(error) => selector.set_status(true, &error, Duration::from_secs(3)),
            }
        }
    }

    fn apply_files_restored(&mut self, result: Result<String, String>) {
        match result {
            Ok(message) => {
                self.show_status(&message);
                if let Some(entry) = self.rewind_after_restore.take() {
                    if self.operation.is_some() {
                        self.send(LaneCommand::Abort);
                    }
                    self.send(LaneCommand::Fork(entry));
                }
            }
            Err(error) => {
                self.rewind_after_restore = None;
                self.show_status(&format!("Error: {error}"));
            }
        }
    }

    fn apply_agent_transcript(&mut self, id: String, messages: Result<Vec<Value>, String>) {
        match messages {
            Ok(messages) => {
                if let Some(Overlay::AgentHub(hub)) = self.overlay.as_mut() {
                    let entries = messages
                        .into_iter()
                        .enumerate()
                        .map(|(i, message)| {
                            let at = message["timestamp"].as_i64().map_or_default(|t| t.to_string());
                            crate::copy_targets::TranscriptEntry::new(&format!("{id}#{i}"), &at, message)
                        })
                        .collect();
                    hub.set_transcript(&id, entries);
                }
            }
            Err(error) => self.agent_notice(format!("Error: {error}")),
        }
    }

    fn apply_inspected(&mut self, title: &str, result: Result<Value, String>) {
        self.show_status(&match result {
            Ok(value) => crate::inspection::format_inspection(title, &value),
            Err(error) => format!("Error: {error}"),
        });
    }

    fn apply_fork_messages(&mut self, messages: Vec<crate::lane::ForkMessage>) {
        if self.rewind_pending {
            self.rewind_pending = false;
            self.open_rewind(messages);
            return;
        }
        if messages.is_empty() {
            self.show_status("No messages to fork from");
            return;
        }
        let last = messages.last().map(|m| m.entry_id.clone());
        self.set_panel(Some(Panel::Fork(UserMessageSelector::new(
            Rc::clone(&self.theme),
            messages,
            last.as_deref(),
        ))));
        self.editor.focused = false;
    }
}
