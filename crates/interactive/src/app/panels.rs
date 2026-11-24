//! Interactive panels, overlays, selectors, login flows, and tree/rewind navigation for [`super::App`].

use super::*;

impl App {
    pub(super) fn settings_event(&mut self, event: Option<SettingsEvent>) {
        match event {
            Some(SettingsEvent::Cancelled) => self.set_panel(None),
            Some(SettingsEvent::Preview(setting)) => {
                if let Some(theme) = self.theme_controller.as_mut().and_then(|c| c.preview(&setting)) {
                    self.adopt_theme(&theme);
                }
            }
            Some(SettingsEvent::Change(key, value)) => self.setting_changed(&key, value),
            Some(SettingsEvent::SessionModel(selector)) => {
                if let Some(model) = self
                    .models
                    .iter()
                    .find(|m| format!("{}/{}", m.provider, m.id) == selector)
                    .cloned()
                {
                    self.send(LaneCommand::SelectModel {
                        provider: model.provider.to_string(),
                        id: model.id.to_string(),
                    });
                    self.model = model;
                    self.show_status(&format!("Session model: {selector} (saved as default)"));
                }
            }
            Some(SettingsEvent::ModelSetting { key, label, value }) => {
                let stored = if value.is_empty() {
                    Value::Null
                } else if key == "rlm.raceModels" {
                    Value::from(value.split(", ").collect::<Vec<_>>())
                } else {
                    Value::String(value.clone())
                };
                self.send(LaneCommand::SaveSetting {
                    key: key.to_string(),
                    value: stored,
                });
                let next = match key.as_str() {
                    "rlm.frameModel" | "rlm.frameThinking" => Some("the next frame"),
                    "review.model" => Some("the next /review"),
                    "observer.model" => Some("the next observer check"),
                    "rlm.childModel" => Some("the next sub-agent"),
                    "rlm.raceModels" => Some("the next race"),
                    "judge.escalateModel" | "judge.fallbackModel" => Some("the next judge.decide"),
                    _ => None,
                };
                let when = match next {
                    Some(next) => format!("applies to {next}"),
                    None if key.starts_with("claudeCode.") => "applies when ultron claude next starts".to_string(),
                    None => "applies to the running session".to_string(),
                };
                let shown = match value.is_empty() {
                    true if key == "rlm.raceModels" => "none".to_string(),
                    true => "default".to_string(),
                    false => value,
                };
                self.show_status(&format!("{label}: {shown} ({when})"));
            }
            None => {}
        }
    }

