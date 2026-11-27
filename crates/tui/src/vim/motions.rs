use std::sync::OnceLock;

use fancy_regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use super::{
    Desired, FindSpec, Motion, Operator, Pos, VimBuffer, VimState, first_non_blank, is_space_at, next_grapheme_start,
    pos, prev_grapheme_start,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum NavKind {
    Whitespace,
    Word,
    Delimiter,
    Cjk,
    Other,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap_or_else(|_| Regex::new("^$").unwrap_or_else(|_| unreachable!())))
}

fn regex_matches(cell: &'static OnceLock<Regex>, pat: &str, s: &str) -> bool {
    re(cell, pat).is_match(s).unwrap_or(false)
}

/// omp `getWordNavKind`.
fn nav_kind(g: &str) -> NavKind {
    static DELIM: OnceLock<Regex> = OnceLock::new();
    static CJK: OnceLock<Regex> = OnceLock::new();
    static WORD: OnceLock<Regex> = OnceLock::new();
    let Some(c) = g.chars().next() else {
        return NavKind::Other;
    };
    let s = c.to_string();
    if c.is_whitespace() {
        NavKind::Whitespace
    } else if c == '_' {
        NavKind::Word
    } else if regex_matches(&DELIM, r"^[\p{P}\p{S}]$", &s) {
        NavKind::Delimiter
    } else if regex_matches(&CJK, r"^[\p{Han}\p{Hiragana}\p{Katakana}\p{Hangul}]$", &s) {
        NavKind::Cjk
    } else if regex_matches(&WORD, r"^[\p{L}\p{N}]$", &s) {
        NavKind::Word
    } else {
        NavKind::Other
    }
}

fn is_joiner(g: &str) -> bool {
    matches!(g.chars().next(), Some('\'' | '’' | '-' | '‐' | '‑'))
}

fn skip_left_kind(gs: &mut Vec<&str>, i: &mut usize, kind: NavKind) {
    while gs.last().is_some_and(|g| nav_kind(g) == kind) {
        *i -= gs.pop().map_or(0, str::len);
    }
}

fn is_left_word_joiner(gs: &[&str], g: &str, k: NavKind, has_right_word: bool) -> bool {
    has_right_word
        && k == NavKind::Delimiter
        && is_joiner(g)
        && gs.len() >= 2
        && nav_kind(gs[gs.len() - 2]) == NavKind::Word
}

fn skip_left_word(gs: &mut Vec<&str>, mut i: usize) -> usize {
    let mut has_right_word = false;
    while let Some(g) = gs.last().copied() {
        let k = nav_kind(g);
        if k == NavKind::Word {
            has_right_word = true;
            i -= gs.pop().map_or(0, str::len);
            continue;
        }
        if is_left_word_joiner(gs, g, k, has_right_word) {
            i -= gs.pop().map_or(0, str::len);
            continue;
        }
        break;
    }
    i
}

pub(super) fn move_word_left(text: &str, cursor: usize) -> usize {
    let mut i = cursor.min(text.len());
    if i == 0 {
        return 0;
    }
    let mut gs: Vec<&str> = text[..i].graphemes(true).collect();
    skip_left_kind(&mut gs, &mut i, NavKind::Whitespace);
    let Some(last) = gs.last() else { return i };
    let kind = nav_kind(last);
    if kind == NavKind::Delimiter || kind == NavKind::Cjk {
        skip_left_kind(&mut gs, &mut i, kind);
        return i;
    }
    if kind == NavKind::Word {
        return skip_left_word(&mut gs, i);
    }
    i.saturating_sub(gs.pop().map_or(0, str::len))
}

fn skip_right_kind(gs: &[&str], mut k: usize, mut i: usize, kind: NavKind) -> (usize, usize) {
    while k < gs.len() && nav_kind(gs[k]) == kind {
        i += gs[k].len();
        k += 1;
    }
    (k, i)
}

fn is_right_word_joiner(gs: &[&str], k: usize, kind: NavKind, has_left_word: bool) -> bool {
    has_left_word
        && kind == NavKind::Delimiter
        && is_joiner(gs[k])
        && gs.get(k + 1).is_some_and(|n| nav_kind(n) == NavKind::Word)
}

fn skip_right_word(gs: &[&str], mut k: usize, mut i: usize) -> usize {
    let mut has_left_word = false;
    while k < gs.len() {
        let kind = nav_kind(gs[k]);
        if kind == NavKind::Word {
            has_left_word = true;
            i += gs[k].len();
            k += 1;
            continue;
        }
        if is_right_word_joiner(gs, k, kind, has_left_word) {
            i += gs[k].len();
            k += 1;
            continue;
        }
        break;
    }
    i
}

