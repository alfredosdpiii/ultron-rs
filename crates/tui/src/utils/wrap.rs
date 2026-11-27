use jsrt::string;
use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{AnsiCodeTracker, active_osc8_close, ansi_len, text_run_end};
use super::width::{grapheme_width, is_cjk_break, is_printable_ascii, visible_width};

fn js_is_blank(s: &str) -> bool {
    s.chars().all(string::is_space)
}

fn push_grapheme_token(
    seg: &str,
    current: &mut String,
    pending: &mut String,
    current_is_space: &mut Option<bool>,
    tokens: &mut Vec<String>,
) {
    let is_space = seg == " ";
    if !is_space && seg.chars().any(is_cjk_break) {
        if !current.is_empty() {
            tokens.push(std::mem::take(current));
        }
        *current_is_space = None;
        tokens.push(std::mem::take(pending) + seg);
        return;
    }
    if !current.is_empty() && *current_is_space != Some(is_space) {
        tokens.push(std::mem::take(current));
    }
    current.push_str(&std::mem::take(pending));
    *current_is_space = Some(is_space);
    current.push_str(seg);
}

fn flush_trailing_pending(pending: String, current: &mut String, tokens: &mut Vec<String>) {
    if !pending.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending);
        } else {
            *current = pending;
        }
    }
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending = String::new();
    let mut current_is_space: Option<bool> = None;
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = ansi_len(text, i) {
            pending.push_str(&text[i..i + len]);
            i += len;
            continue;
        }
        let end = text_run_end(text, i, false);
        for seg in text[i..end].graphemes(true) {
            push_grapheme_token(seg, &mut current, &mut pending, &mut current_is_space, &mut tokens);
        }
        i = end;
    }
    flush_trailing_pending(pending, &mut current, &mut tokens);
    tokens
}

/// Word-wrap text to `width` columns, carrying active ANSI styles across breaks. No padding.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::default();
    for input_line in split_lines(text) {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.active_codes()
        };
        result.extend(wrap_single_line(&(prefix + input_line), width));
        tracker.update_from_text(input_line);
    }
    result
}

