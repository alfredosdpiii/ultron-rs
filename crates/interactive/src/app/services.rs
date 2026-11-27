//! Startup settings, theme controller, clipboard/git/viewport services, and frame planning for [`super::App`].

use super::*;

impl App {
    pub fn apply_startup_settings(&mut self, settings: &Value, var: &dyn Fn(&str) -> Option<String>) {
        self.show_hardware_cursor = match &settings["showHardwareCursor"] {
            Value::Null => var(config::keys::HARDWARE_CURSOR_ENV).as_deref() == Some("1"),
            value => jsrt::value::truthy(Some(value)),
        };
        self.clear_on_shrink = match settings["terminal"].get("clearOnShrink") {
            None => var(config::keys::CLEAR_ON_SHRINK_ENV).as_deref() == Some("1"),
            Some(value) => jsrt::value::truthy(Some(value)),
        };
        if let Some(action) = settings["doubleEscapeAction"].as_str() {
            self.double_escape_action = action.to_string();
        }
        self.header.splash = self.splash_allowed && settings["quietStartup"] != true;
        self.rlm_pane_auto = settings["rlmPaneAutoOpen"] != false && !env_off(var(RLM_PANE_AUTO_ENV).as_deref());
        if let Some(padding) = settings["editorPaddingX"].as_f64() {
            self.apply_local_setting("editorPaddingX", &Value::from(padding));
        }
        if let Some(visible) = settings["autocompleteMaxVisible"].as_f64() {
            self.apply_local_setting("autocompleteMaxVisible", &Value::from(visible));
        }
    }

    /// `#applySettingLocally` (and what reads its setting on use): a presentation setting changed in `/settings`.
    pub(super) fn apply_local_setting(&mut self, key: &str, value: &Value) {
        if self.apply_bool_setting(key, value) {
            return;
        }
        match key {
            "theme" => self.apply_theme_setting(value),
            "editorPaddingX" => self.apply_editor_padding(value),
            "autocompleteMaxVisible" => self.apply_autocomplete_max_visible(value),
            "doubleEscapeAction" => self.double_escape_action = value.as_str().unwrap_or("tree").to_string(),
            _ => {}
        }
    }

    fn apply_bool_setting(&mut self, key: &str, value: &Value) -> bool {
        match key {
            "hideThinkingBlock" => self.chat.set_hide_thinking(*value == true),
            "showHardwareCursor" => self.show_hardware_cursor = *value == true,
            "terminal.clearOnShrink" => self.clear_on_shrink = *value == true,
            "compaction.enabled" => self.auto_compact = *value != false,
            "quietStartup" => self.header.splash = self.splash_allowed && *value != true,
            "rlmPaneAutoOpen" => self.rlm_pane_auto = *value != false,
            _ => return false,
        }
        true
    }

    fn apply_theme_setting(&mut self, value: &Value) {
        let Some(setting) = value.as_str() else {
            return;
        };
        let Some(controller) = self.theme_controller.as_mut() else {
            return;
        };
        let applied = controller.set_setting(setting);
        self.adopt_theme(&applied.theme);
        self.sync_scheme_notifications(applied.query_scheme);
        self.query_background(applied.query_background);
        if let Some(error) = applied.error {
            self.show_status(&format!("Error: {error}"));
        }
    }

    fn apply_editor_padding(&mut self, value: &Value) {
        if let Some(padding) = value.as_f64().filter(|p| p.is_finite()) {
            self.editor.set_padding_x(padding.max(0.0).floor() as usize);
        }
    }

    fn apply_autocomplete_max_visible(&mut self, value: &Value) {
        if let Some(visible) = value.as_f64().filter(|v| v.is_finite()) {
            self.editor
                .set_autocomplete_max_visible(visible.clamp(3.0, 20.0).floor() as usize);
        }
    }

    /// The theme setting's controller: the theme it selects applies now (`applyFromSettings`); a confidently detected
    /// appearance is saved as the setting, a theme that fails to load says so.
    pub fn set_theme_controller(&mut self, mut controller: crate::theme_setup::ThemeController) {
        let applied = controller.apply_from_settings();
        self.theme_controller = Some(controller);
        self.tell_terminal_theme();
        self.adopt_theme(&applied.theme);
        self.sync_scheme_notifications(applied.query_scheme);
        self.query_background(applied.query_background);
        if let Some(save) = applied.save {
            self.send(LaneCommand::SaveSetting {
                key: "theme".into(),
                value: Value::String(save),
            });
        }
        if let Some(error) = applied.error {
            self.show_status(&format!("Error: {error}"));
        }
    }

