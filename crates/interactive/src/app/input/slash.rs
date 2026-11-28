//! Slash-command dispatch for [`crate::app::App`].

use super::super::*;

impl App {
    pub(in crate::app) fn execute_slash_command(&mut self, name: &str, args: &str) {
        self.editor.set_text("");
        let Some(command) = Slash::parse(name, self.qol.overlays) else {
            self.status = format!("Unknown slash command: /{name}");
            return;
        };
        let _ = self.cmd_session(command, args)
            || self.cmd_navigation(command, args)
            || self.cmd_model_and_mode(command, args)
            || self.cmd_automation(command, args)
            || self.cmd_tools_and_ui(command, args)
            || self.cmd_auth_and_ext(command, args)
            || self.cmd_usage_and_inspect(command, name, args);
    }

    fn cmd_session(&mut self, command: Slash, args: &str) -> bool {
        match command {
            Slash::Name => self.cmd_name(args),
            Slash::Session => self.cmd_session_info(),
            Slash::Quit => self.exit = true,
            Slash::New | Slash::Clear => self.abort_and_send(LaneCommand::NewSession),
            Slash::Continue => {
                let id = args.trim();
                let target = (!id.is_empty()).then(|| id.to_string());
                self.abort_and_send(LaneCommand::ContinueSession(target));
            }
            Slash::Clone => self.abort_and_send(LaneCommand::CloneSession),
            _ => return false,
        }
        true
    }

    fn cmd_navigation(&mut self, command: Slash, args: &str) -> bool {
        match command {
            Slash::Resume => self.send(LaneCommand::ListSessions),
            Slash::Import => self.cmd_import(args),
            Slash::Compact => {
                self.show_status("Compacting…");
                self.send(LaneCommand::Compact(
                    (!args.is_empty()).then_some(args).map(str::to_string),
                ));
            }
            Slash::Fork => self.fork_messages(false),
            Slash::Tree => self.send(LaneCommand::Tree),
            Slash::Export => self.send(LaneCommand::Export {
                path: path_argument(args),
                theme: self.active_theme_name(),
            }),
            _ => return false,
        }
        true
    }

    fn cmd_model_and_mode(&mut self, command: Slash, args: &str) -> bool {
        match command {
            Slash::ScopedModels => self.open_scoped_models(),
            Slash::Model | Slash::Models if self.qol.overlays && args.is_empty() => self.open_model_hub(),
            Slash::Model | Slash::Models => self.model_command(args),
            Slash::Thinking => self.thinking_command(args),
            Slash::Mode => self.cmd_mode(args),
            Slash::Engineering => self.cmd_engineering(args),
            Slash::Persona => self.cmd_persona(args),
            _ => return false,
        }
        true
    }

    fn cmd_automation(&mut self, command: Slash, args: &str) -> bool {
        match command {
            Slash::KeepGoing => self.cmd_keep_going(args),
            Slash::Schedules => self.send(LaneCommand::Inspect {
                title: "/schedules".into(),
                request: "schedule.list".into(),
                payload: serde_json::json!({"text": true}),
            }),
            Slash::Autoreview => self.cmd_autoreview(args),
            Slash::Review => self.cmd_review(args),
            Slash::Learning => self.cmd_learning(args),
            Slash::Goal => self.send(LaneCommand::Goal(args.to_string())),
            Slash::Observer => self.send(LaneCommand::Observer(args.to_string())),
            _ => return false,
        }
        true
    }

    fn cmd_tools_and_ui(&mut self, command: Slash, args: &str) -> bool {
        match command {
            Slash::Copy if self.qol.overlays => self.open_copy_picker(),
            Slash::Copy => self.copy_last_message(),
            Slash::Hotkeys => self.cmd_hotkeys(),
            Slash::Rlm => self.cmd_rlm(args),
            Slash::Settings => self.send(LaneCommand::ReadSettings),
            Slash::Trust => self.send(LaneCommand::ReadTrust),
            Slash::Debug => self.cmd_debug(),
            Slash::Reload => {
                self.show_status("Reloading plugins…");
                self.send(LaneCommand::ReloadPlugins);
            }
            _ => return false,
        }
        true
    }

    fn cmd_auth_and_ext(&mut self, command: Slash, args: &str) -> bool {
        match command {
            Slash::Mcp => self.send(LaneCommand::Mcp(args.trim().to_string())),
            Slash::Extension => self.show_status(&format!("Error: {EXTENSIONS_UNSUPPORTED}")),
            Slash::Bug => self.open_bug_report(args),
            Slash::Logout => self.send(LaneCommand::Credentials),
            Slash::Login => self.send(LaneCommand::LoginProviders(args.to_string())),
            Slash::Changelog => self.send(LaneCommand::Changelog),
            Slash::Share => self.send(LaneCommand::Share {
                theme: self.active_theme_name(),
            }),
            _ => return false,
        }
        true
    }

