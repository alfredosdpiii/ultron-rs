//! Lane event dispatch, extension UI dialogs, compaction, bash, and bug-report lifecycle for [`super::App`].

use super::*;

impl App {
    pub(super) fn apply_opened(&mut self, opened: crate::lane::SessionOpened) {
        let switched = self.session_id.is_some();
        self.chat.clear();
        self.transcript.clear();
        for message in &opened.messages {
            self.chat.add_message(message);
        }
        self.transcript = opened.messages;
        self.context_from = 0;
        self.compacted = false;
        self.set_panel(None);
        self.path_usage = Value::Null;
        self.session_id = Some(opened.session_id);
        self.session_file = opened.session_file;
        self.session_name = opened.name;
        self.pending_name = None;
        self.reset_run_status();
        let prompts: Vec<TimedPrompt> = self
            .transcript
            .iter()
            .filter(|m| m["role"] == "user")
            .map(|m| TimedPrompt {
                text: message_text(m).trim().to_string(),
                timestamp: m["timestamp"].as_i64().unwrap_or(0),
            })
            .filter(|p| !p.text.is_empty())
            .collect();
        let history = self.history.history(&prompts);
        self.editor
            .set_history(&history.iter().map(String::as_str).collect::<Vec<_>>());
        if !switched {
            self.status.clear();
            if self.transcript.is_empty() {
                self.send(LaneCommand::StartupChangelog);
            }
            for notice in std::mem::take(&mut self.early_startup_notices) {
                self.show_startup_notice(notice);
            }
            self.check_subscription_auth();
        }
        self.update_title();
    }

    pub(super) fn apply_settings(
        &mut self,
        settings: Value,
        env_overrides: std::collections::BTreeMap<String, (String, String)>,
        themes: Vec<String>,
        terminal_theme: String,
    ) {
        let (themes, terminal_theme) = match &self.theme_controller {
            Some(controller) => (controller.available(), controller.terminal_theme().to_string()),
            None => (themes, terminal_theme),
        };
        let input = SettingsInput {
            settings,
            env_overrides,
            models: self.models.clone(),
            session: (!self.model.id.is_empty()).then(|| self.model.clone()),
            themes,
            terminal_theme,
            supports_images: self.image_protocol.is_some(),
            follow_up_key: key_display_text(&self.keys, "app.message.followUp"),
            cycle_thinking_key: key_display_text(&self.keys, "app.thinking.cycle"),
            rust_rows: self.qol.any(),
        };
        let selector = SettingsSelector::new(Rc::clone(&self.theme), self.keys.clone(), input);
        self.set_panel(Some(Panel::Settings(Box::new(selector))));
        self.editor.focused = false;
    }

    pub(super) fn apply_credentials(&mut self, stored: Vec<(String, String)>) {
        if stored.is_empty() {
            self.show_status(
                "No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.",
            );
            return;
        }
        let mut providers: Vec<crate::selectors::AuthProvider> = stored
            .into_iter()
            .map(|(id, kind)| crate::selectors::AuthProvider {
                name: crate::selectors::provider_name(&id),
                status: Some((kind.clone(), Some("stored credential".into()))),
                id,
                auth_type: kind,
                api_key_method: None,
                login_label: None,
                generic_login: false,
            })
            .collect();
        providers.sort_by(|a, b| jsrt::collate::locale_compare(&a.name, &b.name));
        let selector =
            crate::selectors::AuthSelector::new(Rc::clone(&self.theme), self.keys.clone(), false, providers, None);
        self.set_panel(Some(Panel::Logout(Box::new(selector))));
        self.editor.focused = false;
    }