    /// The terminal's appearance, told to the client when it is new (the controller's: OSC 11, a scheme report,
    /// else `COLORFGBG`).
    pub(super) fn tell_terminal_theme(&mut self) {
        let Some(theme) = self
            .theme_controller
            .as_ref()
            .map(crate::theme_setup::ThemeController::terminal_theme)
        else {
            return;
        };
        if self.told_terminal_theme != Some(theme) {
            self.told_terminal_theme = Some(theme);
            self.send(LaneCommand::TerminalTheme(theme.to_string()));
        }
    }

    /// `queryTerminalBackgroundColor`: the OSC 11 query; the reply arrives as input ([`App::handle_input`]).
    pub(super) fn query_background(&mut self, query: bool) {
        if query {
            self.terminal_writes.push(tui::OSC11_BACKGROUND_QUERY.to_string());
        }
    }

    /// An OSC 11 reply: the reported background applies when the controller still wants it (a repaint), and a
    /// detected appearance is saved as the setting.
    pub(super) fn background_reported(&mut self, rgb: Option<(u8, u8, u8)>) {
        let applied = self.theme_controller.as_mut().and_then(|c| c.background_reported(rgb));
        self.tell_terminal_theme();
        let Some(applied) = applied else {
            return;
        };
        self.adopt_theme(&applied.theme);
        if let Some(save) = applied.save {
            self.send(LaneCommand::SaveSetting {
                key: "theme".into(),
                value: Value::String(save),
            });
        }
    }

    /// `setAutoSync` / `queryTerminalColorScheme`: the palette notifications follow the controller's auto sync, and an
    /// applied pair asks for the current scheme; the answer arrives as input ([`App::handle_input`]).
    pub(super) fn sync_scheme_notifications(&mut self, query: bool) {
        let want = self
            .theme_controller
            .as_ref()
            .is_some_and(crate::theme_setup::ThemeController::auto_sync);
        if want != self.scheme_notifications {
            self.scheme_notifications = want;
            self.terminal_writes
                .push(if want { "\x1b[?2031h" } else { "\x1b[?2031l" }.to_string());
        }
        if query {
            self.terminal_writes.push("\x1b[?996n".to_string());
        }
    }

    /// The writes that leave the terminal as it was (`TUI.stop`): the palette notifications off.
    pub fn stop_terminal_notifications(&mut self) {
        if std::mem::take(&mut self.scheme_notifications) {
            self.terminal_writes.push("\x1b[?2031l".to_string());
        }
    }

    /// `theme.name`: the name of the theme drawn now (the controller's, else the startup theme's).
    pub(super) fn active_theme_name(&self) -> Option<String> {
        match &self.theme_controller {
            Some(controller) => controller.active_theme_name(),
            None => self.theme.name.clone(),
        }
    }

    /// Draw with `theme` from now on (`refreshTheme`): every view shares the app's theme; the highlighters and the
    /// transcript's rendered rows are rebuilt and the terminal repainted.
    pub(super) fn adopt_theme(&mut self, theme: &Theme) {
        self.theme.adopt(theme);
        let mut style = self.chat.style();
        if self.qol.highlight {
            style.code_highlight = Some(code_highlighter(&self.theme));
        }
        self.chat.set_style(style);
        self.update_editor_border();
        self.reset_display = true;
        self.committed.clear();
    }

    /// Whether Ctrl+Z asked to suspend; clears the request.
    pub fn take_suspend_request(&mut self) -> bool {
        std::mem::take(&mut self.suspend_request)
    }

    /// How Ctrl+Z suspends the process (the product stops it with SIGTSTP; returns once it continues). Without one,
    /// Ctrl+Z does nothing.
    pub fn set_suspend_hook(&mut self, hook: SuspendHook) {
        self.suspend_hook = Some(hook);
    }

    /// Run the suspend hook (the terminal is already given back); a failure shows in the status line.
    pub fn suspend(&mut self) -> bool {
        let Some(hook) = self.suspend_hook.as_mut() else {
            return false;
        };
        if let Err(error) = hook() {
            self.show_status(&error);
        }
        true
    }

    pub fn can_suspend(&self) -> bool {
        self.suspend_hook.is_some()
    }