/// `text.split(/\r\n|\r|\n/)`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\r' => {
                out.push(&text[start..i]);
                i += if b.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                start = i;
            }
            b'\n' => {
                out.push(&text[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push(&text[start..]);
    out
}

fn wrap_oversized_token(
    token: &str,
    width: usize,
    tracker: &mut AnsiCodeTracker,
    current: &mut String,
    wrapped: &mut Vec<String>,
) -> usize {
    if !current.is_empty() {
        current.push_str(&tracker.line_end_reset());
        wrapped.push(std::mem::take(current));
    }
    let mut broken = break_long_word(token, width, tracker);
    *current = broken.pop().unwrap_or_default();
    wrapped.extend(broken);
    visible_width(current)
}

#[expect(clippy::too_many_arguments, reason = "internal line wrapping state")]
fn wrap_fitting_token(
    token: &str,
    token_width: usize,
    width: usize,
    is_ws: bool,
    tracker: &AnsiCodeTracker,
    current: &mut String,
    current_width: &mut usize,
    wrapped: &mut Vec<String>,
) {
    if *current_width + token_width > width && *current_width > 0 {
        let mut line_to_wrap = current.trim_end_matches(string::is_space).to_string();
        line_to_wrap.push_str(&tracker.line_end_reset());
        wrapped.push(line_to_wrap);
        if is_ws {
            *current = tracker.active_codes();
            *current_width = 0;
        } else {
            *current = tracker.active_codes() + token;
            *current_width = token_width;
        }
    } else {
        current.push_str(token);
        *current_width += token_width;
    }
}

fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if visible_width(line) <= width {
        return vec![line.to_string()];
    }
    let mut wrapped: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::default();
    let mut current = String::new();
    let mut current_width = 0;
    for token in split_into_tokens_with_ansi(line) {
        let token_width = visible_width(&token);
        let is_ws = js_is_blank(&token);
        if token_width > width && !is_ws {
            current_width = wrap_oversized_token(&token, width, &mut tracker, &mut current, &mut wrapped);
            continue;
        }
        wrap_fitting_token(
            &token,
            token_width,
            width,
            is_ws,
            &tracker,
            &mut current,
            &mut current_width,
            &mut wrapped,
        );
        tracker.update_from_text(&token);
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        return vec![String::new()];
    }
    wrapped
        .iter()
        .map(|l| l.trim_end_matches(string::is_space).to_string())
        .collect()
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = tracker.active_codes();
    let mut current_width = 0;
    let mut i = 0;
    while i < word.len() {
        if let Some(len) = ansi_len(word, i) {
            let code = &word[i..i + len];
            current.push_str(code);
            tracker.process(code);
            i += len;
            continue;
        }
        let end = text_run_end(word, i, false);
        for g in word[i..end].graphemes(true) {
            let w = visible_width(g);
            if current_width + w > width {
                current.push_str(&tracker.line_end_reset());
                lines.push(std::mem::replace(&mut current, tracker.active_codes()));
                current_width = 0;
            }
            current.push_str(g);
            current_width += w;
        }
        i = end;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() { vec![String::new()] } else { lines }
}

/// Pad `line` to `width` and wrap it with a background function.
pub fn apply_background_to_line(line: &str, width: usize, bg: &dyn Fn(&str) -> String) -> String {
    let pad = width.saturating_sub(visible_width(line));
    bg(&format!("{line}{}", " ".repeat(pad)))
}

fn truncate_fragment(text: &str, max_width: usize) -> (String, usize) {
    if max_width == 0 || text.is_empty() {
        return (String::new(), 0);
    }
    if is_printable_ascii(text) {
        let clipped = &text[..text.len().min(max_width)];
        return (clipped.to_string(), clipped.len());
    }
    let mut result = String::new();
    let mut width = 0;
    let mut pending = String::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = ansi_len(text, i) {
            pending.push_str(&text[i..i + len]);
            i += len;
            continue;
        }
        if text.as_bytes()[i] == b'\t' {
            if width + 3 > max_width {
                break;
            }
            result.push_str(&std::mem::take(&mut pending));
            result.push('\t');
            width += 3;
            i += 1;
            continue;
        }
        let end = text_run_end(text, i, true);
        for g in text[i..end].graphemes(true) {
            let w = grapheme_width(g);
            if width + w > max_width {
                return (result, width);
            }
            result.push_str(&std::mem::take(&mut pending));
            result.push_str(g);
            width += w;
        }
        i = end;
    }
    (result, width)
}

fn finalize_truncated(
    prefix: &str,
    prefix_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max: usize,
    pad: bool,
) -> String {
    let close = active_osc8_close(prefix);
    let mut out = if ellipsis.is_empty() {
        format!("{prefix}{close}\x1b[0m")
    } else {
        format!("{prefix}{close}\x1b[0m{ellipsis}\x1b[0m")
    };
    if pad {
        out.push_str(&" ".repeat(max.saturating_sub(prefix_width + ellipsis_width)));
    }
    out
}

fn pad_to_width(s: &str, w: usize, max_width: usize, pad: bool) -> String {
    if pad {
        format!("{s}{}", " ".repeat(max_width.saturating_sub(w)))
    } else {
        s.to_string()
    }
}

fn truncate_wide_ellipsis(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    let text_width = visible_width(text);
    if text_width <= max_width {
        return pad_to_width(text, text_width, max_width, pad);
    }
    let (clipped, w) = truncate_fragment(ellipsis, max_width);
    if w == 0 {
        return pad_to_width("", 0, max_width, pad);
    }
    finalize_truncated("", 0, &clipped, w, max_width, pad)
}

#[derive(Default)]
struct TruncateState {
    result: String,
    pending: String,
    seen: usize,
    kept: usize,
    keep: bool,
}

impl TruncateState {
    fn new() -> Self {
        Self {
            keep: true,
            ..Self::default()
        }
    }

    fn push_unit(&mut self, unit: &str, w: usize, target: usize, max_width: usize) -> bool {
        if self.keep && self.kept + w <= target {
            self.result.push_str(&std::mem::take(&mut self.pending));
            self.result.push_str(unit);
            self.kept += w;
        } else {
            self.keep = false;
            self.pending.clear();
        }
        self.seen += w;
        self.seen > max_width
    }

    fn push_run(&mut self, run: &str, target: usize, max_width: usize) -> bool {
        for g in run.graphemes(true) {
            if self.push_unit(g, grapheme_width(g), target, max_width) {
                return true;
            }
        }
        false
    }
}

fn truncate_ascii_or_wide(text: &str, max_width: usize, ellipsis: &str, ellipsis_width: usize, pad: bool) -> String {
    if text.len() <= max_width {
        return pad_to_width(text, text.len(), max_width, pad);
    }
    let target = max_width - ellipsis_width;
    finalize_truncated(&text[..target], target, ellipsis, ellipsis_width, max_width, pad)
}

/// Truncate to `max_width` columns, appending `ellipsis` when cut; optionally pad to exactly `max_width`.
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return pad_to_width("", 0, max_width, pad);
    }
    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        return truncate_wide_ellipsis(text, max_width, ellipsis, pad);
    }
    if is_printable_ascii(text) {
        return truncate_ascii_or_wide(text, max_width, ellipsis, ellipsis_width, pad);
    }
    let target = max_width - ellipsis_width;
    let mut state = TruncateState::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = ansi_len(text, i) {
            state.pending.push_str(&text[i..i + len]);
            i += len;
            continue;
        }
        if text.as_bytes()[i] == b'\t' {
            if state.push_unit("\t", 3, target, max_width) {
                return finalize_truncated(&state.result, state.kept, ellipsis, ellipsis_width, max_width, pad);
            }
            i += 1;
            continue;
        }
        let end = text_run_end(text, i, true);
        if state.push_run(&text[i..end], target, max_width) {
            return finalize_truncated(&state.result, state.kept, ellipsis, ellipsis_width, max_width, pad);
        }
        i = end;
    }
    pad_to_width(text, state.seen, max_width, pad)
}

