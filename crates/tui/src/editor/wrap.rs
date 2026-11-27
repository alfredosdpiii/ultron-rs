use unicode_segmentation::UnicodeSegmentation;

use super::Editor;
use crate::utils::{is_cjk_break, visible_width};
use jsrt::string;

/// One visual chunk of a wrapped logical line; `start`/`end` are byte offsets into the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy)]
pub(super) struct VisualLine {
    pub(super) logical: usize,
    pub(super) start: usize,
    pub(super) len: usize,
}

/// TS `isWhitespaceChar(segment)`: `/\s/.test`, true when any character is whitespace.
pub(super) fn is_ws(s: &str) -> bool {
    s.chars().any(string::is_space)
}

fn is_cjk(s: &str) -> bool {
    s.chars().any(is_cjk_break)
}

/// Byte offset of the first character starting at or after UTF-16 offset `u`.
pub(super) fn byte_of_u16(s: &str, u: usize) -> usize {
    let mut acc = 0;
    for (b, c) in s.char_indices() {
        if acc >= u {
            return b;
        }
        acc += c.len_utf16();
    }
    s.len()
}

/// A `[paste #N( +L lines| C chars)?]` match.
pub(super) struct Marker<'a> {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) id: &'a str,
    pub(super) suffix: &'a str,
}

pub(super) fn digits_at(s: &str, i: usize) -> usize {
    s[i..].bytes().take_while(u8::is_ascii_digit).count()
}

fn marker_suffix_len(text: &str, after: usize) -> Option<usize> {
    let rest = text[after..].strip_prefix(' ')?;
    let (plus, unit) = if rest.starts_with('+') {
        (1, " lines")
    } else {
        (0, " chars")
    };
    let d = digits_at(rest, plus);
    (d > 0 && rest[plus + d..].starts_with(unit)).then_some(1 + plus + d + unit.len())
}

/// Parse a marker starting at byte `i` (PASTE_MARKER_REGEX, with its backtracking on the optional suffix).
pub(super) fn marker_at(text: &str, i: usize) -> Option<Marker<'_>> {
    let id_start = i + "[paste #".len();
    if !text[i..].starts_with("[paste #") {
        return None;
    }
    let n = digits_at(text, id_start);
    if n == 0 {
        return None;
    }
    let after = id_start + n;
    if let Some(len) = marker_suffix_len(text, after)
        && text[after + len..].starts_with(']')
    {
        return Some(Marker {
            start: i,
            end: after + len + 1,
            id: &text[id_start..after],
            suffix: &text[after..after + len],
        });
    }
    text[after..].starts_with(']').then(|| Marker {
        start: i,
        end: after + 1,
        id: &text[id_start..after],
        suffix: "",
    })
}

/// All non-overlapping markers, left to right (`matchAll`).
pub(super) fn find_markers(text: &str) -> Vec<Marker<'_>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = text[pos..].find("[paste #") {
        let i = pos + off;
        match marker_at(text, i) {
            Some(m) => {
                pos = m.end;
                out.push(m);
            }
            None => pos = i + 1,
        }
    }
    out
}

/// TS `isPasteMarker`: the whole segment is one marker.
fn is_paste_marker(s: &str) -> bool {
    s.len() >= 10 && marker_at(s, 0).is_some_and(|m| m.end == s.len())
}

/// Grapheme segments with valid paste markers merged into single atomic segments (`segmentWithMarkers`).
pub(super) fn graphemes_with_markers<'a>(text: &'a str, valid: &[u64]) -> Vec<(usize, &'a str)> {
    let spans: Vec<(usize, usize)> = if valid.is_empty() || !text.contains("[paste #") {
        Vec::new()
    } else {
        find_markers(text)
            .into_iter()
            .filter(|m| m.id.parse::<u64>().is_ok_and(|id| valid.contains(&id)))
            .map(|m| (m.start, m.end))
            .collect()
    };
    let mut out = Vec::new();
    let mut mi = 0;
    for (idx, g) in text.grapheme_indices(true) {
        while mi < spans.len() && spans[mi].1 <= idx {
            mi += 1;
        }
        match spans.get(mi) {
            Some(&(s, e)) if idx >= s && idx < e => {
                if idx == s {
                    out.push((s, &text[s..e]));
                }
            }
            _ => out.push((idx, g)),
        }
    }
    out
}

/// `wordWrapLine`: wrap at word boundaries (whitespace runs, CJK), force-breaking words wider than `max_width`.
pub fn word_wrap_line(line: &str, max_width: usize) -> Vec<TextChunk> {
    wrap_segments(line, max_width, &graphemes_with_markers(line, &[]))
}

struct WrapState<'a> {
    line: &'a str,
    max_width: usize,
    chunks: Vec<TextChunk>,
    current_width: usize,
    chunk_start: usize,
    wrap_opp: Option<(usize, usize)>,
}

impl<'a> WrapState<'a> {
    fn make_chunk(line: &str, start: usize, end: usize) -> TextChunk {
        TextChunk {
            text: line[start..end].to_string(),
            start,
            end,
        }
    }

