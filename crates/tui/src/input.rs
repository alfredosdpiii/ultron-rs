//! Single-line text input (port of packages/tui/src/components/input.ts). Cursor is a byte offset.

use unicode_segmentation::UnicodeSegmentation;

use crate::attachments::{BracketedChunk, buffer_bracketed_paste};
use crate::components::Component;
use crate::editing::{KillRing, find_word_backward, find_word_forward, next_grapheme_len, prev_grapheme_len};
use crate::frame::CURSOR_MARKER;
use crate::keybindings::KeybindingsManager;
use crate::keys::decode_printable_key;
use crate::mouse::{MouseButton, MouseEvent, MouseEventType, MouseResult};
use crate::utils::{slice_with_width, truncate_to_width, visible_width};

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// What `handle_input` asked the owner to do (TS `onSubmit` / `onEscape` callbacks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Submit(String),
    Escape,
}

/// Styles placeholder text.
pub type StyleFn = Box<dyn Fn(&str) -> String>;

pub struct Input {
    value: String,
    cursor: usize,
    prompt: String,
    placeholder: String,
    placeholder_style: StyleFn,
    mask: Option<String>,
    rendered_start_column: usize,
    pub focused: bool,
    paste_buffer: String,
    in_paste: bool,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo_stack: Vec<(String, usize)>,
    keys: KeybindingsManager,
}

impl Input {
    /// `keys` stands in for the TS global `getKeybindings()`.
    pub fn new(keys: KeybindingsManager) -> Self {
        Input {
            value: String::new(),
            cursor: 0,
            prompt: "> ".into(),
            placeholder: String::new(),
            placeholder_style: Box::new(str::to_string),
            mask: None,
            rendered_start_column: 0,
            focused: false,
            paste_buffer: String::new(),
            in_paste: false,
            kill_ring: KillRing::default(),
            last_action: None,
            undo_stack: Vec::new(),
            keys,
        }
    }

    pub fn with_options(
        mut self,
        prompt: Option<&str>,
        placeholder: Option<&str>,
        placeholder_style: Option<StyleFn>,
        mask: Option<&str>,
    ) -> Self {
        if let Some(p) = prompt {
            self.prompt = p.into();
        }
        if let Some(p) = placeholder {
            self.placeholder = p.into();
        }
        if let Some(s) = placeholder_style {
            self.placeholder_style = s;
        }
        self.mask = mask.map(str::to_string);
        self
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.cursor.min(value.len());
        while !self.value.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    pub fn move_cursor_to_end(&mut self) {
        self.cursor = self.value.len();
    }
}

type InputAction = fn(&mut Input);

const INPUT_ACTIONS: &[(&str, InputAction)] = &[
    ("tui.editor.deleteCharBackward", Input::delete_char_backward),
    ("tui.editor.deleteCharForward", Input::delete_char_forward),
    ("tui.editor.deleteWordBackward", Input::delete_word_backward),
    ("tui.editor.deleteWordForward", Input::delete_word_forward),
    ("tui.editor.deleteToLineStart", Input::delete_to_line_start),
    ("tui.editor.deleteToLineEnd", Input::delete_to_line_end),
    ("tui.editor.yank", Input::yank),
    ("tui.editor.yankPop", Input::yank_pop),
    ("tui.editor.cursorLeft", Input::move_left),
    ("tui.editor.cursorRight", Input::move_right),
    ("tui.editor.cursorLineStart", Input::move_line_start),
    ("tui.editor.cursorLineEnd", Input::move_line_end),
    ("tui.editor.cursorWordLeft", Input::move_word_left),
    ("tui.editor.cursorWordRight", Input::move_word_right),
];

impl Input {
    pub fn handle_input(&mut self, data: &str) -> Option<InputEvent> {
        if let Some(res) = self.handle_paste_input(data) {
            return res;
        }
        if self.keys.matches(data, "tui.select.cancel") {
            return Some(InputEvent::Escape);
        }
        if self.keys.matches(data, "tui.editor.undo") {
            self.undo();
            return None;
        }
        if self.keys.matches(data, "tui.input.submit") || data == "\n" {
            return Some(InputEvent::Submit(self.value.clone()));
        }
        if self.dispatch_input_action(data) {
            return None;
        }
        self.handle_printable_fallback(data);
        None
    }