    pub(super) fn apply_agents(&mut self, listed: Value) {
        let agents = crate::agent_hub::agent_views(&listed);
        if let Some(Overlay::AgentHub(hub)) = self.overlay.as_mut() {
            hub.set_agents(agents, true);
            return;
        }
        let style = self.chat.style();
        let theme = Rc::clone(&style.theme);
        let (highlight, code, expand, dequeue, interrupt) = (
            style.highlight,
            style.code_highlight,
            style.expand_key,
            style.dequeue_key,
            style.interrupt_key,
        );
        let mut hub = AgentHub::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            Rc::new(move || ChatStyle {
                theme: Rc::clone(&theme),
                highlight: Rc::clone(&highlight),
                code_highlight: code.clone(),
                expand_key: expand.clone(),
                dequeue_key: dequeue.clone(),
                interrupt_key: interrupt.clone(),
            }),
        );
        hub.set_agents(agents, true);
        self.overlay = Some(Overlay::AgentHub(Box::new(hub)));
    }

    pub(super) fn apply_navigated(&mut self, result: Result<Option<String>, String>) {
        self.chat.set_navigating(false);
        let summarized = self.summarizing_branch.take();
        match result {
            Err(error) if error == BRANCH_SUMMARY_ABORTED => {
                self.show_status(BRANCH_SUMMARY_ABORTED);
                if let (Some(entry), Some((tree, leaf))) = (summarized, self.tree.clone()) {
                    self.show_tree(tree, leaf, Some(&entry));
                }
            }
            Ok(text) => {
                if let Some(text) = text.filter(|t| !t.is_empty())
                    && self.editor.text().trim().is_empty()
                {
                    self.editor.set_text(&text);
                    self.update_editor_border();
                }
                self.show_status("Navigated to selected point");
            }
            Err(error) => self.show_status(&format!("Error: {error}")),
        }
    }

    pub(super) fn apply_pi_imported(
        &mut self,
        imported: String,
        source: String,
        session_file: Option<String>,
        current: bool,
    ) {
        if current {
            self.show_status(&format!("{imported} (the current session)"));
            return;
        }
        let items = ["Switch to the imported session", "Stay here"]
            .into_iter()
            .map(|choice| SelectItem {
                value: choice.into(),
                label: choice.into(),
                description: None,
            })
            .collect();
        let title = format!("{imported}. Switch to it?");
        self.open_selection(
            &title,
            items,
            None,
            Purpose::SwitchImported {
                session_file,
                imported,
                source,
            },
        );
    }

    pub fn apply(&mut self, event: LaneEvent) {
        match event {
            LaneEvent::Keybindings(user) => self.set_user_keybindings(user),
            LaneEvent::Opened(opened) => self.apply_opened(opened),
            LaneEvent::Entries(entries) => self.load_entries(&entries),
            LaneEvent::OperationStarted(id) => {
                self.operation_started_ms = Some(jsrt::date::now_ms() as f64);
                self.operation = Some(id);
                self.aborting = false;
                self.turn_started = Some(self.now());
                self.rlm_polled = None;
                self.chat.set_working(true);
            }
            LaneEvent::Streaming(message) => self.chat.set_streaming(&message),
            LaneEvent::Message(message) => {
                self.chat.add_message(&message);
                if let Some(notice) = crate::lane::route_notice(&message) {
                    self.notice(&self.theme.fg(Fg::Warning, &notice));
                }
                self.transcript.push(message);
            }
            LaneEvent::ToolStarted { id, name, args } => {
                self.chat.tool_started(&name, &id, args);
                self.refresh_rlm();
            }
            LaneEvent::ToolEnded { .. } => self.refresh_rlm(),
            LaneEvent::Queues(queues) => self.chat.set_queues(queues),
            LaneEvent::OperationEnded { error } => {
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
            LaneEvent::Name(name) => {
                let confirmed = name.is_some() && self.pending_name == name;
                self.session_name = name;
                self.update_title();
                if let Some(name) = confirmed.then(|| self.pending_name.take()).flatten() {
                    self.notice(&self.theme.fg(Fg::Dim, &format!("Session name set: {name}")));
                }
            }
            LaneEvent::Model(model) => {
                self.model = model;
                if self.session_id.is_some() {
                    self.check_subscription_auth();
                }
            }
            LaneEvent::Thinking(level) => {
                self.thinking_level = level;
                self.update_editor_border();
            }
            LaneEvent::Notice(text) => self.notice(&text),
            LaneEvent::GoalReply(text) => self.notice(&self.theme.fg(Fg::Dim, &text)),
            LaneEvent::BashDone(result) => self.bash_done(result),
            LaneEvent::BugSummary(result) => self.bug_summary_done(result),
            LaneEvent::BugReport(outcome) => self.bug_report_done(outcome),
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
                self.chat.clear_retry_indicator();
                if branch_summary {
                    self.chat.set_branch_summary_indicator();
                } else {
                    self.chat.set_compacting_for(&reason);
                }
            }
            LaneEvent::SummarizationRetryFinished => self.chat.clear_retry_indicator(),
            LaneEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
            } => {
                self.retrying = true;
                self.chat.set_retry_indicator(attempt, max_attempts, delay_ms);
            }
            LaneEvent::AutoRetryEnd { attempt, failed } => {
                self.retrying = false;
                self.chat.clear_retry_indicator();
                if let Some(error) = failed {
                    let text = format!("Error: Retry failed after {attempt} attempts: {error}");
                    self.notice(&self.theme.fg(Fg::Error, &text));
                }
            }
            LaneEvent::Tree { tree, leaf } => self.show_tree(tree, leaf, None),
            LaneEvent::RlmStatus(listed) => {
                self.rlm_agents = listed;
                self.auto_open_rlm_pane();
                self.wave_active = !self.wave_summary(200).is_empty();
                self.update_work_active();
            }
            LaneEvent::RlmInspected { tag, result } => self.rlm_inspected(&tag, result),
            LaneEvent::ShareLoader(true) => {
                let loader = crate::extension_dialogs::BorderedLoader::new(
                    Rc::clone(&self.theme),
                    self.keys.clone(),
                    "Creating gist...",
                );
                self.editor.focused = false;
                self.set_panel(Some(Panel::ShareLoader(Box::new(loader))));
            }
            LaneEvent::ShareLoader(false) => {
                if matches!(self.panel, Some(Panel::ShareLoader(_))) {
                    self.set_panel(None);
                }
            }
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
            LaneEvent::Mode {
                engineering,
                always_skills,
            } => self.mode_read(engineering, always_skills),
            LaneEvent::Persona(persona) => self.show_status(&crate::modes::persona_status(persona.as_deref())),
            LaneEvent::AutoreviewAccounts(accounts) => self.autoreview_accounts(accounts),
            LaneEvent::Settings {
                settings,
                env_overrides,
                themes,
                terminal_theme,
            } => self.apply_settings(settings, env_overrides, themes, terminal_theme),
            LaneEvent::LoginProviders { providers, query } => self.login(providers, &query),
            LaneEvent::Login(id, step) => self.login_step(id, step),
            LaneEvent::Changelog(tokens) => {
                let notice = crate::commands::LexedNotice {
                    theme: Rc::clone(&self.theme),
                    title: "What's New".into(),
                    tokens,
                };
                self.chat.append_notice(Box::new(notice));
            }
            LaneEvent::Credentials(stored) => self.apply_credentials(stored),
            LaneEvent::Catalog(models) => {
                if let Some(current) = models
                    .iter()
                    .find(|m| m.provider == self.model.provider && m.id == self.model.id)
                {
                    self.model = current.clone();
                }
                self.models = models;
            }
            LaneEvent::CatalogRefreshed(failed) => self.catalog_refreshed(failed),
            LaneEvent::StartupNotice(notice) => match self.session_id {
                Some(_) => self.show_startup_notice(notice),
                None => self.early_startup_notices.push(notice),
            },
            LaneEvent::SessionDeleted(result) => {
                if let Some(Panel::Sessions(selector)) = self.panel.as_mut() {
                    match result {
                        Ok(()) => selector.set_status(false, "Session moved to trash", Duration::from_secs(2)),
                        Err(error) => selector.set_status(true, &error, Duration::from_secs(3)),
                    }
                }
            }
            LaneEvent::FilesRestored(result) => match result {
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
            },
            LaneEvent::Navigated(result) => self.apply_navigated(result),
            LaneEvent::AgentTranscript {
                id,
                messages: Ok(messages),
            } => {
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
            LaneEvent::AgentTranscript {
                messages: Err(error), ..
            } => self.agent_notice(format!("Error: {error}")),
            LaneEvent::AgentAnswered { id, action, answer } => {
                self.agent_notice(crate::agent_hub::agent_notice(&id, action, answer));
            }
            LaneEvent::Inspected { title, result } => self.show_status(&match result {
                Ok(value) => crate::inspection::format_inspection(&title, &value),
                Err(error) => format!("Error: {error}"),
            }),
            LaneEvent::Agents(listed) => self.apply_agents(listed),
            LaneEvent::ForkMessages(messages) if self.rewind_pending => {
                self.rewind_pending = false;
                self.open_rewind(messages);
            }
            LaneEvent::ForkMessages(messages) => {
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
            LaneEvent::Forked { editor_text, status } => {
                self.editor.set_text(&editor_text);
                self.update_editor_border();
                self.show_status(&status);
            }
            LaneEvent::PiImported {
                imported,
                source,
                session_file,
                current,
            } => self.apply_pi_imported(imported, source, session_file, current),
            LaneEvent::Sessions(sessions) => self.show_resume(sessions),
            LaneEvent::PiSessions(sessions) => self.show_pi_sessions(sessions),
            LaneEvent::Status(text) => self.show_status(&text),
            LaneEvent::Recovery { busy, status } => {
                self.recovering = busy;
                self.show_status(&status);
            }
            LaneEvent::Error(text) => self.show_status(&format!("Error: {text}")),
            LaneEvent::ExtensionUi(request) => self.extension_ui(request),
            LaneEvent::Trust(view) => {
                let selector =
                    crate::trust_selector::TrustSelector::new(Rc::clone(&self.theme), self.keys.clone(), view);
                self.set_panel(Some(Panel::Trust(Box::new(selector))));
                self.editor.focused = false;
            }
            LaneEvent::DebugLogWritten { path, summary } => {
                let notice = crate::debug_log::DebugNotice {
                    theme: Rc::clone(&self.theme),
                    path,
                    summary,
                };
                self.chat.append_notice(Box::new(notice));
            }
        }
    }

    /// `#serveExtensionUI` for `/mcp` and the MCP gateway: dialogs open the selector or a text input (one at a
    /// time, in order), `notify` shows in the status line.
    pub(super) fn extension_ui(&mut self, request: Value) {
        match request["method"].as_str().unwrap_or_default() {
            "select" | "input" => {
                self.extension_dialogs.push_back(request);
                self.next_extension_dialog();
            }
            "notify" => self.show_status(request["message"].as_str().unwrap_or_default()),
            _ => {}
        }
    }

    /// A `/mcp` dialog is open: the panel or the selection shows one (derived, so a dialog that was displaced is
    /// never counted as open).
    pub(super) fn extension_dialog_open(&self) -> bool {
        matches!(self.panel, Some(Panel::ExtensionInput { .. }))
            || matches!(
                self.selection,
                Some(Selection {
                    purpose: Purpose::ExtensionSelect { .. },
                    ..
                })
            )
    }

    pub(super) fn next_extension_dialog(&mut self) {
        if self.extension_dialog_open() {
            return;
        }
        let Some(request) = self.extension_dialogs.pop_front() else {
            return;
        };
        let id = request["id"].as_str().unwrap_or_default().to_string();
        let title = request["title"].as_str().unwrap_or_default();
        match request["method"].as_str().unwrap_or_default() {
            "select" => {
                let items = request["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|o| SelectItem {
                        value: o.into(),
                        label: o.into(),
                        description: None,
                    })
                    .collect();
                self.open_selection(title, items, None, Purpose::ExtensionSelect { id });
            }
            _ => {
                let dialog = crate::extension_dialogs::ExtensionInputDialog::new(
                    Rc::clone(&self.theme),
                    self.keys.clone(),
                    title,
                    request["timeout"].as_u64(),
                    None,
                    self.now(),
                );
                self.editor.focused = false;
                self.set_panel(Some(Panel::ExtensionInput {
                    id,
                    dialog: Box::new(dialog),
                }));
            }
        }
    }

    /// An input dialog whose countdown ran out cancels (`CountdownTimer` `onExpire`).
    pub fn expire_dialogs(&mut self) {
        let now = self.now();
        if let Some(Panel::ExtensionInput { id, dialog }) = &self.panel
            && dialog.expired(now)
        {
            let id = id.clone();
            self.panel.take();
            self.answer_extension_dialog(&id, serde_json::json!({ "cancelled": true }));
        }
    }

    /// Answer the open `/mcp` dialog and show the next one.
    pub(super) fn answer_extension_dialog(&mut self, id: &str, answer: Value) {
        self.send(LaneCommand::ExtensionUiRespond {
            id: id.to_string(),
            answer,
        });
        self.next_extension_dialog();
    }

    /// The one way a panel is shown or closed: the panel it replaces is dismissed first, settling what it still owes
    /// another party (a `/mcp` input dialog is answered cancelled, a login is cancelled), so nothing waits on a
    /// dialog that is no longer shown. A handler that answers its panel's obligation itself takes the panel
    /// (`self.panel.take()`) instead. Closing the panel shows the next queued `/mcp` dialog.
    pub(super) fn set_panel(&mut self, panel: Option<Panel>) {
        let closing = panel.is_none();
        match std::mem::replace(&mut self.panel, panel) {
            Some(Panel::ExtensionInput { id, .. }) => self.send(LaneCommand::ExtensionUiRespond {
                id,
                answer: serde_json::json!({ "cancelled": true }),
            }),
            Some(Panel::OAuthLogin(_, LoginFlow::Client(id))) => {
                self.send(LaneCommand::LoginReply { id, answer: None });
            }
            _ => {}
        }
        if closing {
            self.next_extension_dialog();
        }
    }

    /// Turn bookkeeping of the additions: active time, the git refresh and the turn-end notification.
    pub(super) fn turn_ended(&mut self, failed: bool) {
        if let Some(started) = self.turn_started.take() {
            self.active_ms += started.elapsed().as_millis() as u64;
        }
        if let Some(git) = &self.git {
            git.refresh();
        }
        let aborted = std::mem::take(&mut self.aborting);
        if !self.qol.notifications || aborted || self.operation.is_none() {
            return;
        }
        let errored = failed
            || self
                .transcript
                .iter()
                .rev()
                .find(|m| m["role"] == "assistant")
                .is_some_and(|m| m["stopReason"] == "error");
        let outcome = if errored {
            crate::notify::TurnOutcome::Error
        } else {
            crate::notify::TurnOutcome::Complete
        };
        let message = crate::notify::Message::turn(self.session_name.as_deref(), outcome);
        if let Some(notify) = self.notify.as_mut() {
            notify(message);
        }
    }

    /// The agent hub's notice, or a status line when the hub is closed.
    pub(super) fn agent_notice(&mut self, notice: String) {
        match self.overlay.as_mut() {
            Some(Overlay::AgentHub(hub)) => hub.set_notice(Some(notice)),
            _ => self.show_status(&notice),
        }
    }

    pub(super) fn send(&self, command: LaneCommand) {
        let _ = self.lane.send(command);
    }

    /// Queue `text` (and its images) as a steering message or a follow-up; returns its entry id.
    pub(super) fn enqueue(&self, kind: QueueKind, text: String, images: Vec<PromptImage>) -> String {
        let entry_id = jsrt::random::uuid_v4();
        self.send(LaneCommand::Enqueue {
            kind,
            text,
            images,
            entry_id: entry_id.clone(),
        });
        entry_id
    }

    /// `showStatus`: short text in the status line; more than three lines go to the transcript.
    pub(super) fn show_status(&mut self, text: &str) {
        if text.split('\n').count() > 3 {
            self.chat.append_notice_text(text);
            self.status.clear();
        } else {
            self.status = text.to_string();
        }
    }

    pub(super) fn notice(&mut self, text: &str) {
        self.chat.append_notice_text(text);
    }

    /// The terminal draws tool images inline with `protocol` (Kitty; TypeScript's alternate screen turns iTerm2
    /// images off): tool results draw their images, and the terminal is asked for its cell size (`CSI 16 t`,
    /// `queryCellSize`), whose answer arrives as input.
    pub(super) fn show_error(&mut self, message: &str) {
        let text = self.theme.fg(Fg::Error, &format!("Error: {message}"));
        self.notice(&text);
    }

    /// `/bug [description]` (`handleBugCommand`): the report's dialogs in the editor's place.
    pub(super) fn open_bug_report(&mut self, args: &str) {
        let hint = args.trim();
        let model = (!self.model.name.is_empty()).then(|| (self.model.name.clone(), self.model.provider.clone()));
        let flow = crate::bug_report::BugFlow::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            self.keys.clone(),
            (!hint.is_empty()).then_some(hint),
            model,
            self.bug_report_target.clone(),
        );
        self.editor.focused = false;
        self.set_panel(Some(Panel::BugReport(Box::new(flow))));
    }

    pub(super) fn bug_cancelled(&mut self) {
        self.set_panel(None);
        self.show_status("Bug report cancelled");
    }

    /// What a key in the `/bug` flow asks for.
    pub(super) fn bug_action(&mut self, action: crate::bug_report::Action) {
        use crate::bug_report::Action;
        let Some(Panel::BugReport(flow)) = self.panel.as_mut() else {
            return;
        };
        match action {
            Action::None => {}
            Action::Cancelled => self.bug_cancelled(),
            Action::Offline => {
                self.set_panel(None);
                self.show_error("Uploading bug reports requires online mode. Use Export as Zip instead.");
            }
            Action::Summarize => {
                let hint = flow.choices.hint.clone();
                self.send(LaneCommand::BugReportSummary(hint));
            }
            Action::CancelSummary => {
                self.send(LaneCommand::AbortBugReportSummary);
                self.bug_cancelled();
            }
            Action::Deliver => {
                let choices = flow.choices.clone();
                let request = crate::lane::BugReportRequest {
                    hint: choices.hint,
                    include_session: choices.include_session,
                    summary: flow.summary.clone(),
                    upload: choices.upload,
                };
                if !request.upload {
                    self.set_panel(None);
                }
                self.send(LaneCommand::BugReport(request));
            }
            Action::CancelUpload => {
                self.send(LaneCommand::CancelBugReport);
                self.bug_cancelled();
            }
            Action::ExportAfterFailure => {
                self.set_panel(None);
                self.send(LaneCommand::BugReportExport);
            }
            Action::External(text) => {
                if self.external_editor.is_some() {
                    self.edit_dialog = true;
                    self.edit_request = Some(text);
                } else {
                    self.show_status("No editor configured. Set $VISUAL or $EDITOR environment variable.");
                }
            }
        }
    }

    /// The summary arrived; a flow that was cancelled meanwhile ignores it.
    pub(super) fn bug_summary_done(&mut self, result: Result<String, String>) {
        let Some(Panel::BugReport(flow)) = self.panel.as_mut() else {
            return;
        };
        if !matches!(flow.step, crate::bug_report::Step::Summarizing(_)) {
            return;
        }
        match result {
            Ok(summary) => {
                let action = flow.summarized(summary);
                self.bug_action(action);
            }
            Err(error) => {
                self.set_panel(None);
                self.show_error(&format!("Failed to write bug report summary: {error}"));
            }
        }
    }

    pub(super) fn bug_report_done(&mut self, outcome: crate::lane::BugReportOutcome) {
        use crate::lane::BugReportOutcome;
        let uploading = matches!(
            &self.panel,
            Some(Panel::BugReport(flow)) if matches!(flow.step, crate::bug_report::Step::Uploading(_))
        );
        match outcome {
            BugReportOutcome::UploadFailed(failure) => {
                if let (true, Some(Panel::BugReport(flow))) = (uploading, self.panel.as_mut()) {
                    flow.upload_failed(&failure);
                }
            }
            outcome => {
                if uploading {
                    self.set_panel(None);
                }
                match outcome {
                    BugReportOutcome::Uploaded(id) => {
                        self.show_status(&format!("Bug report uploaded. Report ID: {id}"));
                    }
                    BugReportOutcome::Exported { path, id } => {
                        self.show_status(&format!("Bug report exported to: {path}\nReport ID: {id}"));
                    }
                    BugReportOutcome::Failed(error) => self.show_error(&error),
                    BugReportOutcome::UploadFailed(_) => {}
                }
            }
        }
    }

    /// Pi's `!` command: output shows while it runs; the recorded result joins the transcript.
    pub(super) fn run_bash(&mut self, command: &str, excluded: bool) {
        self.editor.set_text("");
        if self.bash.is_some() {
            self.show_status("A bash command is already running. Press Esc to cancel it first.");
            self.editor
                .set_text(&format!("{}{command}", if excluded { "!!" } else { "!" }));
            self.update_editor_border();
            return;
        }
        self.bash = Some(self.chat.bash_component(command, excluded));
        self.send(LaneCommand::Bash {
            command: command.to_string(),
            exclude: excluded,
        });
        self.update_editor_border();
    }

    pub(super) fn bash_done(&mut self, result: Result<BashResult, String>) {
        self.bash = None;
        if let Err(error) = result {
            self.status = format!("Error: {error}");
        }
    }

    /// A session opened (a switch, a worker restart, a reattach): nothing the old lane was doing runs any more, so
    /// every run status goes with the operation; a later run or compaction reports itself again. Before, a worker
    /// lost mid-compaction or mid-retry left `compacting`/`retrying` set with no indicator, and Esc then cancelled a
    /// compaction that no longer existed.
    pub(super) fn reset_run_status(&mut self) {
        if let Some(started) = self.turn_started.take() {
            self.active_ms += started.elapsed().as_millis() as u64;
        }
        self.operation = None;
        self.compacting = false;
        self.retrying = false;
        self.aborting = false;
        self.summarizing_branch = None;
    }

    /// After a compaction the run it happened in (if any) is still working.
    pub(super) fn compaction_over(&mut self) {
        self.compacting = false;
        self.chat.set_compacting(false);
        if self.operation.is_some() {
            self.chat.set_working(true);
        }
    }

    /// An automatic compaction ended without a compaction entry (interactive-mode.ts `compaction_end`): a failure is
    /// shown in the transcript, a cancellation in the status line; a declined one (trimmed instead) shows nothing.
    pub(super) fn auto_compaction_ended(&mut self, status: &str, error: Option<String>) {
        self.compaction_over();
        match status {
            "aborted" => self.status = "Auto-compaction cancelled".into(),
            "failed" => {
                let message = error.unwrap_or_else(|| "Auto-compaction failed".into());
                self.notice(&self.theme.fg(Fg::Error, &message));
            }
            _ => {}
        }
    }

    pub(super) fn compacted(&mut self, result: Result<Value, String>) {
        match result {
            Ok(entry) => {
                self.chat.clear();
                self.chat.add_entry(&entry);
                self.context_from = self.transcript.len();
                self.compacted = true;
                self.status.clear();
            }
            Err(error) => {
                let error = if error.starts_with("Nothing to compact") || error == "Already compacted" {
                    "Lane \"main\" has nothing to compact".to_string()
                } else {
                    error
                };
                self.status = if error == "Compaction cancelled" {
                    error
                } else {
                    format!("Operation rejected: {error}")
                };
            }
        }
        self.compaction_over();
    }

    /// Redraw the transcript from the lane's entries (`#syncTranscript` of a fresh snapshot): compactions with their
    /// retained tails, branch summaries; the context restarts after the last compaction, and branch summaries'
    /// usage joins the footer's totals as the lane's usage rows do.
    pub(super) fn load_entries(&mut self, entries: &[Value]) {
        if entries.is_empty() {
            return;
        }
        let start = entries.iter().rposition(|e| e["type"] == "compaction").unwrap_or(0);
        let entries = &entries[start..];
        self.chat.clear();
        let mut messages = Vec::new();
        let mut compacted_at = None;
        self.path_usage = Value::Null;
        let mut usage = serde_json::Map::new();
        for entry in entries {
            self.chat.add_entry(entry);
            match entry["type"].as_str() {
                Some("compaction") => {
                    messages.extend(entry["retainedTail"].as_array().cloned().unwrap_or_default());
                    compacted_at = Some(messages.len());
                }
                Some("message") => messages.push(entry["message"].clone()),
                Some("branch_summary") => {
                    for key in ["input", "output", "cacheRead", "cacheWrite"] {
                        let add = entry["usage"][key].as_u64().unwrap_or(0);
                        let sum = usage.get(key).and_then(Value::as_u64).unwrap_or(0) + add;
                        usage.insert(key.into(), sum.into());
                    }
                    let cost = usage.get("cost").and_then(Value::as_f64).unwrap_or(0.0)
                        + entry["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
                    usage.insert("cost".into(), cost.into());
                }
                _ => {}
            }
        }
        if !usage.is_empty() {
            self.path_usage = Value::Object(usage);
        }
        self.transcript = messages;
        self.compacted = compacted_at.is_some();
        self.context_from = compacted_at.unwrap_or(0);
    }
}