    /// The context use the footer shows: `(tokens, percent)`, both unknown right after a compaction
    /// (`laneContextUsage`); `None` without a context window.
    pub(super) fn context(&self) -> Option<(Option<u64>, Option<f64>)> {
        let window = self.model.context_window;
        let after = &self.transcript[self.context_from.min(self.transcript.len())..];
        let measured = after.iter().any(|m| {
            m["role"] == "assistant"
                && m["stopReason"] != "aborted"
                && m["stopReason"] != "error"
                && context_tokens(&m["usage"]) > 0
        });
        if window > 0 && self.compacted && !measured {
            return Some((None, None));
        }
        context_usage(after, window).map(|(tokens, percent)| (Some(tokens), Some(percent)))
    }

    pub fn qol(&self) -> &QolSettings {
        &self.qol
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Opens hyperlinks: an OSC 8 link clicked in the fullscreen transcript (`openUrl`) and a link chosen in the copy
    /// picker. Without one, clicks select as usual and nothing opens.
    pub fn set_url_opener(&mut self, opener: tui::UrlOpener) {
        self.viewport.open_url = Some(opener);
    }

    /// The clipboard for image paste and Vim yanks.
    pub fn set_clipboard(&mut self, clipboard: ClipboardService) {
        if let Some(vim) = self.vim.as_mut() {
            vim.on_yank = Some(clipboard.yank_sink());
        }
        self.clipboard = Some(clipboard);
    }

    /// The status line's git fields.
    pub fn set_git_source(&mut self, source: Box<dyn GitSource>) {
        self.git = Some(source);
    }

    /// Where the footer's git branch comes from (started with the services).
    pub fn set_footer_branch(&mut self, branch: crate::footer_data::FooterBranch) {
        self.footer_branch = Some(branch);
    }

    /// Where turn-end notifications go (with `qol.notifications`).
    pub fn set_notify_sink(&mut self, sink: NotifyFn) {
        self.notify = Some(sink);
    }

    /// A queue service threads push terminal bytes into; the paint loop writes them.
    pub fn side_writes(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.side_writes)
    }

    /// Images staged for the draft.
    pub fn attachments(&self) -> &Attachments {
        &self.attachments
    }

    /// The Vim mode, when Vim mode is on.
    pub fn vim_mode(&self) -> Option<VimMode> {
        self.vim.as_ref().map(VimEditor::mode)
    }

    pub fn editor_text(&self) -> String {
        self.editor.text()
    }

    /// Apply what background services produced (clipboard reads, git changes, notification bytes); true when the
    /// screen may have changed. Never blocks.
    pub fn poll_services(&mut self) -> bool {
        let mut changed = self.poll_clipboard();
        changed |= self.chat.poll_images();
        changed |= self.poll_git_sources();
        changed |= self.drain_side_writes();
        changed
    }

    fn poll_clipboard(&mut self) -> bool {
        let replies: Vec<ClipboardReply> = self
            .clipboard
            .as_ref()
            .map_or_default(|c| std::iter::from_fn(|| c.try_reply()).collect());
        let changed = !replies.is_empty();
        for reply in replies {
            self.handle_clipboard_reply(reply);
            self.update_editor_border();
        }
        changed
    }

    fn handle_clipboard_reply(&mut self, reply: ClipboardReply) {
        match reply {
            ClipboardReply::Copied(result) => self.handle_clipboard_copied(result),
            reply if self.pending_reads.pop_front() == Some(true) => self.handle_pending_clipboard_read(reply),
            ClipboardReply::Image { data, mime_type } => self.handle_clipboard_image(data, mime_type),
            ClipboardReply::Text(text) => self.handle_clipboard_text(text),
            ClipboardReply::Error(e) => {
                if self.qol.attachments {
                    self.show_status(&format!("Error: {e}"));
                }
            }
        }
    }

    fn handle_clipboard_copied(&mut self, result: Result<Option<String>, String>) {
        match (self.pending_copies.pop_front().flatten(), result) {
            (Some(done), Ok(osc52)) => {
                self.terminal_writes.extend(osc52);
                self.show_status(&done);
            }
            (Some(_), Err(error)) => self.show_status(&format!("Error: {error}")),
            (None, Ok(osc52)) => {
                self.terminal_writes.extend(osc52);
                self.viewport.copied(Ok(()), self.now());
            }
            (None, Err(error)) => self.viewport.copied(Err(error), self.now()),
        }
    }

