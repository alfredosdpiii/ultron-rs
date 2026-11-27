//! Frame layout, header/dock/footer rendering, stats, and clipboard export for [`super::App`].

use super::*;

impl App {
    pub(super) fn last_assistant_text(&self) -> Option<String> {
        self.transcript
            .iter()
            .rev()
            .filter(|m| m["role"] == "assistant")
            .find(|m| !(m["stopReason"] == "aborted" && m["content"].as_array().is_none_or(Vec::is_empty)))
            .map(protocol::pi_message::text)
    }

    pub(super) fn copy_last_message(&mut self) {
        match self.last_assistant_text().filter(|t| !t.is_empty()) {
            None => self.show_status("Error: No agent messages to copy yet."),
            Some(text) => self.copy_to_clipboard(&text, "Copied last agent message to clipboard"),
        }
    }

    pub(super) fn session_stats(&self, session_id: String) -> SessionStats {
        let mut s = SessionStats {
            session_file: self.session_file.clone(),
            session_id,
            name: self.session_name.clone(),
            ..SessionStats::default()
        };
        for m in &self.transcript {
            match m["role"].as_str() {
                Some("user") => s.user_messages += 1,
                Some("assistant") => {
                    s.assistant_messages += 1;
                    let u = &m["usage"];
                    s.input += u["input"].as_u64().unwrap_or(0);
                    s.output += u["output"].as_u64().unwrap_or(0);
                    s.cache_read += u["cacheRead"].as_u64().unwrap_or(0);
                    s.cache_write += u["cacheWrite"].as_u64().unwrap_or(0);
                    s.cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
                    s.tool_calls += m["content"]
                        .as_array()
                        .map_or(0, |b| b.iter().filter(|b| b["type"] == "toolCall").count());
                }
                Some("toolResult") => s.tool_results += 1,
                Some("custom") if m["customType"] == protocol::custom_types::NUDGE => s.auto_continues += 1,
                _ => {}
            }
        }
        s.context = self
            .context()
            .map(|(tokens, percent)| (tokens, self.model.context_window, percent));
        s
    }

    pub(super) fn footer_view(&self) -> FooterView {
        let mut v = FooterView {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            git_branch: self.footer_git_branch.clone(),
            using_subscription: config::providers::runs_claude_code(&self.model.provider),
            session_name: self.session_name.clone(),
            thinking_level: self.thinking_level.clone(),
            auto_compact: self.auto_compact,
            model: Some(FooterModel {
                id: self.model.id.clone(),
                provider: self.model.provider.clone(),
                reasoning: self.model.reasoning,
                context_window: self.model.context_window,
            }),
            ..FooterView::default()
        };
        use crate::footer_data::{CLAUDE_USAGE_STATUS, claude_code_usage_text};
        if let Some(text) = claude_code_usage_text(&self.transcript) {
            v.statuses.push((CLAUDE_USAGE_STATUS.into(), text));
        }
        let mut providers: Vec<&str> = self.models.iter().map(|m| m.provider.as_str()).collect();
        providers.sort_unstable();
        providers.dedup();
        v.provider_count = providers.len();
        for m in self.transcript.iter().filter(|m| m["role"] == "assistant") {
            let u = &m["usage"];
            v.input += u["input"].as_u64().unwrap_or(0);
            v.output += u["output"].as_u64().unwrap_or(0);
            v.cache_read += u["cacheRead"].as_u64().unwrap_or(0);
            v.cache_write += u["cacheWrite"].as_u64().unwrap_or(0);
            v.cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
        }
        let p = &self.path_usage;
        v.input += p["input"].as_u64().unwrap_or(0);
        v.output += p["output"].as_u64().unwrap_or(0);
        v.cache_read += p["cacheRead"].as_u64().unwrap_or(0);
        v.cache_write += p["cacheWrite"].as_u64().unwrap_or(0);
        v.cost += p["cost"].as_f64().unwrap_or(0.0);
        if let Some(last) = self.transcript.iter().rev().find(|m| m["role"] == "assistant") {
            let u = &last["usage"];
            let read = u["cacheRead"].as_f64().unwrap_or(0.0);
            let prompt = u["input"].as_f64().unwrap_or(0.0) + read + u["cacheWrite"].as_f64().unwrap_or(0.0);
            if prompt > 0.0 {
                v.latest_cache_hit_rate = Some(read / prompt * 100.0);
            }
        }
        if let Some((_, percent)) = self.context() {
            v.context_percent = Some(percent);
            v.context_window = Some(self.model.context_window);
        }
        v
    }

