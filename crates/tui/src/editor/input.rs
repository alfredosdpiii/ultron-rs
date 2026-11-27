use super::Editor;
use super::wrap::digits_at;
use crate::attachments::{BracketedChunk, buffer_bracketed_paste};
use crate::keys::{decode_printable_key, matches_key};

type EditorAction = fn(&mut Editor);

const PRE_DELETE_ACTIONS: &[(&str, EditorAction)] = &[
    ("tui.editor.deleteToLineEnd", Editor::delete_to_line_end),
    ("tui.editor.deleteToLineStart", Editor::delete_to_line_start),
    ("tui.editor.deleteWordBackward", Editor::delete_word_backward),
    ("tui.editor.deleteWordForward", Editor::delete_word_forward),
];

const POST_DELETE_ACTIONS: &[(&str, EditorAction)] = &[
    ("tui.editor.yank", Editor::yank),
    ("tui.editor.yankPop", Editor::yank_pop),
    ("tui.editor.historyPrevious", Editor::history_prev_action),
    ("tui.editor.historyNext", Editor::history_next_action),
    ("tui.editor.cursorLineStart", Editor::move_to_line_start),
    ("tui.editor.cursorLineEnd", Editor::move_to_line_end),
    ("tui.editor.cursorWordLeft", Editor::move_word_backward),
    ("tui.editor.cursorWordRight", Editor::move_word_forward),
];

const POST_NAV_ACTIONS: &[(&str, EditorAction)] = &[
    ("tui.editor.cursorRight", Editor::move_cursor_right_action),
    ("tui.editor.cursorLeft", Editor::move_cursor_left_action),
    ("tui.editor.pageUp", Editor::page_scroll_up_action),
    ("tui.editor.pageDown", Editor::page_scroll_down_action),
    ("tui.editor.jumpForward", Editor::start_jump_forward),
    ("tui.editor.jumpBackward", Editor::start_jump_backward),
];

/// `\r\n` and `\r` to `\n`, tabs to four spaces.
pub(super) fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ")
}

