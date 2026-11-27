//! Provider login dialogs, OAuth/API-key flows, and model-scope configuration for [`super::App`].

use super::*;

fn login_method_items(options: &[crate::selectors::AuthProvider], all: bool) -> Vec<SelectItem> {
    let oauth_label = (!all)
        .then(|| {
            options
                .iter()
                .find(|o| o.auth_type == "oauth")
                .and_then(|o| o.login_label.clone())
        })
        .flatten();
    let subscription = oauth_label.unwrap_or_else(|| "Sign in with an account".into());
    let has = |kind: &str| all || options.iter().any(|o| o.auth_type == kind);
    let mut items = Vec::new();
    if has("oauth") {
        items.push(SelectItem {
            value: "oauth".into(),
            label: subscription,
            description: None,
        });
    }
    if has("api_key") {
        items.push(SelectItem {
            value: "api_key".into(),
            label: "Sign in with an API key".into(),
            description: None,
        });
    }
    items
}

impl App {
    pub(super) fn login(&mut self, providers: Vec<crate::selectors::AuthProvider>, query: &str) {
        let reference = query.trim().to_lowercase();
        if reference.is_empty() {
            self.ask_login_method(providers, true);
            return;
        }
        let matches: Vec<crate::selectors::AuthProvider> = providers
            .iter()
            .filter(|p| p.id.to_lowercase() == reference || p.name.to_lowercase() == reference)
            .cloned()
            .collect();
        if matches.len() == 1 {
            self.start_login(matches[0].clone());
            return;
        }
        let ids: std::collections::HashSet<&str> = matches.iter().map(|p| p.id.as_str()).collect();
        if matches.len() > 1 && ids.len() == 1 {
            self.ask_login_method(matches, false);
            return;
        }
        self.show_login_providers(providers, None, query.trim());
    }

    /// `showAuthTypeSelector`: account (subscription) or API key; `all` when it chooses among every provider.
    pub(super) fn ask_login_method(&mut self, options: Vec<crate::selectors::AuthProvider>, all: bool) {
        let items = login_method_items(&options, all);
        if items.is_empty() {
            self.show_status("No login methods available.");
            return;
        }
        if !all && items.len() == 1 {
            self.start_login(options[0].clone());
            return;
        }
        let title = if all {
            "Select authentication method:".to_string()
        } else {
            format!("Select authentication method for {}:", options[0].name)
        };
        self.open_selection(&title, items, None, Purpose::LoginMethod(options, all));
    }