pub(super) fn move_word_right(text: &str, cursor: usize) -> usize {
    let i = cursor.min(text.len());
    if i == text.len() {
        return i;
    }
    let gs: Vec<&str> = text[i..].graphemes(true).collect();
    let (k, i) = skip_right_kind(&gs, 0, i, NavKind::Whitespace);
    if k == gs.len() {
        return i;
    }
    let first = nav_kind(gs[k]);
    if first == NavKind::Delimiter || first == NavKind::Cjk {
        return skip_right_kind(&gs, k, i, first).1;
    }
    if first == NavKind::Word {
        return skip_right_word(&gs, k, i);
    }
    i + gs[k].len()
}

fn word_forward(text: &str, col: usize) -> usize {
    let mut next = move_word_right(text, col);
    while next < text.len() && is_space_at(text, next) {
        next += text[next..].chars().next().map_or(1, char::len_utf8);
    }
    next
}

fn word_end(text: &str, col: usize) -> usize {
    let mut from = next_grapheme_start(text, col);
    while from < text.len() && is_space_at(text, from) {
        from += text[from..].chars().next().map_or(1, char::len_utf8);
    }
    let end = move_word_right(text, from);
    col.max(prev_grapheme_start(text, end))
}

pub(super) fn quote_marks(text: &str, quote: char) -> Vec<usize> {
    let mut marks = Vec::new();
    let mut it = text.char_indices();
    while let Some((i, c)) = it.next() {
        if c == '\\' {
            it.next();
        } else if c == quote {
            marks.push(i);
        }
    }
    marks
}

pub(super) fn flatten(buf: &VimBuffer) -> (String, usize) {
    let cursor = buf.cursor_col + (0..buf.cursor_line).map(|i| buf.line(i).len() + 1).sum::<usize>();
    (buf.lines.join("\n"), cursor)
}

pub(super) fn offset_to_pos(lines: &[String], offset: usize) -> Pos {
    let mut remaining = offset;
    for (line, text) in lines.iter().enumerate() {
        if remaining <= text.len() {
            return pos(line, remaining);
        }
        remaining -= text.len() + 1;
    }
    let line = lines.len().saturating_sub(1);
    pos(line, lines.get(line).map_or(0, String::len))
}

fn bracket_match(c: u8) -> Option<(u8, bool)> {
    Some(match c {
        b'(' => (b')', true),
        b')' => (b'(', false),
        b'[' => (b']', true),
        b']' => (b'[', false),
        b'{' => (b'}', true),
        b'}' => (b'{', false),
        _ => return None,
    })
}

fn quote_mate(marks: &[usize], col: usize) -> Option<usize> {
    for p in marks.as_chunks::<2>().0 {
        if col == p[0] {
            return Some(p[1]);
        }
        if col == p[1] {
            return Some(p[0]);
        }
    }
    None
}

fn find_quote_marks_at<'a>(dq: &'a [usize], sq: &'a [usize], col: usize) -> Option<&'a [usize]> {
    if dq.contains(&col) {
        Some(dq)
    } else if sq.contains(&col) {
        Some(sq)
    } else {
        None
    }
}

fn find_next_delimiter(lb: &[u8], dq: &[usize], sq: &[usize], mut col: usize) -> usize {
    while col < lb.len() && bracket_match(lb[col]).is_none() && find_quote_marks_at(dq, sq, col).is_none() {
        col += 1;
    }
    col
}

fn scan_bracket_forward(lines: &[String], b: &[u8], cursor: usize, open: u8, close: u8) -> Option<Pos> {
    let mut depth = 0usize;
    for (i, &ch) in b.iter().enumerate().skip(cursor + 1) {
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                return Some(offset_to_pos(lines, i));
            }
            depth -= 1;
        }
    }
    None
}

fn scan_bracket_backward(lines: &[String], b: &[u8], cursor: usize, open: u8, close: u8) -> Option<Pos> {
    let mut depth = 0usize;
    for i in (0..cursor).rev() {
        let ch = b[i];
        if ch == close {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                return Some(offset_to_pos(lines, i));
            }
            depth -= 1;
        }
    }
    None
}