    pub(super) fn handle_panel_input(&mut self, data: &str) {
        match self.panel.as_mut() {
            Some(Panel::Fork(selector)) => match selector.handle_input(&self.keys, data) {
                Some(SelectorAction::Selected(entry_id)) => {
                    self.set_panel(None);
                    if self.operation.is_some() {
                        self.send(LaneCommand::Abort);
                    }
                    self.send(LaneCommand::Fork(entry_id));
                }
                Some(SelectorAction::Cancelled) => self.set_panel(None),
                None => {}
            },
            Some(Panel::RlmFocus(_)) => {
                let snapshot = self.rlm_snapshot();
                let height = self.rlm_focus_height();
                let Some(Panel::RlmFocus(focus)) = self.panel.as_mut() else {
                    return;
                };
                match focus.handle_input(&self.keys, data, &snapshot, height) {
                    rlm_views::focus::FocusEvent::Exit => self.set_panel(None),
                    rlm_views::focus::FocusEvent::LoadTrace { key, trace_id } => self.send(LaneCommand::RlmInspect {
                        tag: format!("trace:{key}"),
                        request: "rlm.frames".into(),
                        payload: serde_json::json!({ "id": trace_id }),
                    }),
                    _ => {}
                }
            }
            Some(Panel::Sessions(selector)) => match selector.handle_input(data) {
                Some(SessionAction::Selected(path)) => {
                    self.set_panel(None);
                    if Some(&path) == self.session_file.as_ref() {
                        self.show_status("Already in this session");
                    } else {
                        self.send(LaneCommand::Resume(path));
                    }
                }
                Some(SessionAction::Cancelled) => self.set_panel(None),
                Some(SessionAction::Delete(path)) => self.send(LaneCommand::DeleteSession(path)),
                Some(SessionAction::LoadAll) => self.send(LaneCommand::ListAllSessions),
                Some(SessionAction::Rename(path, name)) => {
                    if Some(&path) == self.session_file.as_ref() {
                        self.session_name = Some(name.clone());
                        self.update_title();
                    }
                    self.send(LaneCommand::RenameSession { path, name });
                }
                None => {}
            },
            Some(Panel::PiSessions(selector)) => match selector.handle_input(data) {
                Some(SessionAction::Selected(path)) => {
                    self.set_panel(None);
                    self.show_status(&format!("Importing {path}…"));
                    self.send(LaneCommand::ImportPi(path));
                }
                Some(SessionAction::Cancelled) => self.set_panel(None),
                Some(SessionAction::LoadAll) => self.send(LaneCommand::ListAllPiSessions),
                Some(SessionAction::Delete(_) | SessionAction::Rename(..)) => {
                    selector.set_status(true, "Pi sessions are read-only here", Duration::from_secs(3));
                }
                None => {}
            },
            Some(Panel::Login(selector, options, auth_type)) => match selector.handle_input(data) {
                Some(Ok((id, kind))) => {
                    let option = options.iter().find(|o| o.id == id && o.auth_type == kind).cloned();
                    self.set_panel(None);
                    if let Some(option) = option {
                        self.start_login(option);
                    }
                }
                Some(Err(())) => {
                    let (options, typed) = (options.clone(), auth_type.is_some());
                    self.set_panel(None);
                    if typed {
                        self.ask_login_method(options, false);
                    }
                }
                None => {}
            },
            Some(Panel::ScopedModels { selector, changed, .. }) => match selector.handle_input(data) {
                crate::scoped_models::ScopedAction::Changed(enabled) => {
                    *changed = true;
                    self.update_session_models(enabled);
                }
                crate::scoped_models::ScopedAction::Persist(enabled) => self.persist_scope(enabled),
                crate::scoped_models::ScopedAction::Cancel => self.set_panel(None),
                crate::scoped_models::ScopedAction::None => {}
            },
            Some(Panel::OAuthLogin(dialog, flow)) => match dialog.handle_input(data) {
                crate::login_dialog::DialogAction::Answer(answer) => match flow {
                    LoginFlow::ApiKey(login) => {
                        let steps = login.0.answer(answer);
                        self.api_login_steps(steps);
                    }
                    LoginFlow::Client(id) => {
                        let id = *id;
                        self.send(LaneCommand::LoginReply {
                            id,
                            answer: Some(answer),
                        });
                    }
                },
                crate::login_dialog::DialogAction::Cancel => self.set_panel(None),
                crate::login_dialog::DialogAction::None => {}
            },
            Some(Panel::LoginPrompt { provider, input, .. }) => {
                if self.keys.matches(data, "tui.select.cancel") {
                    self.set_panel(None);
                } else if self.keys.matches(data, "tui.select.confirm") {
                    let key = input.value().to_string();
                    let id = provider.id.clone();
                    self.set_panel(None);
                    self.send(LaneCommand::SaveApiKey { provider: id, key });
                } else {
                    input.handle_input(data);
                }
            }
            Some(Panel::Trust(selector)) => match selector.handle_input(data) {
                Some(crate::trust_selector::TrustAction::Selected { trusted, updates }) => {
                    self.set_panel(None);
                    self.send(LaneCommand::SetProjectTrust { trusted, updates });
                }
                Some(crate::trust_selector::TrustAction::Cancelled) => self.set_panel(None),
                None => {}
            },
            Some(Panel::Logout(selector)) => match selector.handle_input(data) {
                Some(Ok((id, kind))) => {
                    self.set_panel(None);
                    let name = crate::selectors::provider_name(&id);
                    let done = if kind == "oauth" {
                        format!("Logged out of {name}")
                    } else {
                        format!(
                            "Removed stored API key for {name}. Environment variables and models.json config are unchanged"
                        )
                    };
                    self.send(LaneCommand::Logout { provider: id, done });
                }
                Some(Err(())) => self.set_panel(None),
                None => {}
            },
            Some(Panel::Settings(selector)) => {
                let event = selector.handle_input(data);
                self.settings_event(event);
            }
            Some(Panel::Tree(tree)) => match tree.handle_input(data) {
                Some(TreeAction::Selected(entry)) => {
                    self.set_panel(None);
                    self.tree_selected(entry);
                }
                Some(TreeAction::Cancelled) => self.set_panel(None),
                Some(TreeAction::Copy(text)) => match text {
                    None => self.show_status("Error: Selected entry has no text to copy"),
                    Some(text) => self.copy_to_clipboard(&text, "Copied selected message to clipboard"),
                },
                Some(TreeAction::Label(entry, label)) => self.send(LaneCommand::SetLabel { entry_id: entry, label }),
                Some(TreeAction::RestoreFiles(entry)) => {
                    self.set_panel(None);
                    self.ask_restore(entry, false);
                }
                None => {}
            },
            Some(Panel::ExtensionInput { id, dialog }) => {
                use crate::extension_dialogs::DialogOutcome;
                let answer = match dialog.handle_input(data) {
                    DialogOutcome::Submit(value) => serde_json::json!({ "value": value }),
                    DialogOutcome::Cancel => serde_json::json!({ "cancelled": true }),
                    _ => return,
                };
                let id = id.clone();
                self.panel.take();
                self.answer_extension_dialog(&id, answer);
            }
            Some(Panel::TextEditor { dialog, .. }) => {
                use crate::extension_dialogs::DialogOutcome;
                let outcome = dialog.handle_input(data);
                if let DialogOutcome::External(text) = outcome {
                    if self.external_editor.is_some() {
                        self.edit_dialog = true;
                        self.edit_request = Some(text);
                    } else {
                        self.show_status("No editor configured. Set $VISUAL or $EDITOR environment variable.");
                    }
                    return;
                }
                let submitted = match outcome {
                    DialogOutcome::Submit(text) => Some(text),
                    DialogOutcome::Cancel => None,
                    _ => return,
                };
                let Some(Panel::TextEditor { entry, .. }) = self.panel.take() else {
                    return;
                };
                match submitted {
                    Some(text) => self.navigate(&entry, true, Some(&text)),
                    None => self.ask_summary(entry),
                }
            }
            Some(Panel::BugReport(flow)) => {
                let action = flow.handle_input(data);
                self.bug_action(action);
            }
            Some(Panel::ShareLoader(loader)) => {
                if loader.handle_input(data) {
                    self.set_panel(None);
                    self.send(LaneCommand::CancelShare);
                    self.show_status("Share cancelled");
                }
            }
            Some(Panel::HistorySearch(search)) => match search.handle_input(data) {
                HistorySearchEvent::Accept(text) => {
                    self.set_panel(None);
                    self.editor.set_text(&text);
                    self.update_editor_border();
                }
                HistorySearchEvent::Cancel => self.set_panel(None),
                HistorySearchEvent::None => {}
            },
            None => {}
        }
    }