/// Bracketed-paste CSI-u Ctrl+letter (`ESC [ code ; 5 u`) back to its control byte.
pub(super) fn decode_csi_u_ctrl(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("\x1b[") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let d = digits_at(tail, 0);
        let decoded = (d > 0 && tail[d..].starts_with(";5u"))
            .then(|| tail[..d].parse::<u32>().ok())
            .flatten()
            .and_then(|cp| match cp {
                97..=122 => char::from_u32(cp - 96),
                65..=90 => char::from_u32(cp - 64),
                _ => None,
            });
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[d + 3..];
            }
            None => {
                out.push_str("\x1b[");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_newline_sequence(data: &str, first_unit: u16) -> bool {
    (first_unit == 10 && data.len() > 1)
        || data == "\x1b\r"
        || data == "\x1b[13;2~"
        || (data.len() > 1 && data.contains('\x1b') && data.contains('\r'))
        || data == "\n"
}

impl Editor {
    fn history_prev_action(&mut self) {
        self.cancel_autocomplete();
        self.navigate_history(-1);
    }

    fn history_next_action(&mut self) {
        self.cancel_autocomplete();
        self.navigate_history(1);
    }

    fn move_cursor_right_action(&mut self) {
        self.move_cursor(0, 1);
    }

    fn move_cursor_left_action(&mut self) {
        self.move_cursor(0, -1);
    }

    fn page_scroll_up_action(&mut self) {
        self.page_scroll(-1);
    }

    fn page_scroll_down_action(&mut self) {
        self.page_scroll(1);
    }

    fn start_jump_forward(&mut self) {
        self.jump_forward = Some(true);
    }

    fn start_jump_backward(&mut self) {
        self.jump_forward = Some(false);
    }

    fn handle_pending_jump(&mut self, data: &str) -> bool {
        let Some(forward) = self.jump_forward else {
            return false;
        };
        if self.keys.matches(data, "tui.editor.jumpForward") || self.keys.matches(data, "tui.editor.jumpBackward") {
            self.jump_forward = None;
            return true;
        }
        let printable = decode_printable_key(data)
            .or_else(|| (data.encode_utf16().next().is_some_and(|c| c >= 32)).then(|| data.to_string()));
        self.jump_forward = None;
        if let Some(p) = printable {
            self.jump_to_char(&p, forward);
            return true;
        }
        false
    }

    fn handle_paste_buffer(&mut self, data: &str) -> Option<Option<String>> {
        match buffer_bracketed_paste(&mut self.in_paste, &mut self.paste_buffer, data) {
            BracketedChunk::NotInPaste => None,
            BracketedChunk::Buffering => Some(None),
            BracketedChunk::Complete { content, remaining } => {
                if !content.is_empty() {
                    self.handle_paste(&content);
                }
                if !remaining.is_empty() {
                    return Some(self.handle_input(&remaining));
                }
                Some(None)
            }
        }
    }

    fn handle_pre_dispatch(&mut self, data: &str) -> bool {
        if self.keys.matches(data, "tui.input.copy") {
            return true;
        }
        if self.keys.matches(data, "tui.editor.undo") {
            self.undo();
            return true;
        }
        if self.picker.is_some() && !self.handle_picker_key(data) {
            return true;
        }
        if self.keys.matches(data, "tui.input.tab") && self.picker.is_none() {
            self.handle_tab_completion();
            return true;
        }
        false
    }

    fn dispatch_action_table(&mut self, data: &str, table: &[(&str, EditorAction)]) -> bool {
        for &(id, action) in table {
            if self.keys.matches(data, id) {
                action(self);
                return true;
            }
        }
        false
    }

    fn handle_char_delete(&mut self, data: &str) -> bool {
        let ctx = self.keys.ctx();
        if self.keys.matches(data, "tui.editor.deleteCharBackward") || matches_key(data, "shift+backspace", ctx) {
            self.handle_backspace();
            return true;
        }
        if self.keys.matches(data, "tui.editor.deleteCharForward") || matches_key(data, "shift+delete", ctx) {
            self.handle_forward_delete();
            return true;
        }
        false
    }

    fn is_newline_key(&self, data: &str, first_unit: u16) -> bool {
        self.keys.matches(data, "tui.input.newLine") || is_newline_sequence(data, first_unit)
    }

    fn handle_submit_key(&mut self) -> Option<String> {
        if self.disable_submit {
            return None;
        }
        if self.cursor_col > 0 && self.line().as_bytes()[self.cursor_col - 1] == b'\\' {
            self.handle_backspace();
            self.add_new_line();
            return None;
        }
        Some(self.submit_value())
    }

    fn handle_newline_or_submit(&mut self, data: &str, first_unit: u16) -> Option<Option<String>> {
        if self.is_newline_key(data, first_unit) {
            if self.should_submit_on_backslash_enter(data) {
                self.handle_backspace();
                return Some(Some(self.submit_value()));
            }
            self.add_new_line();
            return Some(None);
        }
        if self.keys.matches(data, "tui.input.submit") {
            return Some(self.handle_submit_key());
        }
        None
    }

    fn cursor_up_action(&mut self) {
        let first = self.current_visual_line().0 == 0;
        if first && (self.is_editor_empty() || self.history_index.is_some() || self.cursor_col == 0) {
            self.navigate_history(-1);
        } else if first {
            self.move_to_line_start();
        } else {
            self.move_cursor(-1, 0);
        }
    }

    fn cursor_down_action(&mut self) {
        let (cur, vls) = self.current_visual_line();
        let last = cur == vls.len() - 1;
        if self.history_index.is_some() && last {
            self.navigate_history(1);
        } else if last {
            self.move_to_line_end();
        } else {
            self.move_cursor(1, 0);
        }
    }

    fn handle_vertical_nav(&mut self, data: &str) -> bool {
        if self.keys.matches(data, "tui.editor.cursorUp") {
            self.cursor_up_action();
            return true;
        }
        if self.keys.matches(data, "tui.editor.cursorDown") {
            self.cursor_down_action();
            return true;
        }
        false
    }

    fn handle_printable_fallback(&mut self, data: &str, first_unit: u16) {
        if matches_key(data, "shift+space", self.keys.ctx()) {
            self.insert_character(" ");
        } else if let Some(p) = decode_printable_key(data) {
            self.insert_character(&p);
        } else if first_unit >= 32 {
            self.insert_character(data);
        }
    }

    fn handle_edit_or_nav(&mut self, data: &str) -> bool {
        self.dispatch_action_table(data, PRE_DELETE_ACTIONS)
            || self.handle_char_delete(data)
            || self.dispatch_action_table(data, POST_DELETE_ACTIONS)
    }

    /// Returns the submitted text when the input submits (TS `onSubmit`).
    pub fn handle_input(&mut self, data: &str) -> Option<String> {
        if self.handle_pending_jump(data) {
            return None;
        }
        if let Some(res) = self.handle_paste_buffer(data) {
            return res;
        }
        if self.handle_pre_dispatch(data) || self.handle_edit_or_nav(data) {
            return None;
        }
        let first_unit = data.encode_utf16().next().unwrap_or(0);
        if let Some(res) = self.handle_newline_or_submit(data, first_unit) {
            return res;
        }
        if self.handle_vertical_nav(data) || self.dispatch_action_table(data, POST_NAV_ACTIONS) {
            return None;
        }
        self.handle_printable_fallback(data, first_unit);
        None
    }
}