    fn cmd_usage_and_inspect(&mut self, command: Slash, name: &str, args: &str) -> bool {
        match command {
            Slash::Usage => self.send(LaneCommand::Usage),
            Slash::ArminSaysHi => self.cmd_armin(name, args),
            Slash::DementedElves => self.cmd_demented_elves(name, args),
            Slash::Agents => self.cmd_agents(name, args),
            Slash::Memory | Slash::Skills | Slash::Experiments | Slash::Goals | Slash::Progress => {
                self.cmd_inspect(name, args);
            }
            _ => return false,
        }
        true
    }

    fn abort_and_send(&mut self, command: LaneCommand) {
        if self.operation.is_some() {
            self.send(LaneCommand::Abort);
        }
        self.send(command);
    }

    fn cmd_name(&mut self, args: &str) {
        let name = args.trim();
        if name.is_empty() {
            match self.session_name.clone() {
                Some(current) => self.notice(&self.theme.fg(Fg::Dim, &format!("Session name: {current}"))),
                None => self.show_status("Usage: /name <name>"),
            }
            return;
        }
        self.send(LaneCommand::SetName(name.to_string()));
        self.pending_name = Some(name.to_string());
    }

    fn cmd_session_info(&mut self) {
        let Some(id) = self.session_id.clone() else {
            return;
        };
        let stats = self.session_stats(id);
        let text = session_info(&self.theme, &stats);
        self.notice(&text);
    }

    fn cmd_hotkeys(&mut self) {
        let notice = MarkdownNotice {
            theme: Rc::clone(&self.theme),
            title: "Keyboard Shortcuts".into(),
            markdown: hotkeys_markdown(&self.keys),
        };
        self.chat.append_notice(Box::new(notice));
    }

    fn cmd_import(&mut self, args: &str) {
        match path_argument(args) {
            Some(path) => {
                let path = if Path::new(&path).is_absolute() {
                    path
                } else {
                    Path::new(&self.cwd).join(path).display().to_string()
                };
                self.show_status(&format!("Importing {path}…"));
                self.send(LaneCommand::ImportPi(path));
            }
            None => self.send(LaneCommand::ListPiSessions),
        }
    }

    fn cmd_mode(&mut self, args: &str) {
        match args.trim() {
            "" => self.request_mode(crate::modes::ModeRequest::Picker),
            name => match crate::modes::find(name) {
                Some(mode) => self.request_mode(crate::modes::ModeRequest::Set(mode.engineering)),
                None => self.show_status(&format!(
                    "Error: Unknown mode \"{name}\". Available modes: {}.",
                    crate::modes::MODES.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
                )),
            },
        }
    }

    fn cmd_engineering(&mut self, args: &str) {
        match crate::modes::engineering_arg(args) {
            Some(None) => self.request_mode(crate::modes::ModeRequest::Status),
            Some(Some(enabled)) => self.request_mode(crate::modes::ModeRequest::Set(enabled)),
            None => self.show_status("Usage: /engineering true|false"),
        }
    }

    fn cmd_persona(&mut self, args: &str) {
        match crate::modes::persona_arg(args) {
            Some(crate::modes::PersonaArg::Report) => self.send(LaneCommand::ReadPersona),
            Some(crate::modes::PersonaArg::Global(on)) => {
                self.send(LaneCommand::SaveSetting {
                    key: config::keys::PERSONA_KEY.into(),
                    value: Value::from(if on {
                        config::keys::PERSONA_ULTRON
                    } else {
                        config::keys::PERSONA_OFF
                    }),
                });
                self.show_status(&crate::modes::persona_changed(on));
            }
            Some(crate::modes::PersonaArg::Session(arg)) => self.send(LaneCommand::Persona(arg.to_string())),
            None => self.show_status(crate::modes::PERSONA_USAGE),
        }
    }

    fn cmd_keep_going(&mut self, args: &str) {
        match args.trim().to_lowercase().as_str() {
            arg @ ("" | "on" | "off") => self.send(LaneCommand::KeepGoing(arg.to_string())),
            _ => self.show_status("Usage: /keepgoing on|off"),
        }
    }

