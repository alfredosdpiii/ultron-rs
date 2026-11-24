//! Keyboard, editor, prompt, slash-command, thinking/mode, and model-selection handlers for [`super::App`].

use super::*;

impl App {
    pub fn handle_input(&mut self, data: &str) {
        let expecting = self
            .theme_controller
            .as_ref()
            .is_some_and(crate::theme_setup::ThemeController::expects_background);
        let read = self.osc11.input(data, expecting);
        if let Some(rgb) = read.reply {
            self.background_reported(rgb);
        }
        for data in read.forward {
            self.handle_terminal_input(&data);
        }
    }

    pub(super) fn handle_terminal_input(&mut self, data: &str) {
        if let Some((height, width)) = parse_cell_size_response(data) {
            if height > 0 && width > 0 {
                let mut images = self.chat.image_settings();
                images.cell = tui::Dimensions {
                    width_px: width,
                    height_px: height,
                };
                self.chat.set_image_settings(images);
            }
            return;
        }
        if let Some(scheme) = crate::theme_setup::parse_color_scheme_report(data) {
            if let Some(theme) = self
                .theme_controller
                .as_mut()
                .and_then(|c| c.terminal_scheme_changed(scheme))
            {
                self.adopt_theme(&theme);
            }
            self.tell_terminal_theme();
            self.sync_scheme_notifications(false);
            return;
        }
        self.handle_key(data);
    }