fn match_delimiter(buf: &VimBuffer) -> Option<Pos> {
    let line = buf.line(buf.cursor_line);
    let lb = line.as_bytes();
    let dq = quote_marks(line, '"');
    let sq = quote_marks(line, '\'');
    let col = find_next_delimiter(lb, &dq, &sq, buf.cursor_col);
    if let Some(marks) = find_quote_marks_at(&dq, &sq, col) {
        return quote_mate(marks, col).map(|m| pos(buf.cursor_line, m));
    }
    let (mate, forward) = bracket_match(*lb.get(col)?)?;
    let (text, cursor) = flatten(&VimBuffer {
        lines: buf.lines,
        cursor_line: buf.cursor_line,
        cursor_col: col,
    });
    let b = text.as_bytes();
    if forward {
        scan_bracket_forward(buf.lines, b, cursor, lb[col], mate)
    } else {
        scan_bracket_backward(buf.lines, b, cursor, mate, lb[col])
    }
}

fn is_line_blank(lines: &[String], l: usize) -> bool {
    lines[l].trim().is_empty()
}

fn next_paragraph_line(lines: &[String], mut i: usize, last: usize) -> usize {
    if i < last && !is_line_blank(lines, i) {
        while i < last && !is_line_blank(lines, i + 1) {
            i += 1;
        }
        return if i < last { i + 1 } else { last };
    }
    while i < last && is_line_blank(lines, i) {
        i += 1;
    }
    while i < last && !is_line_blank(lines, i) {
        i += 1;
    }
    i
}

fn prev_paragraph_line(lines: &[String], mut i: usize) -> usize {
    if i > 0 && !is_line_blank(lines, i) {
        while i > 0 && !is_line_blank(lines, i - 1) {
            i -= 1;
        }
        return i.saturating_sub(1);
    }
    while i > 0 && is_line_blank(lines, i) {
        i -= 1;
    }
    while i > 0 && !is_line_blank(lines, i - 1) {
        i -= 1;
    }
    i.saturating_sub(1)
}

fn paragraph_boundary(lines: &[String], line: usize, forward: bool) -> usize {
    if lines.is_empty() {
        return 0;
    }
    let last = lines.len() - 1;
    let i = line.min(last);
    if forward {
        next_paragraph_line(lines, i, last)
    } else {
        prev_paragraph_line(lines, i)
    }
}

fn nth_matching_grapheme<'a>(
    iter: impl Iterator<Item = &'a (usize, &'a str)>,
    target: &str,
    count: usize,
) -> Option<usize> {
    let mut seen = 0;
    for (i, g) in iter {
        if *g == target {
            seen += 1;
            if seen == count {
                return Some(*i);
            }
        }
    }
    None
}

/// Column of the `count`th grapheme equal to `target`, strictly after (or before) `from`.
fn find_grapheme(text: &str, from: usize, target: &str, forward: bool, count: usize) -> Option<usize> {
    if count < 1 || target.is_empty() {
        return None;
    }
    let segs: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let here = segs.iter().rposition(|(i, _)| *i <= from);
    if forward {
        let start = here.map_or(0, |h| h + 1);
        nth_matching_grapheme(segs[start.min(segs.len())..].iter(), target, count)
    } else {
        let h = here?;
        nth_matching_grapheme(segs[..h].iter().rev(), target, count)
    }
}

fn charwise_motion(to: Pos) -> Motion {
    Motion {
        to,
        inclusive: false,
        linewise: false,
    }
}

impl VimState {
    fn vertical_motion(&mut self, key: &str, buf: &VimBuffer, count: usize) -> Motion {
        let target = if key == "j" {
            (buf.cursor_line + count).min(buf.lines.len().saturating_sub(1))
        } else {
            buf.cursor_line.saturating_sub(count)
        };
        let desired = *self.desired.get_or_insert(Desired::Col(buf.cursor_col));
        let col = match desired {
            Desired::Col(c) => c,
            Desired::Eol => usize::MAX,
        };
        Motion {
            to: pos(target, col),
            inclusive: false,
            linewise: true,
        }
    }

    fn word_forward_motion(&self, line: &str, buf: &VimBuffer, count: usize) -> Motion {
        let mut col = buf.cursor_col;
        if self.operator == Some(Operator::Change) && !is_space_at(line, col) {
            for _ in 0..count {
                col = word_end(line, col);
            }
            return Motion {
                to: pos(buf.cursor_line, col),
                inclusive: true,
                linewise: false,
            };
        }
        for _ in 0..count {
            col = word_forward(line, col);
        }
        charwise_motion(pos(buf.cursor_line, col))
    }

    fn percent_motion(&self, buf: &VimBuffer, count: usize) -> Option<Motion> {
        if !self.count.is_empty() {
            if count > 100 {
                return None;
            }
            let target = ((count * buf.lines.len()).div_ceil(100)).saturating_sub(1);
            return Some(Motion {
                to: pos(target, first_non_blank(buf.line(target))),
                inclusive: false,
                linewise: true,
            });
        }
        Some(Motion {
            to: match_delimiter(buf)?,
            inclusive: true,
            linewise: false,
        })
    }

