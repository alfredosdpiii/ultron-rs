//! Multi-line prompt editor with its autocomplete picker (port of packages/tui/src/components/editor.ts).
//! Cursor columns are byte offsets into a logical line. Where TS arithmetic is in UTF-16 code units and the
//! result is observable (sticky visual columns, paste sizes), the port converts to code units explicitly.

mod completion;
mod input;
mod motion;
mod render;
mod wrap;

pub use completion::AUTOCOMPLETE_DEBOUNCE_MS;
pub use wrap::word_wrap_line;

use std::rc::Rc;

use input::{decode_csi_u_ctrl, normalize_text};
use wrap::{find_markers, graphemes_with_markers, is_ws, marker_at};

use crate::autocomplete::AutocompleteProvider;
use crate::editing::KillRing;
use crate::keybindings::{KeybindingsManager, UserBindings};
use crate::keys::{KeyContext, matches_key};
use crate::select::SelectListTheme;
use jsrt::{string, utf16};

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    pastes: Vec<(u64, String)>,
    paste_counter: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Receives the editor text after a change.
pub type ChangeFn = Box<dyn FnMut(&str)>;

/// Styles the top and bottom borders.
pub type StyleFn = Box<dyn Fn(&str) -> String>;

pub struct Editor {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    pub focused: bool,
    /// Stands in for `tui.terminal.rows`: the visible area is 30% of it, at least 5 lines.
    pub terminal_rows: usize,
    pub border_color: StyleFn,
    pub disable_submit: bool,
    padding_x: usize,
    last_width: usize,
    scroll_offset: usize,
    /// Paste registry in `Map` insertion order.
    pastes: Vec<(u64, String)>,
    paste_counter: u64,
    paste_buffer: String,
    in_paste: bool,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<(Vec<String>, usize, usize)>,
    history_limit: usize,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    jump_forward: Option<bool>,
    preferred_visual_col: Option<usize>,
    /// Pre-snap cursor column in UTF-16 units.
    snapped_from: Option<usize>,
    undo_stack: Vec<Snapshot>,
    keys: KeybindingsManager,
    /// TS `onChange`: called with the full text after every edit.
    pub on_change: Option<ChangeFn>,
    select_list_theme: Rc<SelectListTheme>,
    provider: Option<Box<dyn AutocompleteProvider>>,
    trigger_chars: Vec<char>,
    picker: Option<completion::Picker>,
    /// A debounced request: (force, explicit Tab).
    pending_request: Option<(bool, bool)>,
    /// The request whose answer is being computed off the UI thread.
    running: Option<completion::Running>,
    worker: completion::Worker,
    autocomplete_max_visible: usize,
    rendered_visible_lines: usize,
    rendered_picker_height: usize,
}

impl Editor {
    /// `keys` stands in for the TS global `getKeybindings()` and the global key-matching state.
    pub fn new(keys: KeybindingsManager, border_color: StyleFn, padding_x: usize) -> Self {
        Editor {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            focused: false,
            terminal_rows: 24,
            border_color,
            disable_submit: false,
            padding_x,
            last_width: 80,
            scroll_offset: 0,
            pastes: Vec::new(),
            paste_counter: 0,
            paste_buffer: String::new(),
            in_paste: false,
            history: Vec::new(),
            history_index: None,
            history_draft: None,
            history_limit: 100,
            kill_ring: KillRing::default(),
            last_action: None,
            jump_forward: None,
            preferred_visual_col: None,
            snapped_from: None,
            undo_stack: Vec::new(),
            keys,
            on_change: None,
            select_list_theme: Rc::new(SelectListTheme {
                selected_text: Box::new(str::to_string),
                description: Box::new(str::to_string),
                scroll_info: Box::new(str::to_string),
                no_match: Box::new(str::to_string),
            }),
            provider: None,
            trigger_chars: completion::DEFAULT_TRIGGER_CHARACTERS.to_vec(),
            picker: None,
            pending_request: None,
            running: None,
            worker: completion::Worker::default(),
            autocomplete_max_visible: 5,
            rendered_visible_lines: 0,
            rendered_picker_height: 0,
        }
    }