    pub(super) fn handle_key(&mut self, data: &str) {
        if self.fullscreen_input(data) {
            return;
        }
        if tui::is_key_release(data) {
            return;
        }
        if self.recovering {
            if self.keys.matches(data, "app.clear") {
                self.handle_ctrl_c();
            } else if self.editor.text().is_empty() && self.keys.matches(data, "app.exit") {
                self.exit = true;
            }
            return;
        }
        if self.selection.is_some() {
            self.handle_select_input(data);
            return;
        }
        if self.overlay.is_some() {
            self.handle_overlay_input(data);
            return;
        }
        if self.panel.is_some() {
            self.handle_panel_input(data);
            return;
        }
        if self.rlm_pane_focused && self.rlm_pane_shown() {
            if self.keys.matches(data, "app.rlm.pane") {
                self.rlm_pane();
                return;
            }
            let snapshot = self.rlm_snapshot();
            let height = self.rows;
            if self.rlm_pane.handle_input(&self.keys, data, &snapshot, height) == rlm_views::pane::PaneEvent::Close {
                self.close_rlm_pane();
            }
            return;
        }
        if self.qol.overlays && self.handle_overlay_keys(data) {
            return;
        }
        if self.qol.overlays && data == "\x1b\x1b" && self.operation.is_none() && self.editor.text().trim().is_empty() {
            self.fork_messages(true);
            return;
        }
        if self.qol.attachments && data.starts_with(PASTE_START) && self.paste_attachments(data) {
            return;
        }
        let k = |id: &str| self.keys.matches(data, id);
        if k("app.clipboard.pasteImage") {
            if let Some(clipboard) = &self.clipboard {
                self.pending_reads.push_back(false);
                clipboard.request(ClipboardRead::ImageOrText);
            }
            return;
        }
        if k("app.suspend") {
            self.suspend_request = true;
            return;
        }
        if k("app.interrupt") {
            if let Some(vim) = self.vim.as_mut()
                && vim.consumes_escape()
                && !self.editor.is_showing_autocomplete()
            {
                vim.handle_input(&mut self.editor, data);
                return;
            }
            if !self.editor.is_showing_autocomplete() {
                self.handle_escape();
                return;
            }
            self.feed_editor(data);
            return;
        }
        if k("app.exit") && self.editor.text().is_empty() {
            self.exit = true;
            return;
        }
        if !k("app.exit") && !k("tui.editor.historyPrevious") && !k("tui.editor.historyNext") {
            if k("app.clear") {
                self.handle_ctrl_c();
                return;
            }
            if k("app.model.select") {
                if self.qol.overlays {
                    self.open_model_hub();
                } else {
                    self.execute_slash_command("model", "");
                }
                return;
            }
            if k("app.message.followUp") {
                let text = self.editor.expanded_text().trim().to_string();
                if !text.is_empty() {
                    self.editor.set_text("");
                    self.record_history(&text);
                    self.status = "Queueing follow-up…".into();
                    let images = self.take_images(&text);
                    let id = self.enqueue(QueueKind::FollowUp, text, images);
                    self.status = format!("Queued {id}.");
                }
                return;
            }
            if k("app.message.dequeue") {
                let restored = self.restore_queued_to_editor();
                self.show_status(&match restored {
                    0 => "No queued messages to restore".to_string(),
                    1 => "Restored 1 queued message to editor".to_string(),
                    n => format!("Restored {n} queued messages to editor"),
                });
                return;
            }
            if k("app.tools.expand") {
                let expanded = !self.chat.tools_expanded;
                self.chat.set_tools_expanded(expanded);
                if let Some(bash) = self.bash.as_mut() {
                    bash.expanded = expanded;
                }
                self.header.expanded = expanded;
                self.show_status(&format!(
                    "Tool output: {}",
                    if expanded { "expanded" } else { "collapsed" }
                ));
                return;
            }
            if k("app.thinking.toggle") {
                let hide = !self.chat.hide_thinking;
                self.chat.set_hide_thinking(hide);
                self.send(LaneCommand::SaveSetting {
                    key: "hideThinkingBlock".into(),
                    value: Value::Bool(hide),
                });
                self.show_status(&format!("Thinking blocks: {}", if hide { "hidden" } else { "visible" }));
                return;
            }
            if k("app.thinking.cycle") {
                self.cycle_thinking();
                return;
            }
            if k("app.model.cycleForward") {
                self.cycle_model(1);
                return;
            }
            if k("app.model.cycleBackward") {
                self.cycle_model(-1);
                return;
            }
            if k("app.editor.external") {
                if self.external_editor.is_none() {
                    self.show_status("No editor configured. Set $VISUAL or $EDITOR environment variable.");
                } else {
                    self.edit_request = Some(self.editor.expanded_text());
                }
                return;
            }
            if k("app.rlm.toggle") {
                self.toggle_rlm();
                return;
            }
            if k("app.rlm.focus") {
                self.open_rlm_focus();
                return;
            }
            if k("app.rlm.pane") {
                self.rlm_pane();
                return;
            }
            if k("app.history.search") {
                self.show_history_search();
                return;
            }
            if k("app.session.tree") {
                self.execute_slash_command("tree", "");
                return;
            }
            if k("app.session.fork") {
                self.execute_slash_command("fork", "");
                return;
            }
            if k("app.message.copy") {
                self.copy_last_message();
                return;
            }
        }
        self.feed_editor(data);
    }

    pub(super) fn feed_editor(&mut self, data: &str) {
        self.escapes.reset();
        let vim = match self.vim.as_mut() {
            Some(vim) => vim.handle_input(&mut self.editor, data),
            None => false,
        };
        if !vim && let Some(text) = self.editor.handle_input(data) {
            self.run_prompt(&text);
        }
        self.update_editor_border();
    }

    pub(super) fn handle_escape(&mut self) {
        if self.bash.is_some() {
            self.send(LaneCommand::AbortBash);
            return;
        }
        if self.retrying {
            self.retrying = false;
            self.send(LaneCommand::AbortRetry);
            return;
        }
        if self.summarizing_branch.is_some() {
            self.send(LaneCommand::Abort);
            return;
        }
        if self.compacting && self.operation.is_none() {
            self.status = "Cancelling compaction…".into();
            self.send(LaneCommand::Abort);
            return;
        }
        if let Some(operation) = self.operation.clone() {
            self.restore_queued_to_editor();
            self.aborting = true;
            self.status = format!("Aborting {operation}…");
            self.send(LaneCommand::Abort);
        }
        if self.operation.is_some() || !self.editor.text().trim().is_empty() {
            return;
        }
        let now = self.now();
        if self.qol.overlays {
            if self.escapes.tap(now) {
                self.fork_messages(true);
            }
            return;
        }
        if self.double_escape_action == "none" {
            return;
        }
        if self
            .last_escape
            .is_some_and(|t| now.saturating_duration_since(t) < DOUBLE_ESCAPE_WINDOW)
        {
            self.last_escape = None;
            if self.double_escape_action == "tree" {
                self.send(LaneCommand::Tree);
            } else {
                self.fork_messages(false);
            }
        } else {
            self.last_escape = Some(now);
        }
    }

