//! Interactive panels, overlays, selectors, login flows, and tree/rewind navigation for [`super::App`].

mod input;
mod overlay;

use super::*;

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

const MODEL_SETTING_SCOPES: [(&str, &str); 8] = [
    ("rlm.frameModel", "the next frame"),
    ("rlm.frameThinking", "the next frame"),
    ("review.model", "the next /review"),
    ("observer.model", "the next observer check"),
    ("rlm.childModel", "the next sub-agent"),
    ("rlm.raceModels", "the next race"),
    ("judge.escalateModel", "the next judge.decide"),
    ("judge.fallbackModel", "the next judge.decide"),
];

fn model_setting_when(key: &str) -> String {
    if let Some((_, next)) = MODEL_SETTING_SCOPES.iter().find(|(k, _)| *k == key) {
        return format!("applies to {next}");
    }
    if key.starts_with("claudeCode.") {
        "applies when ultron claude next starts".to_string()
    } else {
        "applies to the running session".to_string()
    }
}

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
            Some(SettingsEvent::SessionModel(selector)) => self.apply_session_model(&selector),
            Some(SettingsEvent::ModelSetting { key, label, value }) => self.apply_model_setting(key, label, value),
            None => {}
        }
    }

    fn apply_session_model(&mut self, selector: &str) {
        let Some(model) = self
            .models
            .iter()
            .find(|m| format!("{}/{}", m.provider, m.id) == selector)
            .cloned()
        else {
            return;
        };
        self.send(LaneCommand::SelectModel {
            provider: model.provider.to_string(),
            id: model.id.to_string(),
        });
        self.model = model;
        self.show_status(&format!("Session model: {selector} (saved as default)"));
    }

    fn apply_model_setting(&mut self, key: String, label: String, value: String) {
        let stored = if value.is_empty() {
            Value::Null
        } else if key == "rlm.raceModels" {
            Value::from(value.split(", ").collect::<Vec<_>>())
        } else {
            Value::String(value.clone())
        };
        self.send(LaneCommand::SaveSetting {
            key: key.clone(),
            value: stored,
        });
        let when = model_setting_when(&key);
        let shown = if !value.is_empty() {
            value
        } else if key == "rlm.raceModels" {
            "none".to_string()
        } else {
            "default".to_string()
        };
        self.show_status(&format!("{label}: {shown} ({when})"));
    }

    /// `handleLogin`: a provider named in the command signs in directly; otherwise the method, then the provider.
    pub(super) fn setting_changed(&mut self, key: &str, value: Value) {
        self.send(LaneCommand::SaveSetting {
            key: key.to_string(),
            value: value.clone(),
        });
        self.sync_model_thinking_setting(key, &value);
        match key {
            "treeFilterMode" => self.tree_filter = FilterMode::parse(value.as_str().unwrap_or("default")),
            "enabledModels" => {
                self.enabled_models = value
                    .as_array()
                    .map_or_default(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect());
            }
            _ => self.apply_local_setting(key, &value),
        }
        self.setting_status_notice(key, &value);
    }

    fn sync_model_thinking_setting(&mut self, key: &str, value: &Value) {
        if key == "modelThinkingLevel"
            && value["provider"] == self.model.provider.as_str()
            && value["modelId"] == self.model.id.as_str()
        {
            let level = value["level"].as_str().unwrap_or("medium").to_string();
            self.set_thinking(&level);
        }
    }

    fn setting_status_notice(&mut self, key: &str, value: &Value) {
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
                if let Some(selection) = self.selection.take() {
                    self.handle_selection_purpose(selection.purpose, item.value);
                }
            }
            Some(SelectEvent::Cancelled) => self.handle_selection_cancelled(),
            _ => {}
        }
        self.next_extension_dialog();
    }

    fn handle_selection_purpose(&mut self, purpose: Purpose, value: String) {
        match purpose {
            Purpose::Model => self.select_model_by_selector(&value),
            Purpose::LoginMethod(options, all) => self.handle_login_method_choice(options, all, &value),
            Purpose::Summarize(entry) => self.handle_summarize_choice(entry, &value),
            Purpose::RestoreFiles(entry) => self.handle_restore_choice(entry, &value),
            Purpose::SwitchImported {
                session_file,
                imported,
                source,
            } => self.handle_switch_imported(&value, session_file, &imported, &source),
            Purpose::Thinking => {
                self.set_thinking(&value);
                self.status = format!("Thinking level: {value}.");
            }
            Purpose::Mode(always_skills) => self.handle_mode_choice(&value, &always_skills),
            Purpose::LearningTopic => self.handle_learning_topic_choice(&value),
            Purpose::AutoreviewAccount(accounts) => self.handle_autoreview_choice(&accounts, &value),
            Purpose::ExtensionSelect { id } => {
                self.answer_extension_dialog(&id, serde_json::json!({ "value": value }));
            }
        }
    }

    fn select_model_by_selector(&mut self, selector: &str) {
        if let Some(model) = self
            .models
            .iter()
            .find(|m| format!("{}/{}", m.provider, m.id) == selector)
            .cloned()
        {
            self.select_model(model);
        }
    }

    fn handle_login_method_choice(&mut self, options: Vec<crate::selectors::AuthProvider>, all: bool, kind: &str) {
        if all {
            self.show_login_providers(options, Some(kind), "");
        } else if let Some(option) = options.iter().find(|o| o.auth_type == kind).cloned() {
            self.start_login(option);
        }
    }

    fn handle_summarize_choice(&mut self, entry: String, choice: &str) {
        if choice == "Summarize with custom prompt" {
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
        } else {
            self.navigate(&entry, choice == "Summarize", None);
        }
    }

    fn handle_restore_choice(&mut self, entry: String, choice: &str) {
        match choice {
            RESTORE_FILES => self.send(LaneCommand::RestoreFiles(entry)),
            RESTORE_FILES_AND_REWIND => {
                self.rewind_after_restore = Some(entry.clone());
                self.send(LaneCommand::RestoreFiles(entry));
            }
            _ => {}
        }
    }

    fn handle_switch_imported(&mut self, choice: &str, session_file: Option<String>, imported: &str, source: &str) {
        match (choice, session_file) {
            ("Switch to the imported session", Some(file)) => {
                self.send(LaneCommand::Resume(file));
                self.show_status(&format!("Session imported from: {source}"));
            }
            _ => self.show_status(&format!("{imported}. Use /resume to open it.")),
        }
    }

    fn handle_mode_choice(&mut self, name: &str, always_skills: &[String]) {
        if let Some(mode) = crate::modes::find(name) {
            self.set_engineering(mode.engineering, always_skills);
        }
    }

    fn handle_autoreview_choice(&mut self, accounts: &[(String, String)], value: &str) {
        let chosen = value.parse::<usize>().ok().and_then(|index| accounts.get(index));
        let request = match chosen {
            Some((login, host)) => AutoreviewRequest::On {
                accounts: Some(vec![format!("{host}/{login}")]),
                reviewing_as: format!("{host}/{login}"),
            },
            None => AutoreviewRequest::On {
                accounts: None,
                reviewing_as: "every logged-in account".into(),
            },
        };
        self.send(LaneCommand::Autoreview(request));
    }

    fn handle_selection_cancelled(&mut self) {
        match self.selection.take() {
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
        }
    }
}