    /// The transcript document: the startup header, then the conversation.
    pub(super) fn document(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.header_lines(width);
        self.mouse_layout.header_rows = lines.len();
        lines.extend(self.chat.render_transcript(width));
        lines
    }

    pub(super) fn header_lines(&mut self, width: usize) -> Vec<String> {
        self.header.details = match &self.session_id {
            Some(id) => self
                .theme
                .fg(Fg::Dim, &format!("Server: {}\nSession: {id}", self.server_id)),
            None => String::new(),
        };
        self.header.rows = Some(self.rows);
        self.header.render(&self.keys, width)
    }

    /// The dock under the transcript: the running-work panel, pending messages, status, editor (or selector) and
    /// footer.
    pub(super) fn dock(&mut self, width: usize) -> Vec<Vec<String>> {
        let pending = self.dock_pending(width);
        let rlm_focus = matches!(self.panel, Some(Panel::RlmFocus(_)));
        let snapshot = (self.rlm_visible || !rlm_focus).then(|| self.rlm_snapshot());
        let options = self.rlm_options();
        let (running, status) = self.dock_status(width, snapshot.as_ref(), &options, rlm_focus);
        let footer = self.dock_footer(width, snapshot.as_ref(), &options, rlm_focus);
        let focus_height = self.rows.saturating_sub(footer.len() + 1).max(10);
        let editor = self.dock_slot(width, focus_height, &options);
        let mut parts = vec![running, pending, status];
        if let Some(line) = self.status_line(width) {
            parts.push(vec![line]);
        }
        if let Some(chips) = self.dock_attachment_chips(width) {
            parts.push(chips);
        }
        parts.push(editor);
        parts.push(footer);
        parts
    }

    fn dock_pending(&mut self, width: usize) -> Vec<String> {
        let mut pending = self.chat.render_pending(width);
        if let Some(bash) = self.bash.as_mut() {
            pending.extend(bash.render(width));
        }
        pending
    }

    fn dock_status(
        &mut self,
        width: usize,
        snapshot: Option<&rlm_views::visualizer::RlmSnapshot>,
        options: &rlm_views::graph::RlmViewOptions,
        rlm_focus: bool,
    ) -> (Vec<String>, Vec<String>) {
        let mut status = Vec::new();
        if !self.status.is_empty() {
            status.extend(Text::new(&self.theme.fg(Fg::Dim, &self.status), 1, 0, None).render(width));
        }
        if self.rlm_visible
            && !rlm_focus
            && let Some(snapshot) = snapshot
        {
            let inner = width.saturating_sub(2).max(1);
            status.extend(
                rlm_views::graph::render_rlm_dock(snapshot, inner, options, usize::MAX)
                    .into_iter()
                    .map(|l| format!(" {l}")),
            );
        }
        let running = self.running_work_lines(snapshot, width);
        status.extend(self.chat.render_status(width));
        let mut waves = self.wave_summary(width);
        if !running.is_empty() {
            waves.truncate(1);
        }
        status.extend(waves);
        (running, status)
    }

    fn dock_footer(
        &self,
        width: usize,
        snapshot: Option<&rlm_views::visualizer::RlmSnapshot>,
        options: &rlm_views::graph::RlmViewOptions,
        rlm_focus: bool,
    ) -> Vec<String> {
        let runtime_line = match snapshot {
            Some(snapshot) if !self.rlm_visible && !rlm_focus => {
                rlm_views::graph::render_rlm_footer(snapshot, width.max(1), options)
            }
            _ => None,
        };
        let mut footer = render_footer(&self.theme, &self.footer_view(), width);
        footer.extend(runtime_line);
        footer
    }

    fn dock_attachment_chips(&self, width: usize) -> Option<Vec<String>> {
        if !self.qol.attachments || self.selection.is_some() || self.panel.is_some() {
            return None;
        }
        let text = self.editor.text();
        let chips = chip_band(&draft_chips(&text, self.editor.pastes(), &self.attachments), width);
        (!chips.is_empty()).then_some(chips)
    }