    pub(super) fn handle_ctrl_c(&mut self) {
        let now = self.now();
        if self
            .last_ctrl_c
            .is_some_and(|t| now.duration_since(t) < CTRL_C_EXIT_WINDOW)
        {
            self.last_ctrl_c = None;
            self.exit = true;
            return;
        }
        self.last_ctrl_c = Some(now);
        self.editor.set_text("");
    }

    /// Queued user messages back into the editor (steering first); returns how many. Queued host items (runtime
    /// events, custom messages) are not the user's: they stay queued and shown (client-tui.ts
    /// `#restoreQueuedToEditor`).
    pub(super) fn restore_queued_to_editor(&mut self) -> usize {
        let all: Vec<Queued> = self.chat.queues().to_vec();
        let queued: Vec<Queued> = all.iter().filter(|q| q.user).cloned().collect();
        if queued.is_empty() {
            return 0;
        }
        let ordered: Vec<&Queued> = queued
            .iter()
            .filter(|q| q.kind == QueueKind::Steer)
            .chain(queued.iter().filter(|q| q.kind != QueueKind::Steer))
            .collect();
        let text = ordered.iter().map(|q| q.text.as_str()).collect::<Vec<_>>().join("\n\n");
        let current = self.editor.text();
        let combined = [text.as_str(), current.as_str()]
            .into_iter()
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.editor.set_text(&combined);
        for q in &ordered {
            self.send(LaneCommand::CancelQueued(q.entry_id.to_string()));
        }
        self.chat.set_queues(all.into_iter().filter(|q| !q.user).collect());
        ordered.len()
    }

    pub(super) fn record_history(&mut self, text: &str) {
        let now = jsrt::date::now_ms();
        self.history.append(text, now);
        self.editor.add_to_history(text);
    }

    pub(super) fn run_prompt(&mut self, text: &str) {
        let prompt = text.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        self.record_history(&prompt);
        if let Some(rest) = prompt.strip_prefix('!') {
            let excluded = rest.starts_with('!');
            let command = rest.strip_prefix('!').unwrap_or(rest).trim();
            if !command.is_empty() {
                self.run_bash(command, excluded);
                return;
            }
        }
        if let Some(rest) = prompt.strip_prefix('/') {
            let (name, args) = match rest.split_once(' ') {
                Some((n, a)) => (n.to_string(), a.trim().to_string()),
                None => (rest.to_string(), String::new()),
            };
            self.execute_slash_command(&name, &args);
            return;
        }
        self.editor.set_text("");
        let images = self.take_images(&prompt);
        if self.operation.is_some() {
            self.status = "Queueing steering message…".into();
            let id = self.enqueue(QueueKind::Steer, prompt, images);
            self.status = format!("Queued {id}.");
        } else {
            self.status = "Running turn…".into();
            self.send(LaneCommand::Prompt { text: prompt, images });
        }
    }

    /// Ask for the user messages: for the rewind picker (`rewind`) or the fork selector. Each request says which, so
    /// a rewind whose request failed (answered by an error, not [`LaneEvent::ForkMessages`]) never turns a later
    /// `/fork` into a rewind.
    pub(super) fn fork_messages(&mut self, rewind: bool) {
        self.rewind_pending = rewind;
        self.send(LaneCommand::ForkMessages);
    }