    fn handle_pending_clipboard_read(&mut self, reply: ClipboardReply) {
        if let ClipboardReply::Text(text) = reply
            && !text.is_empty()
            && self.overlay.is_none()
        {
            self.editor.handle_input(&format!("{PASTE_START}{text}{PASTE_END}"));
        }
    }

    fn handle_clipboard_image(&mut self, data: String, mime_type: String) {
        if self.qol.attachments {
            let placeholder = self.attachments.add_image(data, &mime_type, None);
            self.editor.insert_text_at_cursor(&format!("{placeholder} "));
        } else if let Some(path) = self.save_clipboard_image(&data, &mime_type) {
            self.editor.insert_text_at_cursor(&path);
        }
    }

    fn handle_clipboard_text(&mut self, text: String) {
        if self.qol.attachments {
            self.editor.handle_input(&format!("{PASTE_START}{text}{PASTE_END}"));
        } else if !text.is_empty() {
            self.editor.insert_text_at_cursor(&text);
        }
    }

    fn poll_git_sources(&mut self) -> bool {
        let mut changed = false;
        if let Some(branch) = self
            .footer_branch
            .as_ref()
            .map(crate::footer_data::FooterBranch::branch)
            && branch != self.footer_git_branch
        {
            self.footer_git_branch = branch;
            changed = true;
        }
        if let Some(git) = &self.git {
            let snapshot = git.snapshot();
            if snapshot != self.git_snapshot {
                self.git_snapshot = snapshot;
                changed = true;
            }
        }
        changed
    }

    fn drain_side_writes(&mut self) -> bool {
        let writes = std::mem::take(
            &mut *self
                .side_writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if writes.is_empty() {
            return false;
        }
        self.terminal_writes.extend(writes);
        true
    }

    /// A clipboard image (base64) saved as `<tmp>/ultron-clipboard-<id>.<ext>`, the extension by its MIME type
    /// (`extensionForImageMimeType`, else `png`); its path.
    pub(super) fn save_clipboard_image(&self, data: &str, mime_type: &str) -> Option<String> {
        let bytes = jsrt::base64::atob(data)?;
        let extension = tui::extension_for_image_mime_type(mime_type).unwrap_or("png");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = self
            .temp_dir
            .join(format!("ultron-clipboard-{}-{nanos:x}.{extension}", std::process::id()));
        std::fs::write(&path, bytes).ok()?;
        Some(path.display().to_string())
    }

    /// Copy `text` to the clipboard (`copyToClipboard`): native writers and clipboard tools before OSC 52, through
    /// the clipboard service when there is one (`done` shows once it worked); without one, OSC 52.
    pub(super) fn copy_to_clipboard(&mut self, text: &str, done: &str) {
        match &self.clipboard {
            Some(clipboard) => {
                self.pending_copies.push_back(Some(done.to_string()));
                clipboard.copy_checked(text);
            }
            None => {
                self.terminal_writes
                    .push(format!("\x1b]52;c;{}\x07", jsrt::base64::encode(text)));
                self.show_status(done);
            }
        }
    }

    /// Copy a finished fullscreen selection (`copySelection` of tui-renderer.ts: `copyToClipboard`, then a flash):
    /// through the clipboard service (its tools come from the injected PATH), else OSC 52.
    pub(super) fn copy_selection(&mut self, text: &str) {
        match &self.clipboard {
            Some(clipboard) => {
                self.pending_copies.push_back(None);
                clipboard.copy_checked(text);
            }
            None => {
                self.terminal_writes
                    .push(format!("\x1b]52;c;{}\x07", jsrt::base64::encode(text)));
                self.viewport.copied(Ok(()), self.now());
            }
        }
    }

    /// The mouse tracking the alternate screen turns on ([`tui::mouse_tracking`] of the terminal's
    /// environment); the product sets it, frames painted in tests leave the mouse alone.
    pub fn set_mouse_tracking(&mut self, mouse: Option<&'static str>) {
        self.mouse_tracking = mouse;
    }

    pub fn mouse_tracking(&self) -> Option<&'static str> {
        self.mouse_tracking
    }

    /// The terminal environment's debug outputs and Termux flag ([`crate::run::terminal_debug`]).
    pub fn set_terminal_debug(&mut self, debug: Option<std::sync::Arc<tui::RenderDebug>>, termux: bool) {
        self.render_debug = debug;
        self.termux = termux;
    }

    pub fn render_debug(&self) -> Option<std::sync::Arc<tui::RenderDebug>> {
        self.render_debug.clone()
    }