struct SliceState {
    result: String,
    pending: String,
    result_width: usize,
    col: usize,
}

impl SliceState {
    fn new() -> Self {
        Self {
            result: String::new(),
            pending: String::new(),
            result_width: 0,
            col: 0,
        }
    }

    fn push_ansi(&mut self, code: &str, start: usize, end_col: usize) {
        if self.col >= start && self.col < end_col {
            self.result.push_str(code);
        } else if self.col < start {
            self.pending.push_str(code);
        }
    }

    fn push_run(&mut self, run: &str, start: usize, end_col: usize, strict: bool) {
        for g in run.graphemes(true) {
            let w = grapheme_width(g);
            if self.col >= start && self.col < end_col && (!strict || self.col + w <= end_col) {
                self.result.push_str(&std::mem::take(&mut self.pending));
                self.result.push_str(g);
                self.result_width += w;
            }
            self.col += w;
            if self.col >= end_col {
                break;
            }
        }
    }
}

/// Visible columns `[start, start+len)` of a line, keeping escape codes; `strict` drops wide chars crossing the end.
pub fn slice_with_width(line: &str, start: usize, length: usize, strict: bool) -> (String, usize) {
    if length == 0 {
        return (String::new(), 0);
    }
    let end_col = start + length;
    let mut state = SliceState::new();
    let mut i = 0;
    while i < line.len() {
        if let Some(len) = ansi_len(line, i) {
            state.push_ansi(&line[i..i + len], start, end_col);
            i += len;
            continue;
        }
        let end = text_run_end(line, i, false);
        state.push_run(&line[i..end], start, end_col, strict);
        i = end;
        if state.col >= end_col {
            break;
        }
    }
    (state.result, state.result_width)
}