    pub(super) fn execute_slash_command(&mut self, name: &str, args: &str) {
        self.editor.set_text("");
        let Some(command) = Slash::parse(name, self.qol.overlays) else {
            self.status = format!("Unknown slash command: /{name}");
            return;
        };
        match command {
            Slash::Name => {
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
            Slash::Session => {
                let Some(id) = self.session_id.clone() else {
                    return;
                };
                let stats = self.session_stats(id);
                let text = session_info(&self.theme, &stats);
                self.notice(&text);
            }
            Slash::Copy if self.qol.overlays => self.open_copy_picker(),
            Slash::Copy => self.copy_last_message(),
            Slash::Hotkeys => {
                let notice = MarkdownNotice {
                    theme: Rc::clone(&self.theme),
                    title: "Keyboard Shortcuts".into(),
                    markdown: hotkeys_markdown(&self.keys),
                };
                self.chat.append_notice(Box::new(notice));
            }
            Slash::Quit => self.exit = true,
            Slash::New | Slash::Clear => {
                if self.operation.is_some() {
                    self.send(LaneCommand::Abort);
                }
                self.send(LaneCommand::NewSession);
            }
            Slash::Continue => {
                if self.operation.is_some() {
                    self.send(LaneCommand::Abort);
                }
                let id = args.trim();
                self.send(LaneCommand::ContinueSession((!id.is_empty()).then(|| id.to_string())));
            }
            Slash::ScopedModels => self.open_scoped_models(),
            Slash::Model | Slash::Models if self.qol.overlays && args.is_empty() => self.open_model_hub(),
            Slash::Model | Slash::Models => self.model_command(args),
            Slash::Resume => self.send(LaneCommand::ListSessions),
            Slash::Import => match path_argument(args) {
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
            },
            Slash::Thinking => self.thinking_command(args),
            Slash::Mode => match args.trim() {
                "" => self.request_mode(crate::modes::ModeRequest::Picker),
                name => match crate::modes::find(name) {
                    Some(mode) => self.request_mode(crate::modes::ModeRequest::Set(mode.engineering)),
                    None => self.show_status(&format!(
                        "Error: Unknown mode \"{name}\". Available modes: {}.",
                        crate::modes::MODES.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
                    )),
                },
            },
            Slash::Engineering => match crate::modes::engineering_arg(args) {
                Some(None) => self.request_mode(crate::modes::ModeRequest::Status),
                Some(Some(enabled)) => self.request_mode(crate::modes::ModeRequest::Set(enabled)),
                None => self.show_status("Usage: /engineering true|false"),
            },
            Slash::Persona => match crate::modes::persona_arg(args) {
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
            },
            Slash::KeepGoing => match args.trim().to_lowercase().as_str() {
                arg @ ("" | "on" | "off") => self.send(LaneCommand::KeepGoing(arg.to_string())),
                _ => self.show_status("Usage: /keepgoing on|off"),
            },
            Slash::Schedules => self.send(LaneCommand::Inspect {
                title: "/schedules".into(),
                request: "schedule.list".into(),
                payload: serde_json::json!({"text": true}),
            }),
            Slash::Autoreview => {
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
            Slash::Compact => {
                self.show_status("Compacting…");
                self.send(LaneCommand::Compact(
                    (!args.is_empty()).then_some(args).map(str::to_string),
                ));
            }
            Slash::Fork => self.fork_messages(false),
            Slash::Tree => self.send(LaneCommand::Tree),
            Slash::Rlm => match args.trim() {
                "focus" => self.open_rlm_focus(),
                "pane" => self.rlm_pane(),
                _ => self.toggle_rlm(),
            },
            Slash::Settings => self.send(LaneCommand::ReadSettings),
            Slash::Trust => self.send(LaneCommand::ReadTrust),
            Slash::Debug => {
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
            Slash::Reload => {
                self.show_status("Reloading plugins…");
                self.send(LaneCommand::ReloadPlugins);
            }
            Slash::Mcp => self.send(LaneCommand::Mcp(args.trim().to_string())),
            Slash::Extension => self.show_status(&format!("Error: {EXTENSIONS_UNSUPPORTED}")),
            Slash::Review => {
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
            Slash::Bug => self.open_bug_report(args),
            Slash::ArminSaysHi if args.is_empty() => {
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
            Slash::DementedElves if args.is_empty() => {
                let announcement = crate::easter_eggs::EarendilAnnouncement::new(Rc::clone(&self.theme));
                self.chat.append_notice(Box::new(announcement));
            }
            Slash::Logout => self.send(LaneCommand::Credentials),
            Slash::Login => self.send(LaneCommand::LoginProviders(args.to_string())),
            Slash::Changelog => self.send(LaneCommand::Changelog),
            Slash::Share => self.send(LaneCommand::Share {
                theme: self.active_theme_name(),
            }),
            Slash::Clone => {
                if self.operation.is_some() {
                    self.send(LaneCommand::Abort);
                }
                self.send(LaneCommand::CloneSession);
            }
            Slash::Export => self.send(LaneCommand::Export {
                path: path_argument(args),
                theme: self.active_theme_name(),
            }),
            Slash::Usage => self.send(LaneCommand::Usage),
            Slash::Goal => self.send(LaneCommand::Goal(args.to_string())),
            Slash::Observer => self.send(LaneCommand::Observer(args.to_string())),
            Slash::Agents if self.qol.overlays && args.is_empty() => self.send(LaneCommand::Agents),
            Slash::Agents | Slash::Memory | Slash::Skills | Slash::Experiments | Slash::Goals | Slash::Progress => {
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
            Slash::ArminSaysHi | Slash::DementedElves => {
                self.show_status(&format!("Error: /{name} is not available in the Rust build yet"));
            }
        }
    }

    /// `showError`: `Error: <message>` in the transcript.
    pub(super) fn thinking_command(&mut self, args: &str) {
        let levels = self.model.levels();
        if !args.is_empty() {
            match levels.iter().find(|l| **l == args.to_lowercase()) {
                Some(level) => {
                    let level = level.clone();
                    self.set_thinking(&level);
                    self.status = format!("Thinking level: {level}.");
                }
                None => {
                    self.status = format!(
                        "Error: Unknown thinking level \"{args}\". Available levels: {}.",
                        levels.join(", ")
                    );
                }
            }
            return;
        }
        let items = levels
            .iter()
            .map(|level| SelectItem {
                value: level.clone(),
                label: if *level == self.thinking_level {
                    format!("{level} (selected)")
                } else {
                    level.clone()
                },
                description: thinking_description(level).map(str::to_string),
            })
            .collect();
        let current = self.thinking_level.clone();
        self.open_selection("Select thinking level:", items, Some(&current), Purpose::Thinking);
    }

    pub(super) fn set_thinking(&mut self, level: &str) {
        self.send(LaneCommand::SetThinking(level.to_string()));
        self.thinking_level = level.to_string();
        self.update_editor_border();
    }

    /// `/mode` and `/engineering` act on the settings' mode, which the client reads.
    pub(super) fn request_mode(&mut self, request: crate::modes::ModeRequest) {
        self.mode_request = Some(request);
        self.send(LaneCommand::ReadMode);
    }

    pub(super) fn mode_read(&mut self, engineering: bool, always_skills: Vec<String>) {
        use crate::modes::{MODES, ModeRequest, current};
        match self.mode_request.take() {
            Some(ModeRequest::Picker) => {
                let selected = current(engineering);
                let items = MODES
                    .iter()
                    .map(|mode| SelectItem {
                        value: mode.id.to_string(),
                        label: if mode.id == selected.id {
                            format!("{} (selected)", mode.label)
                        } else {
                            mode.label.to_string()
                        },
                        description: Some(mode.description.to_string()),
                    })
                    .collect();
                self.open_selection("Select mode:", items, Some(selected.id), Purpose::Mode(always_skills));
            }
            Some(ModeRequest::Status) => self.show_status(&crate::modes::status(engineering, &always_skills)),
            Some(ModeRequest::Set(enabled)) => self.set_engineering(enabled, &always_skills),
            None => {}
        }
    }

    /// `/autoreview on`: one account is reviewed as; with several, the user picks one or all of them.
    pub(super) fn autoreview_accounts(&mut self, accounts: Vec<(String, String)>) {
        match accounts.as_slice() {
            [] => self.show_status("No gh account is logged in: run gh auth login, then /autoreview on"),
            [(login, host)] => self.send(LaneCommand::Autoreview(AutoreviewRequest::On {
                accounts: None,
                reviewing_as: format!("{host}/{login}"),
            })),
            _ => {
                let mut items: Vec<SelectItem> = accounts
                    .iter()
                    .enumerate()
                    .map(|(index, (login, host))| SelectItem {
                        value: index.to_string(),
                        label: format!("{login} ({host})"),
                        description: None,
                    })
                    .collect();
                items.push(SelectItem {
                    value: ALL_ACCOUNTS.into(),
                    label: ALL_ACCOUNTS.into(),
                    description: None,
                });
                self.open_selection(
                    "Review pull requests as",
                    items,
                    None,
                    Purpose::AutoreviewAccount(accounts),
                );
            }
        }
    }

    /// Write the `engineering` setting; the session's next request renders its prompt in that mode.
    pub(super) fn set_engineering(&mut self, enabled: bool, always_skills: &[String]) {
        self.send(LaneCommand::SaveSetting {
            key: "engineering".into(),
            value: Value::Bool(enabled),
        });
        self.show_status(&crate::modes::changed(enabled, always_skills));
    }

    /// Pi's Shift+Tab.
    pub(super) fn cycle_thinking(&mut self) {
        if !self.model.reasoning {
            self.show_status("Current model does not support thinking");
            return;
        }
        let levels = self.model.levels();
        if levels.len() < 2 {
            return;
        }
        let index = levels.iter().position(|l| *l == self.thinking_level);
        let next = levels[index.map_or(0, |i| (i + 1) % levels.len())].clone();
        self.set_thinking(&next);
        self.show_status(&format!("Thinking level: {next}"));
    }

    /// Pi's Ctrl+P / Shift+Ctrl+P.
    pub(super) fn cycle_model(&mut self, direction: isize) {
        let scope = self.scoped_ids.clone().unwrap_or_else(|| self.enabled_models.clone());
        let next = match next_model(&self.models, &self.model, direction, &scope) {
            Ok(model) => model.clone(),
            Err(error) => {
                self.show_status(error);
                return;
            }
        };
        self.send(LaneCommand::SelectModel {
            provider: next.provider.to_string(),
            id: next.id.to_string(),
        });
        let suffix = if next.reasoning && self.thinking_level != "off" {
            format!(" (thinking: {})", self.thinking_level)
        } else {
            String::new()
        };
        let name = if next.name.is_empty() { &next.id } else { &next.name };
        self.show_status(&format!("Switched to {name}{suffix}"));
        self.model = next;
    }

    /// What the settings selector answered (a key or a click).
    pub(super) fn model_command(&mut self, args: &str) {
        let exact = |query: &str| -> Option<ModelInfo> {
            let q = query.to_lowercase();
            let matches: Vec<&ModelInfo> = self
                .models
                .iter()
                .filter(|m| format!("{}/{}", m.provider, m.id).to_lowercase() == q || m.id.to_lowercase() == q)
                .collect();
            (matches.len() == 1).then(|| matches[0].clone())
        };
        if !args.is_empty() {
            match exact(args) {
                Some(model) => self.select_model(model),
                None => self.status = format!("Error: Unknown model: {args}"),
            }
            return;
        }
        let items: Vec<SelectItem> = self
            .models
            .iter()
            .map(|m| SelectItem {
                value: format!("{}/{}", m.provider, m.id),
                label: if m.provider == self.model.provider && m.id == self.model.id {
                    format!("{} (selected)", m.name)
                } else {
                    m.name.clone()
                },
                description: Some(format!("{}/{}", m.provider, m.id)),
            })
            .collect();
        let selected = format!("{}/{}", self.model.provider, self.model.id);
        self.open_selection("Select model:", items, Some(&selected), Purpose::Model);
    }

    pub(super) fn select_model(&mut self, model: ModelInfo) {
        self.send(LaneCommand::SelectModel {
            provider: model.provider.to_string(),
            id: model.id.to_string(),
        });
        self.status = format!("Selected {}/{}.", model.provider, model.id);
        self.model = model;
    }
}