    /// The key-matching state the editor matches with.
    pub fn key_context(&self) -> KeyContext {
        self.keys.ctx()
    }

    /// New user bindings (`<agentDir>/keybindings.json` read again on `/reload`).
    pub fn set_user_keybindings(&mut self, user: UserBindings) {
        self.keys.set_user_bindings(user);
    }

    /// TS `EditorTheme.selectList`.
    pub fn set_select_list_theme(&mut self, theme: SelectListTheme) {
        self.select_list_theme = Rc::new(theme);
    }

    fn changed(&mut self) {
        let text = self.text();
        if let Some(cb) = &mut self.on_change {
            cb(&text);
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// (line, byte column).
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_line, self.cursor_col)
    }

    pub fn expanded_text(&self) -> String {
        self.expand_paste_markers(&self.text())
    }

    fn valid_ids(&self) -> Vec<u64> {
        self.pastes.iter().map(|(id, _)| *id).collect()
    }

    fn graphemes<'a>(&self, text: &'a str) -> Vec<(usize, &'a str)> {
        graphemes_with_markers(text, &self.valid_ids())
    }

    fn line(&self) -> &str {
        &self.lines[self.cursor_line]
    }

    fn max_visible_lines(&self) -> usize {
        ((self.terminal_rows as f64 * 0.3).floor() as usize).max(5)
    }

    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim_matches(string::is_space);
        if trimmed.is_empty() || self.history.first().is_some_and(|h| h == trimmed) {
            return;
        }
        self.history.insert(0, trimmed.to_string());
        self.history.truncate(self.history_limit);
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn set_history(&mut self, entries: &[&str]) {
        self.history = entries
            .iter()
            .map(|e| e.trim_matches(string::is_space).to_string())
            .filter(|e| !e.is_empty())
            .take(self.history_limit)
            .collect();
        self.exit_history_browsing();
    }

    fn is_editor_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let new_index = self.history_index.map_or(-1, |i| i as isize) - direction;
        if new_index < -1 || new_index >= self.history.len() as isize {
            return;
        }
        if self.history_index.is_none() && new_index >= 0 {
            self.push_undo();
            self.history_draft = Some((self.lines.clone(), self.cursor_line, self.cursor_col));
        }
        if new_index == -1 {
            self.history_index = None;
            match self.history_draft.take() {
                Some((lines, line, col)) => {
                    self.lines = lines;
                    self.cursor_line = line;
                    self.cursor_col = col;
                    self.preferred_visual_col = None;
                    self.snapped_from = None;
                    self.scroll_offset = 0;
                    self.changed();
                }
                None => self.set_text_internal("", false),
            }
        } else {
            let i = new_index as usize;
            self.history_index = Some(i);
            let text = self.history[i].clone();
            self.set_text_internal(&text, direction == -1);
        }
    }

    fn exit_history_browsing(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }

    fn set_text_internal(&mut self, text: &str, at_start: bool) {
        self.lines = text.split('\n').map(str::to_string).collect();
        self.cursor_line = if at_start { 0 } else { self.lines.len() - 1 };
        let col = if at_start {
            0
        } else {
            self.lines[self.cursor_line].len()
        };
        self.set_cursor_col(col);
        self.scroll_offset = 0;
        self.changed();
    }

    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history_browsing();
        let normalized = normalize_text(text);
        if self.text() != normalized {
            self.push_undo();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, false);
    }

    /// Place the cursor: line clamped to the buffer, column to the line and back to a char boundary.
    pub fn set_cursor(&mut self, line: usize, col: usize) {
        self.cursor_line = line.min(self.lines.len() - 1);
        let text = &self.lines[self.cursor_line];
        let mut col = col.min(text.len());
        while !text.is_char_boundary(col) {
            col -= 1;
        }
        self.set_cursor_col(col);
    }

    /// Replace the buffer and cursor as one edit (`undo`: record an undo step first).
    pub fn apply_edit(&mut self, lines: Vec<String>, cursor: (usize, usize), undo: bool) {
        self.cancel_autocomplete();
        if undo {
            self.push_undo();
        }
        self.last_action = None;
        self.exit_history_browsing();
        self.lines = if lines.is_empty() { vec![String::new()] } else { lines };
        self.set_cursor(cursor.0, cursor.1);
        self.changed();
    }

    /// Undo the last recorded edit.
    pub fn undo_edit(&mut self) {
        self.undo();
    }

    /// Byte spans of the valid paste markers on `line`.
    pub fn paste_marker_spans_on(&self, line: usize) -> Vec<(usize, usize)> {
        let ids = self.valid_ids();
        self.lines.get(line).map_or_else(Vec::new, |l| {
            find_markers(l)
                .into_iter()
                .filter(|m| m.id.parse::<u64>().is_ok_and(|id| ids.contains(&id)))
                .map(|m| (m.start, m.end))
                .collect()
        })
    }

    /// Registered large pastes as (id, content), in insertion order.
    pub fn pastes(&self) -> &[(u64, String)] {
        &self.pastes
    }

    /// `text` with paste markers expanded to their content.
    pub fn expand_markers(&self, text: &str) -> String {
        self.expand_paste_markers(text)
    }

    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo();
        self.last_action = None;
        self.exit_history_browsing();
        self.insert_text_internal(text);
    }

    fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (id, content) in &self.pastes {
            let id = id.to_string();
            let mut out = String::new();
            let mut last = 0;
            for m in find_markers(&result).into_iter().filter(|m| m.id == id) {
                out.push_str(&result[last..m.start]);
                out.push_str(content);
                last = m.end;
            }
            out.push_str(&result[last..]);
            result = out;
        }
        result
    }

    fn insert_text_internal(&mut self, text: &str) {
        self.insert_raw(&normalize_text(text));
        self.changed();
    }

    fn insert_character(&mut self, ch: &str) {
        self.exit_history_browsing();
        if is_ws(ch) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        let col = self.cursor_col;
        self.lines[self.cursor_line].insert_str(col, ch);
        self.set_cursor_col(col + ch.len());
        self.changed();
        self.after_insert_character(ch);
    }

    fn handle_paste(&mut self, pasted: &str) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo();
        let decoded = decode_csi_u_ctrl(pasted);
        let mut filtered: String = normalize_text(&decoded)
            .chars()
            .filter(|&c| c == '\n' || c as u32 >= 32)
            .collect();
        if filtered.starts_with(['/', '~', '.']) {
            let before = self.line()[..self.cursor_col].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                filtered.insert(0, ' ');
            }
        }
        let line_count = filtered.split('\n').count();
        let total = utf16::len(&filtered);
        if line_count > 10 || total > 1000 {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.set_paste(id, filtered);
            let marker = if line_count > 10 {
                format!("[paste #{id} +{line_count} lines]")
            } else {
                format!("[paste #{id} {total} chars]")
            };
            self.insert_text_internal(&marker);
            return;
        }
        self.insert_text_internal(&filtered);
    }

    /// `Map.set`: replace in place or append.
    fn set_paste(&mut self, id: u64, content: String) {
        match self.pastes.iter_mut().find(|(k, _)| *k == id) {
            Some(entry) => entry.1 = content,
            None => self.pastes.push((id, content)),
        }
    }

    fn add_new_line(&mut self) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo();
        let after = self.lines[self.cursor_line].split_off(self.cursor_col);
        self.lines.insert(self.cursor_line + 1, after);
        self.cursor_line += 1;
        self.set_cursor_col(0);
        self.changed();
    }

    fn should_submit_on_backslash_enter(&self, data: &str) -> bool {
        if self.disable_submit || !matches_key(data, "enter", self.keys.ctx()) {
            return false;
        }
        let submit = self.keys.get_keys("tui.input.submit");
        if !submit.iter().any(|k| k == "shift+enter" || k == "shift+return") {
            return false;
        }
        self.cursor_col > 0 && self.line().as_bytes()[self.cursor_col - 1] == b'\\'
    }

    fn submit_value(&mut self) -> String {
        self.cancel_autocomplete();
        let result = self
            .expand_paste_markers(&self.text())
            .trim_matches(string::is_space)
            .to_string();
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        self.changed();
        result
    }

    fn handle_backspace(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        if self.cursor_col > 0 {
            self.push_undo();
            let line = self.line().to_string();
            let last = self
                .graphemes(&line[..self.cursor_col])
                .last()
                .map_or("", |g| g.1)
                .to_string();
            if let Some(target) = marker_at(&last, 0)
                .filter(|m| m.end == last.len())
                .and_then(|m| m.id.parse::<u64>().ok())
            {
                self.pastes.retain(|(id, _)| *id != target);
                self.paste_counter = self.paste_counter.saturating_sub(1);
                let mut higher: Vec<u64> = self
                    .pastes
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| *id > target)
                    .collect();
                higher.sort_unstable();
                for id in higher {
                    if let Some(pos) = self.pastes.iter().position(|(k, _)| *k == id) {
                        let (_, content) = self.pastes.remove(pos);
                        self.set_paste(id - 1, content);
                    }
                }
                for l in &mut self.lines {
                    *l = renumber_markers(l, target);
                }
            }
            let col = self.cursor_col;
            self.lines[self.cursor_line].replace_range(col - last.len()..col, "");
            self.set_cursor_col(col - last.len());
        } else if self.cursor_line > 0 {
            self.push_undo();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            let prev_len = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.set_cursor_col(prev_len);
        }
        self.changed();
        self.after_delete();
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from = None;
    }

    fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    fn move_to_line_end(&mut self) {
        self.last_action = None;
        self.set_cursor_col(self.line().len());
    }

    fn kill(&mut self, text: &str, prepend: bool, accumulate: bool) {
        self.kill_ring.push(text, prepend, accumulate);
        self.last_action = Some(LastAction::Kill);
    }

    fn was_kill(&self) -> bool {
        self.last_action == Some(LastAction::Kill)
    }

    fn delete_to_line_start(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col > 0 {
            self.push_undo();
            let deleted: String = self.lines[self.cursor_line].drain(..self.cursor_col).collect();
            self.kill(&deleted, true, self.was_kill());
            self.set_cursor_col(0);
        } else if self.cursor_line > 0 {
            self.push_undo();
            self.kill("\n", true, self.was_kill());
            self.join_with_previous();
        }
        self.changed();
    }

    fn delete_to_line_end(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col < self.line().len() {
            self.push_undo();
            let deleted = self.lines[self.cursor_line].split_off(self.cursor_col);
            self.kill(&deleted, false, self.was_kill());
        } else if self.cursor_line + 1 < self.lines.len() {
            self.push_undo();
            self.kill("\n", false, self.was_kill());
            self.join_with_next();
        }
        self.changed();
    }

    /// Merge the cursor line into the previous one; the cursor goes to the join.
    fn join_with_previous(&mut self) {
        let current = self.lines.remove(self.cursor_line);
        self.cursor_line -= 1;
        let prev_len = self.line().len();
        self.lines[self.cursor_line].push_str(&current);
        self.set_cursor_col(prev_len);
    }

    fn join_with_next(&mut self) {
        let next = self.lines.remove(self.cursor_line + 1);
        self.lines[self.cursor_line].push_str(&next);
    }

    fn delete_word_backward(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.push_undo();
                self.kill("\n", true, self.was_kill());
                self.join_with_previous();
            }
            self.changed();
            return;
        }
        self.push_undo();
        let was_kill = self.was_kill();
        let from = self.word_backward_target();
        let deleted: String = self.lines[self.cursor_line].drain(from..self.cursor_col).collect();
        self.kill(&deleted, true, was_kill);
        self.set_cursor_col(from);
        self.changed();
    }

    fn delete_word_forward(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col >= self.line().len() {
            if self.cursor_line + 1 < self.lines.len() {
                self.push_undo();
                self.kill("\n", false, self.was_kill());
                self.join_with_next();
            }
            self.changed();
            return;
        }
        self.push_undo();
        let was_kill = self.was_kill();
        let to = self.word_forward_target();
        let deleted: String = self.lines[self.cursor_line].drain(self.cursor_col..to).collect();
        self.kill(&deleted, false, was_kill);
        self.set_cursor_col(self.cursor_col);
        self.changed();
    }

    fn handle_forward_delete(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        let col = self.cursor_col;
        if col < self.line().len() {
            self.push_undo();
            let len = self.graphemes(&self.line()[col..]).first().map_or(1, |g| g.1.len());
            self.lines[self.cursor_line].replace_range(col..col + len, "");
        } else if self.cursor_line + 1 < self.lines.len() {
            self.push_undo();
            self.join_with_next();
        }
        self.changed();
        self.after_delete();
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        self.push_undo();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default().to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn insert_yanked_text(&mut self, text: &str) {
        self.exit_history_browsing();
        self.insert_raw(text);
        self.changed();
    }

    /// Insert `text` at the cursor as is; the cursor ends after it.
    fn insert_raw(&mut self, text: &str) {
        let parts: Vec<&str> = text.split('\n').collect();
        let after = self.lines[self.cursor_line].split_off(self.cursor_col);
        let last = parts[parts.len() - 1];
        self.lines[self.cursor_line].push_str(parts[0]);
        if parts.len() == 1 {
            self.lines[self.cursor_line].push_str(&after);
            self.set_cursor_col(self.cursor_col + text.len());
            return;
        }
        let mut tail: Vec<String> = parts[1..parts.len() - 1]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        tail.push(format!("{last}{after}"));
        let at = self.cursor_line + 1;
        self.lines.splice(at..at, tail);
        self.cursor_line += parts.len() - 1;
        self.set_cursor_col(last.len());
    }

    fn delete_yanked_text(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        let parts: Vec<&str> = yanked.split('\n').collect();
        if parts.len() == 1 {
            let col = self.cursor_col;
            self.lines[self.cursor_line].replace_range(col - yanked.len()..col, "");
            self.set_cursor_col(col - yanked.len());
        } else {
            let start_line = self.cursor_line - (parts.len() - 1);
            let start_col = self.lines[start_line].len() - parts[0].len();
            let after = self.line()[self.cursor_col..].to_string();
            let merged = format!("{}{after}", &self.lines[start_line][..start_col]);
            self.lines.splice(start_line..start_line + parts.len(), [merged]);
            self.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
        self.changed();
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    fn undo(&mut self) {
        self.exit_history_browsing();
        let Some(s) = self.undo_stack.pop() else { return };
        self.lines = s.lines;
        self.cursor_line = s.cursor_line;
        self.cursor_col = s.cursor_col;
        self.pastes = s.pastes;
        self.paste_counter = s.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        self.changed();
    }
}

/// Decrement the ids of markers above `removed` (every marker, valid or not, as the TS regex does).
fn renumber_markers(line: &str, removed: u64) -> String {
    let mut out = String::new();
    let mut last = 0;
    for m in find_markers(line) {
        let Ok(x) = m.id.parse::<u64>() else { continue };
        if x > removed {
            out.push_str(&line[last..m.start]);
            out.push_str(&format!("[paste #{}{}]", x - 1, m.suffix));
            last = m.end;
        }
    }
    out.push_str(&line[last..]);
    out
}