    /// `handleLogin`: a provider named in the command signs in directly; otherwise the method, then the provider.
    pub(super) fn setting_changed(&mut self, key: &str, value: Value) {
        const UNUSED_BY_ULTRON: [&str; 17] = [
            "terminal.showImages",
            "terminal.imageWidthCells",
            "images.autoResize",
            "images.blockImages",
            "enableSkillCommands",
            "markdown.mermaid",
            "showCacheMissNotices",
            "collapseChangelog",
            "enableInstallTelemetry",
            "quietStartup",
            "outputPad",
            "terminal.showTerminalProgress",
            "fullscreenExitOutput",
            "fullscreenScrollbar",
            "fullscreenCopyOnSelect",
            "warnings",
            "cacheWarming",
        ];
        self.send(LaneCommand::SaveSetting {
            key: key.to_string(),
            value: value.clone(),
        });
        if key == "modelThinkingLevel"
            && value["provider"] == self.model.provider.as_str()
            && value["modelId"] == self.model.id.as_str()
        {
            let level = value["level"].as_str().unwrap_or("medium").to_string();
            self.set_thinking(&level);
        }
        match key {
            "treeFilterMode" => self.tree_filter = FilterMode::parse(value.as_str().unwrap_or("default")),
            "enabledModels" => {
                self.enabled_models = value
                    .as_array()
                    .map_or_default(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
            }
            _ => self.apply_local_setting(key, &value),
        }
        if key == "tuiMode" {
            self.show_status(&format!(
                "Saved tuiMode: {}. It takes effect when Ultron restarts.",
                value.as_str().unwrap_or("")
            ));
        } else if matches!(key, "defaultThinkingLevel" | "defaultProjectTrust") {
            self.show_status(&format!(
                "Saved {key}. It takes effect when the Session worker restarts."
            ));
        } else if UNUSED_BY_ULTRON.contains(&key) {
            self.show_status(&format!(
                "Saved {key} to settings. Ultron's native TUI and Session worker do not use it."
            ));
        } else if key == "httpIdleTimeoutMs"
            && let Some(ms) = value.as_u64()
        {
            self.show_status(&format!("HTTP idle timeout: {}", format_http_idle(ms)));
        }
    }

    /// `#refreshRlm`: poll the read-only inspections the RLM views draw from (the subagents, recent frames, the
    /// kernel pool, the context state, retained instances); answers arrive as lane events.
    pub(super) fn show_tree(&mut self, tree: Value, leaf: Option<String>, initial: Option<&str>) {
        if tree.as_array().is_none_or(Vec::is_empty) {
            self.show_status("No entries in session");
            return;
        }
        let selector = TreeSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            &tree,
            (leaf.as_deref(), initial),
            self.rows,
            self.tree_filter,
            self.home.as_deref().unwrap_or(""),
        );
        self.tree = Some((tree, leaf));
        self.set_panel(Some(Panel::Tree(Box::new(selector))));
        self.editor.focused = false;
    }