    fn cmd_autoreview(&mut self, args: &str) {
        let words: Vec<&str> = args.split_whitespace().collect();
        let verb = words.first().map(|word| word.to_lowercase());
        let ban = |login: &str, account: Option<&str>| (login.to_string(), account.map(str::to_string));
        let target = match &words[..] {
            [_, login] => Some(ban(login, None)),
            [_, login, "--account", account] => Some(ban(login, Some(account))),
            _ => None,
        };
        match (verb.as_deref(), target) {
            (None, _) => self.send(LaneCommand::Autoreview(AutoreviewRequest::Status)),
            (Some("on"), _) if words.len() == 1 => {
                self.send(LaneCommand::Autoreview(AutoreviewRequest::Accounts));
            }
            (Some("off"), _) if words.len() == 1 => self.send(LaneCommand::Autoreview(AutoreviewRequest::Off)),
            (Some("ban"), Some((login, account))) => {
                self.send(LaneCommand::Autoreview(AutoreviewRequest::Ban { login, account }));
            }
            (Some("unban"), Some((login, account))) => {
                self.send(LaneCommand::Autoreview(AutoreviewRequest::Unban { login, account }));
            }
            _ => self.show_status("Usage: /autoreview on|off, or ban|unban <login> [--account <login>]"),
        }
    }

    fn cmd_rlm(&mut self, args: &str) {
        match args.trim() {
            "focus" => self.open_rlm_focus(),
            "pane" => self.rlm_pane(),
            _ => self.toggle_rlm(),
        }
    }

    fn cmd_debug(&mut self) {
        let (width, height) = (self.cols.max(1), self.rows.max(1));
        let lines = self.render(width, height);
        self.send(LaneCommand::WriteDebugLog(crate::debug_log::DebugCapture {
            width,
            height,
            lines,
            messages: self.transcript.clone(),
            session_id: self.session_id.clone(),
        }));
    }

    fn cmd_review(&mut self, args: &str) {
        self.show_status("Reviewing changes…");
        let message = format!("/review {args}").trim().to_string();
        if self.operation.is_some() {
            let id = self.enqueue(QueueKind::Steer, message, Vec::new());
            self.status = format!("Queued {id}.");
        } else {
            self.send(LaneCommand::Prompt {
                text: message,
                images: Vec::new(),
            });
        }
    }

    fn cmd_learning(&mut self, args: &str) {
        let trimmed = args.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("switch") {
            self.open_learning_picker();
            return;
        }
        self.send_learning_prompt(trimmed);
    }

    pub(in crate::app) fn send_learning_prompt(&mut self, args: &str) {
        self.show_status("Learning mode…");
        let message = format!("/learning {args}").trim().to_string();
        if self.operation.is_some() {
            let id = self.enqueue(QueueKind::Steer, message, Vec::new());
            self.status = format!("Queued {id}.");
        } else {
            self.send(LaneCommand::Prompt {
                text: message,
                images: Vec::new(),
            });
        }
    }

    fn cmd_armin(&mut self, name: &str, args: &str) {
        if !args.is_empty() {
            self.show_status(&format!("Error: /{name} is not available in the Rust build yet"));
            return;
        }
        let armin = Rc::new(std::cell::RefCell::new(crate::easter_eggs::Armin::new(
            Rc::clone(&self.theme),
            Box::new(crate::easter_eggs::seeded_random(self.now())),
        )));
        let id = self.ticks ^ (self.armin.len() as u64) << 48;
        self.chat
            .append_live_notice(id, Box::new(crate::easter_eggs::Shared(Rc::clone(&armin))));
        let now = self.now();
        self.armin.push((id, armin, now, 0));
    }

    fn cmd_demented_elves(&mut self, name: &str, args: &str) {
        if !args.is_empty() {
            self.show_status(&format!("Error: /{name} is not available in the Rust build yet"));
            return;
        }
        let announcement = crate::easter_eggs::EarendilAnnouncement::new(Rc::clone(&self.theme));
        self.chat.append_notice(Box::new(announcement));
    }

    fn cmd_agents(&mut self, name: &str, args: &str) {
        if self.qol.overlays && args.is_empty() {
            self.send(LaneCommand::Agents);
        } else {
            self.cmd_inspect(name, args);
        }
    }

    fn cmd_inspect(&mut self, name: &str, args: &str) {
        match crate::inspection::inspection_request(name, args.trim()) {
            Some(Ok((request, payload))) => {
                let title = format!("/{name} {args}");
                self.send(LaneCommand::Inspect {
                    title: title.trim().to_string(),
                    request,
                    payload,
                });
            }
            Some(Err(usage)) => self.show_status(&format!("Error: {usage}")),
            None => {}
        }
    }
}