    fn dock_slot(
        &mut self,
        width: usize,
        focus_height: usize,
        options: &rlm_views::graph::RlmViewOptions,
    ) -> Vec<String> {
        if let Some(selection) = self.selection.as_mut() {
            let mut lines = Text::new(&bold(&selection.title), 1, 1, None).render(width);
            lines.extend(selection.list.render(width));
            return lines;
        }
        if matches!(self.panel, Some(Panel::RlmFocus(_))) {
            let focus_snapshot = self.rlm_snapshot();
            if let Some(Panel::RlmFocus(focus)) = self.panel.as_mut() {
                return focus.render(
                    &self.keys,
                    &focus_snapshot,
                    width,
                    focus_height,
                    &options.style,
                    options.spinner_frame,
                );
            }
        }
        if let Some(lines) = self.render_selector_panel(width) {
            return lines;
        }
        let now = self.now();
        if let Some(lines) = self.render_dialog_panel(width, now) {
            return lines;
        }
        self.editor.focused = !self.recovering && !(self.rlm_pane_focused && self.rlm_pane_shown());
        self.editor.terminal_rows = self.rows;
        self.editor.render(width)
    }

    fn render_selector_panel(&mut self, width: usize) -> Option<Vec<String>> {
        match self.panel.as_mut()? {
            Panel::Fork(selector) => Some(selector.render(width)),
            Panel::HistorySearch(search) => Some(search.render(width)),
            Panel::Tree(tree) => Some(tree.render(width)),
            Panel::Settings(selector) => Some(selector.render(width)),
            Panel::Logout(selector) => Some(selector.render(width)),
            Panel::Trust(selector) => Some(selector.render(width)),
            Panel::Login(selector, ..) => Some(selector.render(width)),
            _ => None,
        }
    }

    fn render_dialog_panel(&mut self, width: usize, now: Instant) -> Option<Vec<String>> {
        match self.panel.as_mut()? {
            Panel::OAuthLogin(dialog, _) => Some(dialog.render(&self.theme, width)),
            Panel::ScopedModels { selector, .. } => Some(selector.render(width)),
            Panel::LoginPrompt {
                provider,
                method,
                input,
            } => Some(Self::render_login_prompt(
                &self.theme,
                &self.keys,
                &provider.name,
                method,
                input,
                width,
            )),
            Panel::Sessions(selector) | Panel::PiSessions(selector) => Some(selector.render(width)),
            Panel::ExtensionInput { dialog, .. } => Some(dialog.render(width, now)),
            Panel::TextEditor { dialog, .. } => Some(dialog.render(width, self.rows)),
            Panel::ShareLoader(loader) => Some(loader.render(width)),
            Panel::BugReport(flow) => Some(flow.render(width, self.rows)),
            _ => None,
        }
    }