    /// `#navigateTree`: the summary choice, then the move.
    pub(super) fn tree_selected(&mut self, entry: String) {
        let leaf = self.tree.as_ref().and_then(|(_, leaf)| leaf.clone());
        if Some(&entry) == leaf.as_ref() {
            self.show_status("Already at this point");
            return;
        }
        if self.skip_summary_prompt {
            self.navigate(&entry, false, None);
        } else {
            self.ask_summary(entry);
        }
    }

    pub(super) fn ask_summary(&mut self, entry: String) {
        let items = ["No summary", "Summarize", "Summarize with custom prompt"]
            .iter()
            .map(|v| SelectItem {
                value: (*v).to_string(),
                label: (*v).to_string(),
                description: None,
            })
            .collect();
        self.open_selection("Summarize branch?", items, None, Purpose::Summarize(entry));
    }

    /// The user turn the rewind selector continues from for the selected row: the row's own user turn, else the
    /// next one (`None` after the last).
    pub(super) fn rewind_target(&self, entry_id: &str) -> Option<String> {
        let at = self.rewind_ids.iter().position(|(id, _)| id == entry_id)?;
        let (_, user) = &self.rewind_ids[at];
        let from = if *user { at } else { at + 1 };
        self.rewind_ids[from..]
            .iter()
            .find(|(_, user)| *user)
            .map(|(id, _)| id.clone())
    }

    /// Confirm a file-restoring rewind to `entry`'s checkpoint (`rewind`: offer to rewind the conversation too).
    pub(super) fn ask_restore(&mut self, entry: String, rewind: bool) {
        let item = |value: &str, description: &str| SelectItem {
            value: value.to_string(),
            label: value.to_string(),
            description: Some(description.to_string()),
        };
        let saved = "The current files are saved as a checkpoint first, so this can be undone";
        let mut items = vec![item(RESTORE_FILES, saved)];
        if rewind {
            items.push(item(
                RESTORE_FILES_AND_REWIND,
                "Then continue the conversation from there",
            ));
        }
        items.push(item(RESTORE_CANCEL, "Keep the files as they are"));
        let title = if rewind {
            "Restore files to before this turn?"
        } else {
            "Restore files to this point?"
        };
        self.open_selection(title, items, None, Purpose::RestoreFiles(entry));
    }