    fn paragraph_motion(key: &str, buf: &VimBuffer, count: usize) -> Motion {
        let mut l = buf.cursor_line;
        for _ in 0..count {
            let next = paragraph_boundary(buf.lines, l, key == "}");
            if next == l {
                break;
            }
            l = next;
        }
        let text = buf.line(l);
        let at_end = key == "}" && l + 1 == buf.lines.len() && !text.trim().is_empty();
        charwise_motion(pos(l, if at_end { text.len() } else { 0 }))
    }

    fn goto_line_motion(&self, buf: &VimBuffer, count: usize) -> Motion {
        let last = buf.lines.len().saturating_sub(1);
        let target = if self.count.is_empty() { last } else { count - 1 };
        Motion {
            to: pos(target.min(last), 0),
            inclusive: false,
            linewise: true,
        }
    }

    fn resolve_col_motion(&mut self, key: &str, line: &str, buf: &VimBuffer, count: usize) -> Option<Motion> {
        let col = match key {
            "h" => (0..count).fold(buf.cursor_col, |c, _| prev_grapheme_start(line, c)),
            "l" => (0..count).fold(buf.cursor_col, |c, _| next_grapheme_start(line, c)),
            "0" => 0,
            "^" => first_non_blank(line),
            "$" => {
                self.desired = Some(Desired::Eol);
                line.len()
            }
            _ => return None,
        };
        Some(charwise_motion(pos(buf.cursor_line, col)))
    }

    fn resolve_word_motion(&self, key: &str, line: &str, buf: &VimBuffer, count: usize) -> Option<Motion> {
        match key {
            "w" => Some(self.word_forward_motion(line, buf, count)),
            "b" => {
                let col = (0..count).fold(buf.cursor_col, |c, _| move_word_left(line, c));
                Some(charwise_motion(pos(buf.cursor_line, col)))
            }
            "e" => Some(Motion {
                to: pos(
                    buf.cursor_line,
                    (0..count).fold(buf.cursor_col, |c, _| word_end(line, c)),
                ),
                inclusive: true,
                linewise: false,
            }),
            _ => None,
        }
    }

    fn resolve_structural_motion(&mut self, key: &str, buf: &VimBuffer, count: usize) -> Option<Motion> {
        match key {
            "j" | "k" => Some(self.vertical_motion(key, buf, count)),
            "%" => self.percent_motion(buf, count),
            "{" | "}" => Some(Self::paragraph_motion(key, buf, count)),
            "G" => Some(self.goto_line_motion(buf, count)),
            ";" | "," => self.repeat_find(key == ";", buf, count),
            _ => None,
        }
    }

    pub(super) fn resolve_motion(&mut self, key: &str, buf: &VimBuffer) -> Option<Motion> {
        let count = self.peek_count();
        let line = buf.line(buf.cursor_line);
        let key = if key == " " { "l" } else { key };
        if let Some(m) = self.resolve_col_motion(key, line, buf, count) {
            return Some(m);
        }
        if let Some(m) = self.resolve_word_motion(key, line, buf, count) {
            return Some(m);
        }
        self.resolve_structural_motion(key, buf, count)
    }

    fn repeat_find(&self, same: bool, buf: &VimBuffer, count: usize) -> Option<Motion> {
        let (spec, target) = self.last_find.clone()?;
        let forward = if same { spec.forward } else { !spec.forward };
        let text = buf.line(buf.cursor_line);
        let from = if spec.till {
            if forward {
                next_grapheme_start(text, buf.cursor_col)
            } else {
                prev_grapheme_start(text, buf.cursor_col)
            }
        } else {
            buf.cursor_col
        };
        self.seek(
            &target,
            buf,
            FindSpec {
                forward,
                till: spec.till,
            },
            count,
            from,
        )
    }

    pub(super) fn seek(
        &self,
        target: &str,
        buf: &VimBuffer,
        spec: FindSpec,
        count: usize,
        from: usize,
    ) -> Option<Motion> {
        let text = buf.line(buf.cursor_line);
        let m = find_grapheme(text, from, target, spec.forward, count)?;
        let land = match (spec.till, spec.forward) {
            (true, true) => prev_grapheme_start(text, m),
            (true, false) => next_grapheme_start(text, m),
            _ => m,
        };
        (land != buf.cursor_col).then_some(Motion {
            to: pos(buf.cursor_line, land),
            inclusive: true,
            linewise: false,
        })
    }
}
