use super::motions::{flatten, offset_to_pos, quote_marks};
use super::{Range, VimBuffer, VimState, pos};

fn char_class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

struct Chunk {
    start: usize,
    end: usize,
    space: bool,
}

fn chunk_line(text: &str, big: bool) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    for (i, c) in text.char_indices() {
        let cls = char_class(c, big);
        match chunks.last_mut() {
            Some(last) if char_class(text[last.start..].chars().next().unwrap_or(' '), big) == cls => {
                last.end = i + c.len_utf8();
            }
            _ => chunks.push(Chunk {
                start: i,
                end: i + c.len_utf8(),
                space: cls == 0,
            }),
        }
    }
    chunks
}

fn around_word_bounds(chunks: &[Chunk], mut first: usize, count: usize, last: usize) -> (usize, usize) {
    if chunks[first].space {
        return (first, (first + 2 * count - 1).min(last));
    }
    let mut end = first;
    for n in 0..count {
        if n > 0 && end < last {
            end += 1;
        }
        if end < last && chunks[end + 1].space {
            end += 1;
        }
    }
    if !chunks[end].space && first > 0 && chunks[first - 1].space {
        first -= 1;
    }
    (first, end)
}

fn word_object(text: &str, col: usize, around: bool, big: bool, count: usize) -> Option<(usize, usize)> {
    let chunks = chunk_line(text, big);
    if chunks.is_empty() {
        return None;
    }
    let last = chunks.len() - 1;
    let first = chunks.iter().position(|c| col < c.end).unwrap_or(last);
    let (first, end) = if !around {
        (first, (first + count - 1).min(last))
    } else {
        around_word_bounds(&chunks, first, count, last)
    };
    Some((chunks[first].start, chunks[end].end))
}

fn quote_object(text: &str, col: usize, quote: char, around: bool) -> Option<(usize, usize)> {
    let marks = quote_marks(text, quote);
    for pair in marks.as_chunks::<2>().0 {
        let (open, close) = (pair[0], pair[1]);
        if col > close {
            continue;
        }
        if !around {
            return Some((open + 1, close));
        }
        let mut end = close + 1;
        let mut start = open;
        while let Some(c) = text[end..].chars().next().filter(|c| c.is_whitespace()) {
            end += c.len_utf8();
        }
        if end == close + 1 {
            while let Some(c) = text[..start].chars().next_back().filter(|c| c.is_whitespace()) {
                start -= c.len_utf8();
            }
        }
        return Some((start, end));
    }
    None
}

fn find_enclosing_open(b: &[u8], cursor: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = cursor.min(b.len() - 1) as isize;
    while i >= 0 {
        let ch = b[i as usize];
        if ch == close && i as usize != cursor {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                return Some(i as usize);
            }
            depth -= 1;
        }
        i -= 1;
    }
    None
}

fn find_matching_close(b: &[u8], open_at: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    for (j, &ch) in b.iter().enumerate().skip(open_at + 1) {
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                return Some(j);
            }
            depth -= 1;
        }
    }
    None
}

fn bracket_object(buf: &VimBuffer, open: u8, close: u8, around: bool) -> Option<Range> {
    let (text, cursor) = flatten(buf);
    let b = text.as_bytes();
    if b.is_empty() {
        return None;
    }
    let open_at = find_enclosing_open(b, cursor, open, close)?;
    let close_at = find_matching_close(b, open_at, open, close)?;
    Some(Range {
        from: offset_to_pos(buf.lines, if around { open_at } else { open_at + 1 }),
        to: offset_to_pos(buf.lines, if around { close_at + 1 } else { close_at }),
        linewise: false,
    })
}

fn is_blank_line(buf: &VimBuffer, l: usize) -> bool {
    buf.line(l).trim().is_empty()
}

fn expand_around_paragraph(buf: &VimBuffer, first: &mut usize, end: &mut usize, last: usize, target: bool) {
    let stop = *end;
    while *end < last && is_blank_line(buf, *end + 1) != target {
        *end += 1;
    }
    if *end == stop {
        while *first > 0 && is_blank_line(buf, *first - 1) != target {
            *first -= 1;
        }
    }
}

fn paragraph_object(buf: &VimBuffer, around: bool) -> Range {
    let last = buf.lines.len().saturating_sub(1);
    let target = is_blank_line(buf, buf.cursor_line);
    let (mut first, mut end) = (buf.cursor_line, buf.cursor_line);
    while first > 0 && is_blank_line(buf, first - 1) == target {
        first -= 1;
    }
    while end < last && is_blank_line(buf, end + 1) == target {
        end += 1;
    }
    if around {
        expand_around_paragraph(buf, &mut first, &mut end, last, target);
    }
    Range {
        from: pos(first, 0),
        to: pos(end, buf.line(end).len()),
        linewise: true,
    }
}

fn line_range(line_idx: usize, span: Option<(usize, usize)>) -> Option<Range> {
    let (a, b) = span?;
    Some(Range {
        from: pos(line_idx, a),
        to: pos(line_idx, b),
        linewise: false,
    })
}

impl VimState {
    pub(super) fn resolve_text_object(&self, key: &str, around: bool, buf: &VimBuffer, count: usize) -> Option<Range> {
        let line = buf.line(buf.cursor_line);
        match key {
            "w" | "W" => line_range(
                buf.cursor_line,
                word_object(line, buf.cursor_col, around, key == "W", count),
            ),
            "\"" | "'" | "`" => line_range(
                buf.cursor_line,
                quote_object(line, buf.cursor_col, key.chars().next()?, around),
            ),
            "(" | ")" | "b" => bracket_object(buf, b'(', b')', around),
            "[" | "]" => bracket_object(buf, b'[', b']', around),
            "{" | "}" | "B" => bracket_object(buf, b'{', b'}', around),
            "<" | ">" => bracket_object(buf, b'<', b'>', around),
            "p" => Some(paragraph_object(buf, around)),
            _ => None,
        }
    }
}