    pub(super) fn navigate(&mut self, entry_id: &str, summarize: bool, instructions: Option<&str>) {
        if self.operation.is_some() {
            self.send(LaneCommand::Abort);
        }
        self.summarizing_branch = summarize.then(|| entry_id.to_string());
        self.show_status(if summarize {
            "Summarizing branch… (Esc to cancel)"
        } else {
            "Navigating…"
        });
        self.chat.set_navigating(true);
        self.send(LaneCommand::Navigate {
            target: Some(entry_id.to_string()),
            summarize,
            instructions: instructions.map(str::to_string),
        });
    }

    /// Pi's prompt-history search (`app.history.search`, Alt+R).
    pub(super) fn show_history_search(&mut self) {
        let history = self.editor.history().to_vec();
        if history.is_empty() {
            self.show_status("No prompt history yet");
            return;
        }
        let mut search = PromptHistorySearch::new(history, Rc::clone(&self.theme), self.keys.clone(), "");
        search.set_focused(true);
        self.set_panel(Some(Panel::HistorySearch(Box::new(search))));
        self.editor.focused = false;
    }

    /// The overlays' global keys: F5 retry, Alt+L display reset, Alt+A agent hub.
    pub(super) fn handle_overlay_keys(&mut self, data: &str) -> bool {
        if self.keys.matches(data, "app.retry") {
            self.retry();
            return true;
        }
        if self.keys.matches(data, "app.display.reset") {
            self.reset_display = true;
            self.committed.clear();
            return true;
        }
        if self.keys.matches(data, "app.agents.hub") {
            self.send(LaneCommand::Agents);
            return true;
        }
        if self.keys.matches(data, "tui.altScreen.search") {
            let width = self.cols.max(1);
            let rows = self.document(width);
            let mut search = crate::transcript_search::TranscriptSearch::new(rows, self.keys.clone());
            let (t1, t2) = (Rc::clone(&self.theme), Rc::clone(&self.theme));
            search.set_styles(
                Box::new(move |s| format!("\x1b[4m{}\x1b[24m", t1.fg(Fg::Accent, s))),
                Box::new(move |s| format!("\x1b[7m{}\x1b[27m", t2.fg(Fg::Accent, s))),
            );
            self.overlay = Some(Overlay::Search(Box::new(search)));
            return true;
        }
        false
    }

    /// F5: send the failed turn's prompt again (the session keeps the failed attempt).
    pub(super) fn retry(&mut self) {
        if self.operation.is_some() || !crate::retry::failed_turn_tail(&self.transcript) {
            self.show_status("Nothing to retry");
            return;
        }
        let Some(prompt) = self
            .transcript
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(|m| message_text(m).trim().to_string())
            .filter(|t| !t.is_empty())
        else {
            self.show_status("Nothing to retry");
            return;
        };
        self.status = "Retrying…".into();
        self.send(LaneCommand::Prompt {
            text: prompt,
            images: Vec::new(),
        });
    }

    /// Whether the display reset was asked for; clears the request.
    pub fn take_reset_display(&mut self) -> bool {
        std::mem::take(&mut self.reset_display)
    }

    pub(super) fn transcript_entries(&self) -> Vec<TranscriptEntry> {
        self.transcript
            .iter()
            .enumerate()
            .map(|(i, m)| TranscriptEntry::new(&format!("m{i}"), "", m.clone()))
            .collect()
    }

    pub(super) fn picker(&self, mode: PickerMode, entries: Vec<TranscriptEntry>) -> TranscriptPicker {
        TranscriptPicker::new(
            entries,
            PickerOptions {
                mode,
                theme: Rc::clone(&self.theme),
                keys: self.keys.clone(),
                chat: self.chat.style(),
                title: None,
                action_label: None,
            },
        )
    }

    /// `/copy` with the overlays: pick a turn (or a block of it) over the transcript.
    pub(super) fn open_copy_picker(&mut self) {
        if self.last_assistant_text().is_none_or(|t| t.is_empty()) {
            self.show_status("Error: No agent messages to copy yet.");
            return;
        }
        let picker = self.picker(PickerMode::Copy, self.transcript_entries());
        self.overlay = Some(Overlay::Picker(Box::new(picker)));
    }

