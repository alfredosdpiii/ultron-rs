//! Keyboard, editor, prompt, slash-command, thinking/mode, and model-selection handlers for [`super::App`].

mod slash;

use super::*;

type AppShortcut = (&'static str, fn(&mut App));

const APP_SHORTCUTS: &[AppShortcut] = &[
    ("app.clear", App::handle_ctrl_c),
    ("app.model.select", App::key_model_select),
    ("app.message.followUp", App::key_follow_up),
    ("app.message.dequeue", App::key_dequeue),
    ("app.tools.expand", App::key_toggle_tools_expanded),
    ("app.thinking.toggle", App::key_toggle_thinking),
    ("app.thinking.cycle", App::cycle_thinking),
    ("app.model.cycleForward", App::key_cycle_model_forward),
    ("app.model.cycleBackward", App::key_cycle_model_backward),
    ("app.editor.external", App::key_external_editor),
    ("app.rlm.toggle", App::toggle_rlm),
    ("app.rlm.focus", App::open_rlm_focus),
    ("app.rlm.pane", App::rlm_pane),
    ("app.history.search", App::show_history_search),
    ("app.session.tree", App::key_session_tree),
    ("app.session.fork", App::key_session_fork),
    ("app.message.copy", App::copy_last_message),
];

impl App {
    /// Take terminal input. False when it left the frame as it was (pointer motion that moved no hover), so the
    /// frame need not be painted again.
    pub fn handle_input(&mut self, data: &str) -> bool {
        let expecting = self
            .theme_controller
            .as_ref()
            .is_some_and(crate::theme_setup::ThemeController::expects_background);
        let read = self.osc11.input(data, expecting);
        let mut changed = read.forward.is_empty();
        if let Some(rgb) = read.reply {
            self.background_reported(rgb);
            changed = true;
        }
        for data in read.forward {
            changed |= self.handle_terminal_input(&data);
        }
        changed
    }

    pub(super) fn handle_terminal_input(&mut self, data: &str) -> bool {
        if let Some((height, width)) = parse_cell_size_response(data) {
            if height > 0 && width > 0 {
                let mut images = self.chat.image_settings();
                images.cell = tui::Dimensions {
                    width_px: width,
                    height_px: height,
                };
                self.chat.set_image_settings(images);
            }
            return true;
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
            return true;
        }
        self.handle_key(data)
    }

    /// False when the input left the frame as it was ([`App::fullscreen_input`]).
    pub(super) fn handle_key(&mut self, data: &str) -> bool {
        if let Some(changed) = self.fullscreen_input(data) {
            return changed;
        }
        if !(self.handle_modal_key(data)
            || self.handle_qol_key(data)
            || self.handle_priority_key(data)
            || self.handle_app_shortcut(data))
        {
            self.feed_editor(data);
        }
        true
    }

    fn handle_modal_key(&mut self, data: &str) -> bool {
        if tui::is_key_release(data) {
            return true;
        }
        if self.recovering {
            self.handle_recovering_key(data);
            return true;
        }
        if self.selection.is_some() {
            self.handle_select_input(data);
            return true;
        }
        if self.overlay.is_some() {
            self.handle_overlay_input(data);
            return true;
        }
        if self.panel.is_some() {
            self.handle_panel_input(data);
            return true;
        }
        if self.rlm_pane_focused && self.rlm_pane_shown() {
            self.handle_rlm_pane_key(data);
            return true;
        }
        false
    }

    fn handle_recovering_key(&mut self, data: &str) {
        if self.keys.matches(data, "app.clear") {
            self.handle_ctrl_c();
        } else if self.editor.text().is_empty() && self.keys.matches(data, "app.exit") {
            self.exit = true;
        }
    }

    fn handle_rlm_pane_key(&mut self, data: &str) {
        if self.keys.matches(data, "app.rlm.pane") {
            self.rlm_pane();
            return;
        }
        let snapshot = self.rlm_snapshot();
        let height = self.rows;
        if self.rlm_pane.handle_input(&self.keys, data, &snapshot, height) == rlm_views::pane::PaneEvent::Close {
            self.close_rlm_pane();
        }
    }

    fn handle_qol_key(&mut self, data: &str) -> bool {
        if self.qol.overlays && self.handle_overlay_keys(data) {
            return true;
        }
        if self.qol.overlays && data == "\x1b\x1b" && self.operation.is_none() && self.editor.text().trim().is_empty() {
            self.fork_messages(true);
            return true;
        }
        self.qol.attachments && data.starts_with(PASTE_START) && self.paste_attachments(data)
    }

    fn handle_priority_key(&mut self, data: &str) -> bool {
        if self.keys.matches(data, "app.clipboard.pasteImage") {
            if let Some(clipboard) = &self.clipboard {
                self.pending_reads.push_back(false);
                clipboard.request(ClipboardRead::ImageOrText);
            }
            return true;
        }
        if self.keys.matches(data, "app.suspend") {
            self.suspend_request = true;
            return true;
        }
        if self.keys.matches(data, "app.interrupt") {
            self.handle_interrupt_key(data);
            return true;
        }
        if self.keys.matches(data, "app.exit") && self.editor.text().is_empty() {
            self.exit = true;
            return true;
        }
        false
    }

    fn handle_interrupt_key(&mut self, data: &str) {
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
    }

    fn handle_app_shortcut(&mut self, data: &str) -> bool {
        if self.keys.matches(data, "app.exit")
            || self.keys.matches(data, "tui.editor.historyPrevious")
            || self.keys.matches(data, "tui.editor.historyNext")
        {
            return false;
        }
        for &(id, action) in APP_SHORTCUTS {
            if self.keys.matches(data, id) {
                action(self);
                return true;
            }
        }
        false
    }

    fn key_model_select(&mut self) {
        if self.qol.overlays {
            self.open_model_hub();
        } else {
            self.execute_slash_command("model", "");
        }
    }

    fn key_follow_up(&mut self) {
        let text = self.editor.expanded_text().trim().to_string();
        if !text.is_empty() {
            self.editor.set_text("");
            self.record_history(&text);
            self.status = "Queueing follow-up…".into();
            let images = self.take_images(&text);
            let id = self.enqueue(QueueKind::FollowUp, text, images);
            self.status = format!("Queued {id}.");
        }
    }

    fn key_dequeue(&mut self) {
        let restored = self.restore_queued_to_editor();
        self.show_status(&match restored {
            0 => "No queued messages to restore".to_string(),
            1 => "Restored 1 queued message to editor".to_string(),
            n => format!("Restored {n} queued messages to editor"),
        });
    }

    fn key_toggle_tools_expanded(&mut self) {
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
    }

    fn key_toggle_thinking(&mut self) {
        let hide = !self.chat.hide_thinking;
        self.chat.set_hide_thinking(hide);
        self.send(LaneCommand::SaveSetting {
            key: "hideThinkingBlock".into(),
            value: Value::Bool(hide),
        });
        self.show_status(&format!("Thinking blocks: {}", if hide { "hidden" } else { "visible" }));
    }

    fn key_cycle_model_forward(&mut self) {
        self.cycle_model(1);
    }

    fn key_cycle_model_backward(&mut self) {
        self.cycle_model(-1);
    }

    fn key_external_editor(&mut self) {
        if self.external_editor.is_none() {
            self.show_status("No editor configured. Set $VISUAL or $EDITOR environment variable.");
        } else {
            self.edit_request = Some(self.editor.expanded_text());
        }
    }

    fn key_session_tree(&mut self) {
        self.execute_slash_command("tree", "");
    }

    fn key_session_fork(&mut self) {
        self.execute_slash_command("fork", "");
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
        if self.abort_active_work() || !self.editor.text().trim().is_empty() {
            return;
        }
        self.handle_double_escape();
    }

    fn abort_active_work(&mut self) -> bool {
        if self.bash.is_some() {
            self.send(LaneCommand::AbortBash);
            return true;
        }
        if self.retrying {
            self.retrying = false;
            self.send(LaneCommand::AbortRetry);
            return true;
        }
        if self.summarizing_branch.is_some() {
            self.send(LaneCommand::Abort);
            return true;
        }
        if self.compacting && self.operation.is_none() {
            self.status = "Cancelling compaction…".into();
            self.send(LaneCommand::Abort);
            return true;
        }
        if let Some(operation) = self.operation.clone() {
            self.restore_queued_to_editor();
            self.aborting = true;
            self.status = format!("Aborting {operation}…");
            self.send(LaneCommand::Abort);
            return true;
        }
        false
    }

    fn handle_double_escape(&mut self) {
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

    /// `/learning` (or `/learning switch`): pick a saved topic from `~/.ultron/learning/` or start a new one.
    pub(super) fn open_learning_picker(&mut self) {
        let topics =
            crate::learning_topics::learning_dir().map_or_default(|dir| crate::learning_topics::list_topics(&dir));
        let active_value = topics.iter().find(|t| t.active).map(|t| t.topic.clone());
        let mut items: Vec<SelectItem> = topics
            .into_iter()
            .map(|t| {
                let label = if t.active {
                    format!("{} (active)", t.topic)
                } else {
                    t.topic.clone()
                };
                let desc = if t.due_count > 0 {
                    format!("{}% mastery · {} review(s) due", t.mastery_pct, t.due_count)
                } else {
                    format!("{}% mastery · up to date", t.mastery_pct)
                };
                SelectItem {
                    value: t.topic,
                    label,
                    description: Some(desc),
                }
            })
            .collect();
        items.push(SelectItem {
            value: crate::learning_topics::new_topic_sentinel().to_string(),
            label: "+ Start a new topic".to_string(),
            description: Some("Type `/learning <topic>` to start a new Socratic session".to_string()),
        });
        self.open_selection(
            "Select learning topic (~/.ultron/learning):",
            items,
            active_value.as_deref(),
            Purpose::LearningTopic,
        );
    }

    pub(in crate::app) fn handle_learning_topic_choice(&mut self, value: &str) {
        if value == crate::learning_topics::new_topic_sentinel() {
            self.editor.set_text("/learning ");
            self.show_status("Type a topic after `/learning ` and press Enter");
            return;
        }
        self.send_learning_prompt(&format!("switch {value}"));
    }

    /// `/learning <new-topic>`: pick how to start before sending the first prompt.
    pub(super) fn open_learning_start_picker(&mut self, topic: &str) {
        let items = vec![
            SelectItem {
                value: "beginner".to_string(),
                label: "Beginner — teach me first".to_string(),
                description: Some(
                    "Mental model + roadmap, teach concept 1 clearly with an example, then start the Socratic loop"
                        .to_string(),
                ),
            },
            SelectItem {
                value: "hands-on".to_string(),
                label: "Hands-on — build an example first".to_string(),
                description: Some(
                    "Teach by walking through & running a tiny concrete example first, then Socratic checks"
                        .to_string(),
                ),
            },
            SelectItem {
                value: "probe".to_string(),
                label: "Familiar — quick diagnostic first".to_string(),
                description: Some("3–5 short questions to map your exact edge, then drill your gaps".to_string()),
            },
            SelectItem {
                value: "advanced".to_string(),
                label: "Advanced — skip basics, probe hard".to_string(),
                description: Some(
                    "Skip fundamentals, test edge cases & internals, and drill straight into bottlenecks".to_string(),
                ),
            },
        ];
        self.open_selection(
            &format!("Start \"{topic}\" — how do you want to begin?"),
            items,
            Some("beginner"),
            Purpose::LearningStart(topic.to_string()),
        );
    }

    pub(in crate::app) fn handle_learning_start_choice(&mut self, topic: &str, mode: &str) {
        self.send_learning_prompt(&format!("--{mode} {topic}"));
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