    fn handle_paste_input(&mut self, data: &str) -> Option<Option<InputEvent>> {
        match buffer_bracketed_paste(&mut self.in_paste, &mut self.paste_buffer, data) {
            BracketedChunk::NotInPaste => None,
            BracketedChunk::Buffering => Some(None),
            BracketedChunk::Complete { content, remaining } => {
                self.handle_paste(&content);
                if !remaining.is_empty() {
                    return Some(self.handle_input(&remaining));
                }
                Some(None)
            }
        }
    }

    fn dispatch_input_action(&mut self, data: &str) -> bool {
        for &(id, action) in INPUT_ACTIONS {
            if self.keys.matches(data, id) {
                action(self);
                return true;
            }
        }
        false
    }

    fn handle_printable_fallback(&mut self, data: &str) {
        if let Some(ch) = data.ends_with('u').then(|| decode_printable_key(data)).flatten() {
            self.insert_character(&ch);
        } else if !data
            .chars()
            .any(|c| (c as u32) < 32 || (0x7f..=0x9f).contains(&(c as u32)))
        {
            self.insert_character(data);
        }
    }

    fn delete_char_backward(&mut self) {
        self.last_action = None;
        if self.cursor > 0 {
            self.push_undo();
            let n = prev_grapheme_len(&self.value, self.cursor);
            self.value.replace_range(self.cursor - n..self.cursor, "");
            self.cursor -= n;
        }
    }

    fn delete_char_forward(&mut self) {
        self.last_action = None;
        if self.cursor < self.value.len() {
            self.push_undo();
            let n = next_grapheme_len(&self.value, self.cursor);
            self.value.replace_range(self.cursor..self.cursor + n, "");
        }
    }

    fn delete_word_backward(&mut self) {
        if self.cursor > 0 {
            let was_kill = self.last_action == Some(LastAction::Kill);
            self.push_undo();
            let from = find_word_backward(&self.value, self.cursor);
            self.kill(from, self.cursor, true, was_kill);
            self.cursor = from;
        }
    }

    fn delete_word_forward(&mut self) {
        if self.cursor < self.value.len() {
            let was_kill = self.last_action == Some(LastAction::Kill);
            self.push_undo();
            let to = find_word_forward(&self.value, self.cursor);
            self.kill(self.cursor, to, false, was_kill);
        }
    }

    fn delete_to_line_start(&mut self) {
        if self.cursor > 0 {
            self.push_undo();
            let was_kill = self.last_action == Some(LastAction::Kill);
            self.kill(0, self.cursor, true, was_kill);
            self.cursor = 0;
        }
    }

    fn delete_to_line_end(&mut self) {
        if self.cursor < self.value.len() {
            self.push_undo();
            let was_kill = self.last_action == Some(LastAction::Kill);
            self.kill(self.cursor, self.value.len(), false, was_kill);
        }
    }

    fn yank(&mut self) {
        if let Some(text) = self.kill_ring.peek().filter(|t| !t.is_empty()).map(str::to_string) {
            self.push_undo();
            self.insert(&text);
            self.last_action = Some(LastAction::Yank);
        }
    }

    fn yank_pop(&mut self) {
        if self.last_action == Some(LastAction::Yank) && self.kill_ring.len() > 1 {
            self.push_undo();
            let prev = self.kill_ring.peek().unwrap_or_default().len();
            self.value.replace_range(self.cursor - prev..self.cursor, "");
            self.cursor -= prev;
            self.kill_ring.rotate();
            let text = self.kill_ring.peek().unwrap_or_default().to_string();
            self.insert(&text);
            self.last_action = Some(LastAction::Yank);
        }
    }

    fn move_left(&mut self) {
        self.last_action = None;
        self.cursor -= prev_grapheme_len(&self.value, self.cursor);
    }

    fn move_right(&mut self) {
        self.last_action = None;
        self.cursor += next_grapheme_len(&self.value, self.cursor);
    }

    fn move_line_start(&mut self) {
        self.last_action = None;
        self.cursor = 0;
    }

    fn move_line_end(&mut self) {
        self.last_action = None;
        self.cursor = self.value.len();
    }

    fn move_word_left(&mut self) {
        if self.cursor > 0 {
            self.last_action = None;
            self.cursor = find_word_backward(&self.value, self.cursor);
        }
    }

    fn move_word_right(&mut self) {
        if self.cursor < self.value.len() {
            self.last_action = None;
            self.cursor = find_word_forward(&self.value, self.cursor);
        }
    }