    pub fn termux(&self) -> bool {
        self.termux
    }

    /// Right-click paste in fullscreen ([`tui::right_click_paste_enabled`] of the terminal's
    /// platform and environment; off unless the product turns it on).
    pub fn set_right_click_paste(&mut self, enabled: bool) {
        self.viewport.right_click_paste = enabled;
    }

    pub fn viewport(&self) -> &tui::FullscreenViewport {
        &self.viewport
    }

    /// The alternate screen was entered again: the viewport forgets its gestures and layout.
    pub fn reset_viewport(&mut self) {
        self.viewport.reset();
    }

    /// The fullscreen viewport's timers (scrollbar hide, flashes, a held drag's auto-scroll) due by `now`; true
    /// when the frame changed.
    pub fn poll_viewport(&mut self, now: Instant) -> bool {
        self.viewport.poll(now)
    }

    /// When the fullscreen viewport next has a timer due.
    pub fn viewport_deadline(&self) -> Option<Instant> {
        self.viewport.deadline()
    }

    /// `handleViewportInput` in fullscreen: focus reports are the viewport's; while a full-frame overlay shows it
    /// gets the keys and the mouse (`shouldDeferViewportInputToOverlay`; the search and the pickers read SGR
    /// reports, the others ignore them); otherwise the viewport takes the mouse and its scroll keys. True when the
    /// input was taken.
    pub(super) fn fullscreen_input(&mut self, data: &str) -> bool {
        if self.qol.viewport_split {
            return false;
        }
        if self.overlay.is_some() && !tui::is_focus_report(data) {
            return false;
        }
        let now = self.now();
        let mut mouse = AppMouse {
            layout: &self.mouse_layout,
            theme: &self.theme,
            now,
            rows: self.rows,
            top: self.viewport.viewport_top(),
            editor: &mut self.editor,
            chat: &mut self.chat,
            selection: &mut self.selection,
            panel: &mut self.panel,
            select_events: Vec::new(),
            settings_events: Vec::new(),
        };
        let input = self.viewport.handle_input_with(data, &self.keys, now, &mut mouse);
        let (select_events, settings_events) = (mouse.select_events, mouse.settings_events);
        for event in select_events {
            self.select_event(Some(event));
        }
        for event in settings_events {
            self.settings_event(Some(event));
        }
        match input {
            tui::ViewportInput::Pass => false,
            tui::ViewportInput::Consumed => true,
            tui::ViewportInput::Copy(text) => {
                self.copy_selection(&text);
                true
            }
            tui::ViewportInput::RightClickPaste => {
                if let Some(clipboard) = &self.clipboard {
                    self.pending_reads.push_back(true);
                    clipboard.request(ClipboardRead::Text);
                }
                true
            }
        }
    }

    /// A paste whose every segment names a local image or video becomes attachments (`[Image #N, WxH]`).
    pub(super) fn paste_attachments(&mut self, data: &str) -> bool {
        let Some(content) = data.strip_prefix(PASTE_START).and_then(|d| d.strip_suffix(PASTE_END)) else {
            return false;
        };
        let PasteAction::Attach(paths) = classify_paste(content) else {
            return false;
        };
        let mut placeholders = Vec::new();
        for path in &paths {
            match self.attachments.add_image_file(path) {
                Ok(placeholder) => placeholders.push(placeholder),
                Err(_) => return false,
            }
        }
        self.editor
            .insert_text_at_cursor(&format!("{} ", placeholders.join(" ")));
        true
    }

    /// The staged images `prompt` still references; the registry is emptied (the draft was sent).
    pub(super) fn take_images(&mut self, prompt: &str) -> Vec<PromptImage> {
        let images = self
            .attachments
            .referenced(prompt)
            .into_iter()
            .map(|(_, image)| PromptImage {
                data: image.data_b64.clone(),
                mime_type: image.mime.clone(),
            })
            .collect();
        self.attachments.clear();
        images
    }