    fn flush_before(&mut self, char_index: usize, g_width: usize) {
        if self.current_width + g_width <= self.max_width {
            return;
        }
        match self.wrap_opp {
            Some((idx, w)) if self.current_width - w + g_width <= self.max_width => {
                self.chunks.push(Self::make_chunk(self.line, self.chunk_start, idx));
                self.chunk_start = idx;
                self.current_width -= w;
            }
            _ if self.chunk_start < char_index => {
                self.chunks
                    .push(Self::make_chunk(self.line, self.chunk_start, char_index));
                self.chunk_start = char_index;
                self.current_width = 0;
            }
            _ => {}
        }
        self.wrap_opp = None;
    }

    fn push_oversized(&mut self, char_index: usize, grapheme: &str) {
        let sub = word_wrap_line(grapheme, self.max_width);
        let (last, init) = sub.split_last().expect("wordWrapLine returns at least one chunk");
        for sc in init {
            self.chunks
                .push(Self::make_chunk(self.line, char_index + sc.start, char_index + sc.end));
        }
        self.chunk_start = char_index + last.start;
        self.current_width = visible_width(&last.text);
        self.wrap_opp = None;
    }

    fn record_wrap_opp(&mut self, segments: &[(usize, &str)], i: usize, grapheme: &str, marker: bool, ws: bool) {
        let Some(&(next_index, next)) = segments.get(i + 1) else {
            return;
        };
        let next_marker = is_paste_marker(next);
        let ws_break = ws && (next_marker || !is_ws(next));
        let cjk_break = !ws && !is_ws(next) && ((!marker && is_cjk(grapheme)) || (!next_marker && is_cjk(next)));
        if ws_break || cjk_break {
            self.wrap_opp = Some((next_index, self.current_width));
        }
    }
}

pub(super) fn wrap_segments(line: &str, max_width: usize, segments: &[(usize, &str)]) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![WrapState::make_chunk(line, 0, 0)];
    }
    if visible_width(line) <= max_width {
        return vec![WrapState::make_chunk(line, 0, line.len())];
    }
    let mut state = WrapState {
        line,
        max_width,
        chunks: Vec::new(),
        current_width: 0,
        chunk_start: 0,
        wrap_opp: None,
    };
    for (i, &(char_index, grapheme)) in segments.iter().enumerate() {
        let g_width = visible_width(grapheme);
        let marker = is_paste_marker(grapheme);
        let ws = !marker && is_ws(grapheme);
        state.flush_before(char_index, g_width);
        if g_width > max_width && grapheme.graphemes(true).nth(1).is_some() {
            state.push_oversized(char_index, grapheme);
            continue;
        }
        state.current_width += g_width;
        state.record_wrap_opp(segments, i, grapheme, marker, ws);
    }
    state
        .chunks
        .push(WrapState::make_chunk(line, state.chunk_start, line.len()));
    state.chunks
}

pub(super) fn find_visual_line_at(vls: &[VisualLine], line: usize, col: usize) -> usize {
    for (i, vl) in vls.iter().enumerate() {
        if vl.logical != line || col < vl.start {
            continue;
        }
        let offset = col - vl.start;
        let last = i == vls.len() - 1 || vls[i + 1].logical != vl.logical;
        if offset < vl.len || (last && offset == vl.len) {
            return i;
        }
    }
    vls.len().saturating_sub(1)
}

impl Editor {
    pub(super) fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            if line.is_empty() || visible_width(line) <= width {
                out.push(VisualLine {
                    logical: i,
                    start: 0,
                    len: line.len(),
                });
            } else {
                for c in wrap_segments(line, width, &self.graphemes(line)) {
                    out.push(VisualLine {
                        logical: i,
                        start: c.start,
                        len: c.end - c.start,
                    });
                }
            }
        }
        out
    }

    pub(super) fn current_visual_line(&self) -> (usize, Vec<VisualLine>) {
        let vls = self.build_visual_line_map(self.last_width);
        (find_visual_line_at(&vls, self.cursor_line, self.cursor_col), vls)
    }

    /// Layout lines as (text, cursor byte position when the cursor is on it).
    pub(super) fn layout_text(&self, width: usize) -> Vec<(String, Option<usize>)> {
        if self.is_editor_empty() {
            return vec![(String::new(), Some(0))];
        }
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            let current = i == self.cursor_line;
            if visible_width(line) <= width {
                out.push((line.clone(), current.then_some(self.cursor_col)));
                continue;
            }
            let chunks = wrap_segments(line, width, &self.graphemes(line));
            let n = chunks.len();
            for (ci, chunk) in chunks.into_iter().enumerate() {
                let c = self.cursor_col;
                let has = current && c >= chunk.start && (ci == n - 1 || c < chunk.end);
                let pos = has.then(|| (c - chunk.start).min(chunk.text.len()));
                out.push((chunk.text, pos));
            }
        }
        out
    }
}