    fn kill(&mut self, from: usize, to: usize, prepend: bool, accumulate: bool) {
        self.kill_ring.push(&self.value[from..to], prepend, accumulate);
        self.last_action = Some(LastAction::Kill);
        self.value.replace_range(from..to, "");
    }

    fn insert(&mut self, text: &str) {
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn insert_character(&mut self, ch: &str) {
        if ch.chars().any(char::is_whitespace) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        self.insert(ch);
    }

    fn handle_paste(&mut self, text: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = text.replace(['\r', '\n'], "").replace('\t', "    ");
        self.insert(&clean);
    }

    fn push_undo(&mut self) {
        self.undo_stack.push((self.value.clone(), self.cursor));
    }

    fn undo(&mut self) {
        if let Some((value, cursor)) = self.undo_stack.pop() {
            self.value = value;
            self.cursor = cursor;
            self.last_action = None;
        }
    }
}

fn graphemes(text: &str) -> usize {
    text.graphemes(true).count()
}

impl Input {
    /// `handleMouse`: a left press on the row puts the cursor at the cell under it (past the two-cell prompt).
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseResult> {
        if event.kind != MouseEventType::Press || event.button != MouseButton::Left || event.y != 0 {
            return None;
        }
        let target = self.rendered_start_column + (event.x - 2).max(0) as usize;
        let mut column = 0;
        self.cursor = self.value.len();
        for (index, grapheme) in self.value.grapheme_indices(true) {
            let next = column + visible_width(grapheme);
            if target < next {
                self.cursor = index;
                break;
            }
            column = next;
        }
        self.last_action = None;
        Some(MouseResult {
            handled: Some(true),
            focus: Some(true),
            ..MouseResult::default()
        })
    }

    fn masked_value_and_cursor(&self) -> (String, usize) {
        match &self.mask {
            None => (self.value.clone(), self.cursor),
            Some(m) => (
                m.repeat(graphemes(&self.value)),
                m.len() * graphemes(&self.value[..self.cursor]),
            ),
        }
    }

    fn render_placeholder(&self, available: usize, marker: &str) -> String {
        let ph = truncate_to_width(&self.placeholder, available, "", false);
        let at = ph.graphemes(true).next().unwrap_or(" ");
        let after = if at.len() <= ph.len() { &ph[at.len()..] } else { "" };
        let style = &self.placeholder_style;
        let text = format!("{marker}\x1b[7m{}\x1b[27m{}", style(at), style(after));
        let pad = " ".repeat(available.saturating_sub(visible_width(&text)));
        format!("{}{text}{pad}", self.prompt)
    }

    fn compute_scroll_window(&mut self, value: &str, cursor: usize, available: usize) -> (String, usize) {
        self.rendered_start_column = 0;
        let total = visible_width(value);
        if total < available {
            return (value.to_string(), cursor);
        }
        let scroll = if cursor == value.len() {
            available - 1
        } else {
            available
        };
        if scroll == 0 {
            return (String::new(), 0);
        }
        let cursor_col = visible_width(&value[..cursor]);
        let half = scroll / 2;
        let start = if cursor_col < half {
            0
        } else if cursor_col > total.saturating_sub(half) {
            total.saturating_sub(scroll)
        } else {
            cursor_col.saturating_sub(half)
        };
        self.rendered_start_column = start;
        let visible = slice_with_width(value, start, scroll, true).0;
        let cursor_display = slice_with_width(value, start, cursor_col.saturating_sub(start), true)
            .0
            .len();
        (visible, cursor_display)
    }
}

impl Component for Input {
    fn render(&mut self, width: usize) -> Vec<String> {
        let prompt_w = visible_width(&self.prompt);
        if width <= prompt_w {
            return vec![truncate_to_width(&self.prompt, width, "", false)];
        }
        let available = width - prompt_w;
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let (value, cursor) = self.masked_value_and_cursor();
        if value.is_empty() && !self.placeholder.is_empty() {
            return vec![self.render_placeholder(available, marker)];
        }
        let (visible, cursor_display) = self.compute_scroll_window(&value, cursor, available);
        let at = visible[cursor_display..].graphemes(true).next().unwrap_or(" ");
        let after = visible.get(cursor_display + at.len()..).unwrap_or("");
        let text = format!("{}{marker}\x1b[7m{at}\x1b[27m{after}", &visible[..cursor_display]);
        let pad = " ".repeat(available.saturating_sub(visible_width(&text)));
        vec![format!("{}{text}{pad}", self.prompt)]
    }
}