    fn render_login_prompt(
        theme: &Rc<Theme>,
        keys: &KeybindingsManager,
        provider_name: &str,
        method: &str,
        input: &mut tui::Input,
        width: usize,
    ) -> Vec<String> {
        let border = theme.fg(Fg::Border, &"─".repeat(width.max(1)));
        let mut lines = vec![border.clone()];
        lines.extend(
            Text::new(
                &theme.fg(Fg::Accent, &bold(&format!("Login to {provider_name}"))),
                1,
                0,
                None,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.extend(Text::new(&theme.fg(Fg::Text, &format!("Enter {method}")), 1, 0, None).render(width));
        lines.extend(input.render(width));
        let hint = |id: &str, d: &str| crate::components::key_hint(theme, &key_text(keys, id), d);
        lines.extend(
            Text::new(
                &format!(
                    "({} {})",
                    hint("tui.select.cancel", "to cancel,"),
                    hint("tui.select.confirm", "to submit")
                ),
                1,
                0,
                None,
            )
            .render(width),
        );
        lines.push(border);
        lines
    }

    /// One full-screen frame of `height` rows: the chat, with the RLM pane beside it while it shows (`SplitView`).
    pub fn render(&mut self, width: usize, height: usize) -> Vec<String> {
        self.rows = height;
        self.cols = width;
        if let Some(frame) = self.overlay_frame(width, height) {
            self.viewport.hide_layout();
            self.mouse_layout = MouseLayout::default();
            return frame;
        }
        let frame = self.render_columns(width, height);
        self.viewport.finish_frame(frame, width, height)
    }

    /// The chat column and the RLM pane beside it.
    pub(super) fn render_columns(&mut self, width: usize, height: usize) -> Vec<String> {
        let (main_width, side_width) = tui::split_widths(width, self.rlm_pane_shown(), rlm_pane_width(width));
        let main = self.render_main(main_width, height);
        if side_width == 0 {
            return main;
        }
        let snapshot = self.rlm_snapshot();
        let style = rlm_views::visualizer::RlmStyle::themed(Rc::clone(&self.theme));
        let toggle = self.keys.get_keys("app.rlm.pane").first().cloned();
        let mut side = self.rlm_pane.render(
            &self.keys,
            &snapshot,
            side_width,
            height,
            self.rlm_pane_focused,
            toggle.as_deref(),
            &style,
        );
        side.truncate(height);
        let border = self.theme.fg(
            if self.rlm_pane_focused {
                Fg::BorderAccent
            } else {
                Fg::Border
            },
            "│",
        );
        let mut main = main;
        main.resize(height, String::new());
        tui::render_split(&main, &side, main_width, side_width, &border)
    }

    /// The chat column: transcript above the dock, `height` rows.
    pub(super) fn render_main(&mut self, width: usize, height: usize) -> Vec<String> {
        let mut document = self.viewport.take_content();
        let header = self.header_lines(width);
        self.mouse_layout.header_rows = header.len();
        self.chat.render_items(width);
        refill(&mut document, header.iter().chain(self.chat.lines()));
        let dock = self.dock(width);
        let editor = dock.len() - 2;
        let dock_entries: Vec<StackEntry> = (0..dock.len())
            .map(|i| StackEntry {
                min_size: if i == editor { 3 } else { 0 },
                ..StackEntry::default()
            })
            .collect();
        let dock_intrinsic: Vec<usize> = dock.iter().map(Vec::len).collect();
        let dock_height: usize = tui::allocate_stack_sizes(&dock_entries, &dock_intrinsic, None)
            .iter()
            .sum();
        let root = [
            StackEntry {
                basis: Some(0),
                grow: 1,
                min_size: 1,
                ..StackEntry::default()
            },
            StackEntry {
                min_size: 1,
                ..StackEntry::default()
            },
        ];
        let sizes = tui::allocate_stack_sizes(&root, &[document.len(), dock_height], Some(height));
        let dock_lines = stack_lines(dock, &dock_entries, &[], sizes[1]);
        let dock_sizes = tui::allocate_stack_sizes(&dock_entries, &dock_intrinsic, Some(sizes[1]));
        self.mouse_layout.slot = Some(tui::Rect {
            x: 0,
            y: sizes[0] + dock_sizes[..editor].iter().sum::<usize>(),
            width,
            height: dock_sizes[editor],
        });
        self.mouse_layout.transcript = Some(tui::Rect {
            x: 0,
            y: 0,
            width,
            height: sizes[0],
        });
        self.mouse_layout.item_heights = self.chat.item_heights(width);
        self.style_viewport();
        let mut lines = self.viewport.layout(
            document,
            tui::Rect {
                x: 0,
                y: 0,
                width,
                height: sizes[0],
            },
        );
        lines.extend(dock_lines);
        lines
    }

    /// The transcript scrollbar's theme colors and the jump-to-end label (tui-renderer.ts `scrollToEndIndicator`).
    pub(super) fn style_viewport(&mut self) {
        let (track, thumb) = (Rc::clone(&self.theme), Rc::clone(&self.theme));
        self.viewport.view.track_style = Box::new(move |t| track.fg(Fg::ScrollbarTrack, t));
        self.viewport.view.thumb_style = Box::new(move |t| thumb.fg(Fg::ScrollbarThumb, t));
        let shortcut = key_display_text(&self.keys, "tui.altScreen.bottom");
        let label = if shortcut.is_empty() {
            " ↓ Jump to latest message ".to_string()
        } else {
            format!(" ↓ Jump to latest message · {shortcut} ")
        };
        self.viewport.indicator = Some(self.theme.bg(Bg::SelectedBg, &self.theme.fg(Fg::Text, &label)));
    }

    /// What stays on the main screen after exit: the layout rendered without a height, where the transcript gets its
    /// minimum of one row (the header's first line) and the dock its natural height.
    pub fn exit_document(&mut self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = self.document(width).into_iter().take(1).collect();
        for part in self.dock(width) {
            lines.extend(part);
        }
        lines
    }
}
