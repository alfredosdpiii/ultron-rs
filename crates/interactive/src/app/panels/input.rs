//! Input routing for active dock panels in [`super::super::App`].

use super::super::*;

impl App {
    pub(in crate::app) fn handle_panel_input(&mut self, data: &str) {
        if self.handle_selector_panel_input(data) {
            return;
        }
        if self.handle_auth_panel_input(data) {
            return;
        }
        self.handle_dialog_panel_input(data);
    }

    fn handle_selector_panel_input(&mut self, data: &str) -> bool {
        match self.panel.as_mut() {
            Some(Panel::Fork(selector)) => {
                let action = selector.handle_input(&self.keys, data);
                self.apply_fork_action(action);
            }
            Some(Panel::RlmFocus(_)) => self.handle_rlm_focus_input(data),
            Some(Panel::Sessions(selector)) => {
                let action = selector.handle_input(data);
                self.apply_sessions_action(action);
            }
            Some(Panel::PiSessions(_)) => self.handle_pi_sessions_input(data),
            Some(Panel::Settings(selector)) => {
                let event = selector.handle_input(data);
                self.settings_event(event);
            }
            Some(Panel::Tree(tree)) => {
                let action = tree.handle_input(data);
                self.apply_tree_action(action);
            }
            Some(Panel::HistorySearch(search)) => {
                let event = search.handle_input(data);
                self.apply_history_search_event(event);
            }
            _ => return false,
        }
        true
    }

    fn handle_auth_panel_input(&mut self, data: &str) -> bool {
        match self.panel.as_mut() {
            Some(Panel::Login(..)) => self.handle_login_input(data),
            Some(Panel::ScopedModels { .. }) => self.handle_scoped_models_input(data),
            Some(Panel::OAuthLogin(..)) => self.handle_oauth_login_input(data),
            Some(Panel::LoginPrompt { .. }) => self.handle_login_prompt_input(data),
            Some(Panel::Trust(selector)) => {
                let action = selector.handle_input(data);
                self.apply_trust_action(action);
            }
            Some(Panel::Logout(selector)) => {
                let action = selector.handle_input(data);
                self.apply_logout_action(action);
            }
            _ => return false,
        }
        true
    }

    fn handle_dialog_panel_input(&mut self, data: &str) {
        match self.panel.as_mut() {
            Some(Panel::ExtensionInput { .. }) => self.handle_extension_input(data),
            Some(Panel::TextEditor { .. }) => self.handle_text_editor_input(data),
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
            _ => {}
        }
    }

    fn apply_fork_action(&mut self, action: Option<SelectorAction>) {
        match action {
            Some(SelectorAction::Selected(entry_id)) => {
                self.set_panel(None);
                if self.operation.is_some() {
                    self.send(LaneCommand::Abort);
                }
                self.send(LaneCommand::Fork(entry_id));
            }
            Some(SelectorAction::Cancelled) => self.set_panel(None),
            None => {}
        }
    }

    fn handle_rlm_focus_input(&mut self, data: &str) {
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

    fn apply_sessions_action(&mut self, action: Option<SessionAction>) {
        match action {
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
        }
    }

    fn handle_pi_sessions_input(&mut self, data: &str) {
        let Some(Panel::PiSessions(selector)) = self.panel.as_mut() else {
            return;
        };
        match selector.handle_input(data) {
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
        }
    }

    fn handle_login_input(&mut self, data: &str) {
        let Some(Panel::Login(selector, options, auth_type)) = self.panel.as_mut() else {
            return;
        };
        match selector.handle_input(data) {
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
        }
    }

    fn handle_scoped_models_input(&mut self, data: &str) {
        let Some(Panel::ScopedModels { selector, changed, .. }) = self.panel.as_mut() else {
            return;
        };
        match selector.handle_input(data) {
            crate::scoped_models::ScopedAction::Changed(enabled) => {
                *changed = true;
                self.update_session_models(enabled);
            }
            crate::scoped_models::ScopedAction::Persist(enabled) => self.persist_scope(enabled),
            crate::scoped_models::ScopedAction::Cancel => self.set_panel(None),
            crate::scoped_models::ScopedAction::None => {}
        }
    }

    fn handle_oauth_login_input(&mut self, data: &str) {
        let Some(Panel::OAuthLogin(dialog, flow)) = self.panel.as_mut() else {
            return;
        };
        match dialog.handle_input(data) {
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
        }
    }

    fn handle_login_prompt_input(&mut self, data: &str) {
        let Some(Panel::LoginPrompt { provider, input, .. }) = self.panel.as_mut() else {
            return;
        };
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

    fn apply_trust_action(&mut self, action: Option<crate::trust_selector::TrustAction>) {
        match action {
            Some(crate::trust_selector::TrustAction::Selected { trusted, updates }) => {
                self.set_panel(None);
                self.send(LaneCommand::SetProjectTrust { trusted, updates });
            }
            Some(crate::trust_selector::TrustAction::Cancelled) => self.set_panel(None),
            None => {}
        }
    }

    fn apply_logout_action(&mut self, action: Option<Result<(String, String), ()>>) {
        match action {
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
        }
    }

    fn apply_tree_action(&mut self, action: Option<TreeAction>) {
        match action {
            Some(TreeAction::Selected(entry)) => {
                self.set_panel(None);
                self.tree_selected(entry);
            }
            Some(TreeAction::Cancelled) => self.set_panel(None),
            Some(TreeAction::Copy(None)) => self.show_status("Error: Selected entry has no text to copy"),
            Some(TreeAction::Copy(Some(text))) => self.copy_to_clipboard(&text, "Copied selected message to clipboard"),
            Some(TreeAction::Label(entry, label)) => self.send(LaneCommand::SetLabel { entry_id: entry, label }),
            Some(TreeAction::RestoreFiles(entry)) => {
                self.set_panel(None);
                self.ask_restore(entry, false);
            }
            None => {}
        }
    }

    fn handle_extension_input(&mut self, data: &str) {
        use crate::extension_dialogs::DialogOutcome;
        let Some(Panel::ExtensionInput { id, dialog }) = self.panel.as_mut() else {
            return;
        };
        let answer = match dialog.handle_input(data) {
            DialogOutcome::Submit(value) => serde_json::json!({ "value": value }),
            DialogOutcome::Cancel => serde_json::json!({ "cancelled": true }),
            _ => return,
        };
        let id = id.clone();
        self.panel.take();
        self.answer_extension_dialog(&id, answer);
    }

    fn handle_text_editor_input(&mut self, data: &str) {
        use crate::extension_dialogs::DialogOutcome;
        let Some(Panel::TextEditor { dialog, .. }) = self.panel.as_mut() else {
            return;
        };
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

    fn apply_history_search_event(&mut self, event: HistorySearchEvent) {
        match event {
            HistorySearchEvent::Accept(text) => {
                self.set_panel(None);
                self.editor.set_text(&text);
                self.update_editor_border();
            }
            HistorySearchEvent::Cancel => self.set_panel(None),
            HistorySearchEvent::None => {}
        }
    }
}