    /// `showProviderSelector`.
    pub(super) fn show_login_providers(
        &mut self,
        all: Vec<crate::selectors::AuthProvider>,
        auth_type: Option<&str>,
        search: &str,
    ) {
        let options: Vec<crate::selectors::AuthProvider> = all
            .iter()
            .filter(|p| auth_type.is_none_or(|t| p.auth_type == t))
            .cloned()
            .collect();
        if options.is_empty() {
            self.show_status(match auth_type {
                Some("oauth") => "No subscription providers available.",
                Some("api_key") => "No API key providers available.",
                _ => "No login providers available.",
            });
            return;
        }
        let selector = crate::selectors::AuthSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            true,
            options,
            (!search.is_empty()).then_some(search),
        );
        self.set_panel(Some(Panel::Login(
            Box::new(selector),
            all,
            auth_type.map(str::to_string),
        )));
        self.editor.focused = false;
    }

    /// The `enabledModels` patterns as the selector shows them (`configuredEnabledIds`): the models they select, then
    /// patterns that select none (shown as unavailable); `None` without patterns.
    pub(super) fn configured_enabled_ids(&self) -> Option<Vec<String>> {
        if self.enabled_models.is_empty() {
            return None;
        }
        let (scoped, diagnostics) = model_scope::resolve_model_scope(&self.enabled_models, &self.models);
        let mut ids: Vec<String> = scoped
            .iter()
            .map(|s| format!("{}/{}", s.model.provider, s.model.id))
            .collect();
        for diagnostic in diagnostics {
            if diagnostic.code == "no-match" && !ids.contains(&diagnostic.pattern) {
                ids.push(diagnostic.pattern);
            }
        }
        Some(ids)
    }

    /// Pi's `/scoped-models` (`showScopedModelsSelector`): the session scope, else `enabledModels`; the catalogs refresh
    /// while it is open.
    pub(super) fn open_scoped_models(&mut self) {
        let session_scope = self.scoped_ids.as_ref().is_some_and(|ids| !ids.is_empty());
        let enabled = if session_scope {
            self.scoped_ids.clone()
        } else {
            self.configured_enabled_ids()
        };
        let selector = crate::scoped_models::ScopedModelsSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            &self.models,
            enabled,
            Some("Refreshing model catalogs…"),
        );
        self.set_panel(Some(Panel::ScopedModels {
            selector: Box::new(selector),
            changed: false,
            session_scope,
        }));
        self.editor.focused = false;
        self.send(LaneCommand::RefreshCatalog);
    }

    /// The catalogs refreshed (or timed out, or failed): the open selector shows the catalog it has now.
    pub(super) fn catalog_refreshed(&mut self, refresh: crate::lane::CatalogRefresh) {
        use crate::lane::CatalogRefresh;
        let configured = self.configured_enabled_ids();
        let models = self.models.clone();
        if let Some(Panel::ScopedModels {
            selector,
            changed,
            session_scope,
        }) = self.panel.as_mut()
        {
            let enabled = (!*changed && !*session_scope).then_some(configured);
            selector.update_models(&models, enabled);
            match refresh {
                CatalogRefresh::TimedOut => {
                    selector.set_refresh_status("Model refresh timed out; showing cached models.", Fg::Warning);
                }
                CatalogRefresh::Failed(error) => {
                    selector.set_refresh_status(&format!("Could not refresh model catalogs: {error}"), Fg::Warning);
                }
                CatalogRefresh::Done(failed) if !failed.is_empty() => selector.set_refresh_status(
                    &format!("Could not refresh {}; showing cached models.", failed.join(", ")),
                    Fg::Warning,
                ),
                CatalogRefresh::Done(_) => selector.set_refresh_status("Model catalogs refreshed.", Fg::Success),
            }
        }
    }

    /// `updateSessionModels`: the session scope Ctrl+P uses; every model when the selection covers them all or none
    /// is available.
    pub(super) fn update_session_models(&mut self, enabled: Option<Vec<String>>) {
        let available: Vec<String> = self.models.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect();
        self.scoped_ids = Some(match enabled {
            Some(ids)
                if ids.iter().any(|id| available.contains(id)) && !available.iter().all(|id| ids.contains(id)) =>
            {
                ids
            }
            _ => Vec::new(),
        });
    }

    /// `onPersist`: the selection saved to `enabledModels` (removed when it is every model).
    pub(super) fn persist_scope(&mut self, enabled: Option<Vec<String>>) {
        let available: Vec<String> = self.models.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect();
        let patterns =
            enabled.filter(|ids| !(ids.len() == available.len() && ids.iter().all(|id| available.contains(id))));
        let value = patterns
            .as_ref()
            .map_or(serde_json::Value::Null, |p| serde_json::json!(p));
        self.send(LaneCommand::SaveSetting {
            key: "enabledModels".into(),
            value,
        });
        self.enabled_models = patterns.unwrap_or_default();
        self.show_status("Model selection saved to settings");
    }

    /// A step of the subscription login running in the client (`runProviderLogin`'s dialog).
    /// A step of login `id` reaches only that login's dialog.
    pub(super) fn login_step(&mut self, id: u64, step: crate::lane::LoginStep) {
        use crate::lane::LoginStep;
        if let LoginStep::Started { name, reach } = step {
            let dialog = crate::login_dialog::LoginDialog::new(self.keys.clone(), &name, &reach);
            self.set_panel(Some(Panel::OAuthLogin(Box::new(dialog), LoginFlow::Client(id))));
            self.editor.focused = false;
            return;
        }
        let Some(Panel::OAuthLogin(dialog, LoginFlow::Client(shown))) = self.panel.as_mut() else {
            return;
        };
        if *shown != id {
            return;
        }
        match step {
            LoginStep::Started { .. } => {}
            LoginStep::Event(event) => dialog.show_event(&self.theme, &event),
            LoginStep::Prompt(prompt) => dialog.show_prompt(&self.theme, &prompt),
            LoginStep::Finished => {
                self.panel.take();
                self.next_extension_dialog();
            }
        }
    }

    /// `startProviderLogin`: the login dialog (a subscription login runs in the client; an API key is prompted for).
    pub(super) fn start_login(&mut self, option: crate::selectors::AuthProvider) {
        if option.auth_type == "oauth" {
            self.last_login += 1;
            self.send(LaneCommand::OAuthLogin {
                id: self.last_login,
                provider: option.id,
                name: option.name,
            });
            return;
        }
        if let Some((login, steps)) = crate::api_key_login::ApiKeyLogin::start(&option.id) {
            let dialog = crate::login_dialog::LoginDialog::new(self.keys.clone(), &option.name, "local");
            let flow = LoginFlow::ApiKey(Box::new((login, option)));
            self.set_panel(Some(Panel::OAuthLogin(Box::new(dialog), flow)));
            self.editor.focused = false;
            self.api_login_steps(steps);
            return;
        }
        match option.api_key_method.clone() {
            Some((method, true)) if option.generic_login => {
                let mut input = tui::Input::new(self.keys.clone()).with_options(None, None, None, Some("•"));
                input.focused = true;
                self.set_panel(Some(Panel::LoginPrompt {
                    provider: option,
                    method,
                    input: Box::new(input),
                }));
                self.editor.focused = false;
            }
            Some((method, false)) => self.show_status(&format!("{method} is configured outside Ultron.")),
            _ => self.show_status(&format!(
                "Error: Failed to save API key for {}: this provider's sign-in is not available in the Rust build yet",
                option.name
            )),
        }
    }

    /// The multi-field API-key login's next steps: notices and the next prompt go to the dialog; the credential is
    /// saved (`runLoginDialog`'s outcome statuses come from the client), a failure says so.
    pub(super) fn api_login_steps(&mut self, steps: Vec<crate::api_key_login::Step>) {
        use crate::api_key_login::Step;
        for step in steps {
            match step {
                Step::Event(event) => {
                    if let Some(Panel::OAuthLogin(dialog, LoginFlow::ApiKey(_))) = self.panel.as_mut() {
                        dialog.show_event(&self.theme, &event);
                    }
                }
                Step::Prompt(prompt) => {
                    if let Some(Panel::OAuthLogin(dialog, LoginFlow::ApiKey(_))) = self.panel.as_mut() {
                        dialog.show_prompt(&self.theme, &prompt);
                    }
                }
                Step::Done(credential) => {
                    if let Some(Panel::OAuthLogin(_, LoginFlow::ApiKey(login))) = &self.panel {
                        let provider = login.0.provider().to_string();
                        self.set_panel(None);
                        self.send(LaneCommand::SaveCredential { provider, credential });
                    }
                }
                Step::Failed(error) => {
                    if let Some(Panel::OAuthLogin(_, LoginFlow::ApiKey(login))) = &self.panel {
                        let name = login.1.name.clone();
                        self.set_panel(None);
                        self.show_status(&format!("Error: Failed to save API key for {name}: {error}"));
                    }
                }
            }
        }
    }
}