    pub(super) fn status_data(&self) -> StatusData {
        let footer = self.footer_view();
        let context = self.context();
        let stream = if self.chat.tool_running() {
            StreamState::Running
        } else if self.chat.is_working() {
            StreamState::Streaming
        } else {
            StreamState::Idle
        };
        let vim = self.vim.as_ref().map(|v| {
            let indicator = v.indicator(&self.editor);
            VimStatus {
                mode: indicator.mode,
                pending: indicator.pending,
                selected_lines: indicator.selected_lines,
                display: VimDisplay::Text,
            }
        });
        StatusData {
            model_name: Some(self.model.name.clone()),
            thinking_level: self.model.reasoning.then(|| self.thinking_level.clone()),
            cwd: self.cwd.clone(),
            home: self.home.clone().unwrap_or_default(),
            git_branch: self.git_snapshot.branch.clone(),
            git_status: self.git_snapshot.status,
            pr: self.git_snapshot.pr.clone(),
            context_percent: context.and_then(|(_, percent)| percent),
            context_tokens: context.and_then(|(tokens, _)| tokens).unwrap_or(0),
            context_window: self.model.context_window,
            auto_compact: self.auto_compact,
            usage: Usage {
                input: footer.input,
                output: footer.output,
                cache_read: footer.cache_read,
                cache_write: footer.cache_write,
                cost: footer.cost,
                ..Usage::default()
            },
            active_ms: self.active_ms,
            turn_elapsed_ms: self
                .turn_started
                .map(|t| self.now().duration_since(t).as_millis() as u64),
            now_ms: self.ticks * 80,
            session_id: self.session_id.clone(),
            session_name: self.session_name.clone(),
            stream,
            vim,
            ..StatusData::default()
        }
    }

    /// The status line row (with `qol.status_line`).
    pub fn status_line(&self, width: usize) -> Option<String> {
        if !self.qol.status_line {
            return None;
        }
        let line = render_status_line(
            &self.qol.status_layout.effective(),
            &self.status_data(),
            &self.qol.status_symbols.symbols(),
            &ThemeStyle(&self.theme),
            width,
        );
        (!line.is_empty()).then(|| truncate_to_width(&line, width, "", false))
    }

    /// Rows of `settled` not yet in scrollback: appended when the committed rows are still its head, else the whole
    /// of it replayed (a toggle, a resize or a session switch changed rows already written).
    pub(super) fn commit(committed: &mut Vec<String>, history_id: &mut u64, settled: &[&str]) -> Option<HistoryBatch> {
        let head = committed.len() <= settled.len() && committed.iter().zip(settled).all(|(row, now)| row == now);
        let (rows, kind) = if head {
            if settled.len() == committed.len() {
                return None;
            }
            let rows: Vec<String> = settled[committed.len()..]
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            committed.extend(rows.iter().cloned());
            (rows, HistoryKind::Append)
        } else {
            let rows: Vec<String> = settled.iter().map(std::string::ToString::to_string).collect();
            committed.clone_from(&rows);
            (rows, HistoryKind::Replay)
        };
        *history_id += 1;
        Some(HistoryBatch {
            id: *history_id,
            rows,
            kind,
        })
    }

    /// One frame of the scrollback/live split: finished transcript rows go to history once; the viewport is the
    /// live transcript (a streaming message, running cells) above the dock. The painter keeps the viewport's last
    /// `height` rows, so the editor and footer stay on screen. The batch it returns must be painted. The transcript's
    /// rows are borrowed from the chat's cache: only new and live rows are copied, not the whole session's each frame.
    pub fn frame_plan(&mut self, width: usize, height: usize) -> FramePlan {
        self.rows = height;
        self.cols = width;
        if let Some(frame) = self.overlay_frame(width, height) {
            return FramePlan {
                history: None,
                viewport: frame,
            };
        }
        let header = self.header_lines(width);
        self.chat.render_items(width);
        let document: Vec<&str> = header.iter().chain(self.chat.lines()).map(String::as_str).collect();
        let settled = if self.session_id.is_none() {
            0
        } else if self.operation.is_none() && !self.chat.is_working() {
            document.len()
        } else {
            header.len() + self.chat.settled_lines()
        };
        let history = Self::commit(&mut self.committed, &mut self.history_id, &document[..settled]);
        let mut viewport: Vec<String> = document[settled..]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        for part in self.dock(width) {
            viewport.extend(part);
        }
        FramePlan { history, viewport }
    }

    /// The last frame of the split: the whole transcript in scrollback, the dock below it.
    pub fn exit_plan(&mut self, width: usize) -> FramePlan {
        let document = self.document(width);
        let rows: Vec<&str> = document.iter().map(String::as_str).collect();
        let history = Self::commit(&mut self.committed, &mut self.history_id, &rows);
        FramePlan {
            history,
            viewport: self.dock(width).concat(),
        }
    }
}