    /// Esc-Esc: pick the user turn to continue from; the session forks before it (user messages carry their
    /// session entry ids, matched in order by text).
    pub(super) fn open_rewind(&mut self, forkable: Vec<crate::lane::ForkMessage>) {
        let mut ids = forkable.into_iter();
        let entries: Vec<TranscriptEntry> = self
            .transcript
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let id = if m["role"] == "user" {
                    let text = message_text(m);
                    ids.by_ref()
                        .find(|f| f.text == text)
                        .map_or(format!("m{i}"), |f| f.entry_id)
                } else {
                    format!("m{i}")
                };
                TranscriptEntry::new(&id, "", m.clone())
            })
            .collect();
        self.rewind_ids = entries
            .iter()
            .map(|e| (e.id.clone(), e.role() == "user" && !e.id.starts_with('m')))
            .collect();
        if !self.rewind_ids.iter().any(|(_, user)| *user) {
            self.show_status("No messages to rewind to");
            return;
        }
        let picker = self.picker(PickerMode::Rewind, entries);
        self.overlay = Some(Overlay::Picker(Box::new(picker)));
    }

    pub(super) fn open_model_hub(&mut self) {
        let current = Some((
            format!("{}/{}", self.model.provider, self.model.id),
            self.thinking_level.clone(),
        ));
        let hub = ModelHub::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            self.models.clone(),
            self.recent_models.clone(),
            current,
        );
        self.overlay = Some(Overlay::ModelHub(Box::new(hub)));
    }

    pub(super) fn handle_overlay_input(&mut self, data: &str) {
        self.overlay_input(data);
        if self.overlay.is_none() {
            self.reset_display = true;
            self.committed.clear();
        }
    }

    pub(super) fn overlay_input(&mut self, data: &str) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        match overlay {
            Overlay::Search(search) => {
                if matches!(
                    search.handle_input(data),
                    crate::transcript_search::SearchOutcome::Close
                ) {
                    self.overlay = None;
                }
            }
            Overlay::Picker(picker) => match picker.handle_input(data) {
                PickerEvent::Copy { content, label, .. } => {
                    self.overlay = None;
                    self.copy_to_clipboard(&content, &format!("Copied {label} to clipboard"));
                }
                PickerEvent::Open { href, .. } => {
                    self.overlay = None;
                    match self.viewport.open_url.clone() {
                        Some(open) => {
                            open(&href);
                            self.show_status(&format!("Opened {href}"));
                        }
                        None => self.show_status("Error: no browser opener"),
                    }
                }
                PickerEvent::Rewind { entry_id } => {
                    self.overlay = None;
                    let Some(entry_id) = self.rewind_target(&entry_id) else {
                        self.show_status("Already at this point");
                        return;
                    };
                    if self.operation.is_some() {
                        self.send(LaneCommand::Abort);
                    }
                    self.send(LaneCommand::Fork(entry_id));
                }
                PickerEvent::RestoreFiles { entry_id } => {
                    self.overlay = None;
                    let target = self.rewind_target(&entry_id);
                    match target {
                        Some(entry) => self.ask_restore(entry, true),
                        None => self.show_status("Files are already at this point"),
                    }
                }
                PickerEvent::Cancel => self.overlay = None,
                PickerEvent::Ignored | PickerEvent::Render => {}
            },
            Overlay::ModelHub(hub) => match hub.handle_input(data) {
                ModelHubEvent::Use { selector, thinking } => {
                    self.overlay = None;
                    let Some(model) = self
                        .models
                        .iter()
                        .find(|m| format!("{}/{}", m.provider, m.id) == selector)
                        .cloned()
                    else {
                        self.show_status(&format!("Error: Unknown model: {selector}"));
                        return;
                    };
                    self.recent_models.retain(|s| *s != selector);
                    self.recent_models.insert(0, selector);
                    self.select_model(model);
                    if let Some(level) = thinking {
                        self.set_thinking(&level);
                    }
                }
                ModelHubEvent::Cancel => self.overlay = None,
                ModelHubEvent::Ignored | ModelHubEvent::Render => {}
            },
            Overlay::AgentHub(hub) => match hub.handle_input(data) {
                HubEvent::Close => self.overlay = None,
                HubEvent::Kill(id) => {
                    self.send(LaneCommand::KillAgent(id));
                    self.send(LaneCommand::Agents);
                }
                HubEvent::OpenTranscript(id) => {
                    hub.set_transcript(&id, Vec::new());
                    self.send(LaneCommand::AgentTranscript(id));
                }
                HubEvent::Revive(id) => {
                    self.send(LaneCommand::ReviveAgent(id));
                    self.send(LaneCommand::Agents);
                }
                HubEvent::Steer { id, text } => {
                    let parked = hub
                        .rows()
                        .iter()
                        .any(|r| r.id == id && r.status == crate::agent_hub::AgentStatus::Parked);
                    if parked {
                        self.send(LaneCommand::ReviveAgent(id));
                        self.send(LaneCommand::Agents);
                    } else {
                        self.send(LaneCommand::SteerAgent {
                            id: id.to_string(),
                            text,
                        });
                        self.send(LaneCommand::AgentTranscript(id));
                    }
                }
                HubEvent::Ignored | HubEvent::Render => {}
            },
        }
    }

    /// The open overlay's frame, when one is open.
    pub(super) fn overlay_frame(&mut self, width: usize, height: usize) -> Option<Vec<String>> {
        let now = jsrt::date::now_ms();
        Some(match self.overlay.as_mut()? {
            Overlay::Search(search) => search.render(width, height),
            Overlay::Picker(picker) => picker.render(width, height),
            Overlay::ModelHub(hub) => hub.render(width, height),
            Overlay::AgentHub(hub) => hub.render(width, height, now),
        })
    }

    pub(super) fn show_resume(&mut self, sessions: Vec<SessionListing>) {
        if let Some(Panel::Sessions(selector)) = self.panel.as_mut() {
            selector.set_sessions(sessions);
            return;
        }
        let now = jsrt::date::now_ms();
        let selector = SessionSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            sessions,
            self.session_file.clone(),
            self.home.as_deref().unwrap_or(""),
            now,
        );
        self.set_panel(Some(Panel::Sessions(Box::new(selector))));
        self.editor.focused = false;
    }

    /// `/import` without a path: Pi's session selector over Pi's sessions of this project (`showPiSessionPicker`).
    /// Nothing in it is the attached session, and Pi's files are read-only here.
    pub(super) fn show_pi_sessions(&mut self, sessions: Vec<SessionListing>) {
        let now = jsrt::date::now_ms();
        let mut selector = SessionSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            sessions,
            None,
            self.home.as_deref().unwrap_or(""),
            now,
        );
        selector.set_show_rename_hint(false);
        self.set_panel(Some(Panel::PiSessions(Box::new(selector))));
        self.editor.focused = false;
    }

    pub(super) fn open_selection(
        &mut self,
        title: &str,
        items: Vec<SelectItem>,
        selected: Option<&str>,
        purpose: Purpose,
    ) {
        let index = selected.and_then(|v| items.iter().position(|i| i.value == v));
        let mut list = SelectList::new(
            items.clone(),
            items.len().clamp(1, 12),
            crate::components::select_theme(&self.theme, Fg::Dim, Fg::Warning),
        );
        if let Some(index) = index {
            list.set_selected_index(index);
        }
        self.editor.focused = false;
        if let Some(Selection {
            purpose: Purpose::ExtensionSelect { id },
            ..
        }) = self.selection.replace(Selection {
            title: title.to_string(),
            list,
            purpose,
        }) {
            self.send(LaneCommand::ExtensionUiRespond {
                id,
                answer: serde_json::json!({ "cancelled": true }),
            });
        }
    }

    pub(super) fn handle_select_input(&mut self, data: &str) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        let event = selection.list.handle_input(data);
        self.select_event(event);
    }

    /// What the selection list answered (a key or a click).
    pub(super) fn select_event(&mut self, event: Option<SelectEvent>) {
        match event {
            Some(SelectEvent::Selected(item)) => {
                let selection = self.selection.take().expect("selection is open");
                match selection.purpose {
                    Purpose::Model => {
                        if let Some(model) = self
                            .models
                            .iter()
                            .find(|m| format!("{}/{}", m.provider, m.id) == item.value)
                            .cloned()
                        {
                            self.select_model(model);
                        }
                    }
                    Purpose::LoginMethod(options, all) => {
                        let kind = item.value;
                        if all {
                            self.show_login_providers(options, Some(&kind), "");
                        } else if let Some(option) = options.iter().find(|o| o.auth_type == kind).cloned() {
                            self.start_login(option);
                        }
                    }
                    Purpose::Summarize(entry) => match item.value.as_str() {
                        "Summarize with custom prompt" => {
                            let dialog = crate::extension_dialogs::ExtensionEditorDialog::new(
                                Rc::clone(&self.theme),
                                self.keys.clone(),
                                self.keys.clone(),
                                "Custom summarization instructions",
                                None,
                            );
                            self.editor.focused = false;
                            self.set_panel(Some(Panel::TextEditor {
                                entry,
                                dialog: Box::new(dialog),
                            }));
                        }
                        choice => self.navigate(&entry, choice == "Summarize", None),
                    },
                    Purpose::RestoreFiles(entry) => match item.value.as_str() {
                        RESTORE_FILES => self.send(LaneCommand::RestoreFiles(entry)),
                        RESTORE_FILES_AND_REWIND => {
                            self.rewind_after_restore = Some(entry.clone());
                            self.send(LaneCommand::RestoreFiles(entry));
                        }
                        _ => {}
                    },
                    Purpose::SwitchImported {
                        session_file,
                        imported,
                        source,
                    } => match (item.value.as_str(), session_file) {
                        ("Switch to the imported session", Some(file)) => {
                            self.send(LaneCommand::Resume(file));
                            self.show_status(&format!("Session imported from: {source}"));
                        }
                        _ => self.show_status(&format!("{imported}. Use /resume to open it.")),
                    },
                    Purpose::Thinking => {
                        let level = item.value;
                        self.set_thinking(&level);
                        self.status = format!("Thinking level: {level}.");
                    }
                    Purpose::Mode(always_skills) => {
                        if let Some(mode) = crate::modes::find(&item.value) {
                            self.set_engineering(mode.engineering, &always_skills);
                        }
                    }
                    Purpose::AutoreviewAccount(accounts) => {
                        let chosen = item.value.parse::<usize>().ok().and_then(|index| accounts.get(index));
                        self.send(LaneCommand::Autoreview(match chosen {
                            Some((login, host)) => AutoreviewRequest::On {
                                accounts: Some(vec![format!("{host}/{login}")]),
                                reviewing_as: format!("{host}/{login}"),
                            },
                            None => AutoreviewRequest::On {
                                accounts: None,
                                reviewing_as: "every logged-in account".into(),
                            },
                        }));
                    }
                    Purpose::ExtensionSelect { id } => {
                        self.answer_extension_dialog(&id, serde_json::json!({ "value": item.value }));
                    }
                }
            }
            Some(SelectEvent::Cancelled) => match self.selection.take() {
                Some(Selection {
                    purpose: Purpose::Summarize(entry),
                    ..
                }) => {
                    if let Some((tree, leaf)) = self.tree.clone() {
                        self.show_tree(tree, leaf, Some(&entry));
                    }
                }
                Some(Selection {
                    purpose: Purpose::SwitchImported { imported, .. },
                    ..
                }) => self.show_status(&format!("{imported}. Use /resume to open it.")),
                Some(Selection {
                    purpose: Purpose::ExtensionSelect { id },
                    ..
                }) => self.answer_extension_dialog(&id, serde_json::json!({ "cancelled": true })),
                Some(Selection {
                    purpose: Purpose::AutoreviewAccount(_),
                    ..
                }) => self.show_status("Autoreview left as it was"),
                _ => {}
            },
            _ => {}
        }
        self.next_extension_dialog();
    }
}
