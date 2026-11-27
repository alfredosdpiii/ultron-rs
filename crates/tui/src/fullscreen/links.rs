//! Links on a painted row: the OSC 8 hyperlink or plain-text URL under a column, the columns it covers, and the
//! underline the viewport draws under the one the pointer is over.

use unicode_segmentation::UnicodeSegmentation;

use crate::utils::{ansi_len, grapheme_width};

/// A piece of a row: an escape sequence, or a visible grapheme at a column with its width (tabs are 3 columns).
enum Piece<'a> {
    Code(&'a str),
    Cell { text: &'a str, col: usize, width: usize },
}

fn pieces(line: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut col = 0;
    let mut i = 0;
    while i < line.len() {
        if let Some(len) = ansi_len(line, i) {
            out.push(Piece::Code(&line[i..i + len]));
            i += len;
            continue;
        }
        let mut end = i;
        while end < line.len() && ansi_len(line, end).is_none() {
            end += line[end..].chars().next().map_or(1, char::len_utf8);
        }
        for text in line[i..end].graphemes(true) {
            let width = if text == "\t" { 3 } else { grapheme_width(text) };
            out.push(Piece::Cell { text, col, width });
            col += width;
        }
        i = end;
    }
    out
}

/// A visible grapheme of a row and the OSC 8 hyperlink open at it.
struct Cell<'a> {
    text: &'a str,
    col: usize,
    width: usize,
    link: Option<&'a str>,
}

fn cells(line: &str) -> Vec<Cell<'_>> {
    let mut active = None;
    let mut out = Vec::new();
    for piece in pieces(line) {
        match piece {
            Piece::Code(code) => {
                if let Some(url) = osc8_url(code) {
                    active = (!url.is_empty()).then_some(url);
                }
            }
            Piece::Cell { text, col, width } => out.push(Cell {
                text,
                col,
                width,
                link: active,
            }),
        }
    }
    out
}

/// `/^\x1b\]8;[^;]*;([^\x07\x1b]*)(?:\x07|\x1b\\)$/`: the URL of an OSC 8 sequence (empty closes the link).
fn osc8_url(code: &str) -> Option<&str> {
    let body = code.strip_prefix("\x1b]8;")?;
    let body = body.strip_suffix('\x07').or_else(|| body.strip_suffix("\x1b\\"))?;
    let (params, url) = body.split_once(';')?;
    (!params.contains(';') && !url.contains(['\x07', '\x1b'])).then_some(url)
}

/// A link shown on a row: what it opens and the visible columns `[start, end)` it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RowLink {
    pub(super) url: String,
    pub(super) start: usize,
    pub(super) end: usize,
}

fn cell_at(cells: &[Cell], column: usize) -> Option<usize> {
    cells.iter().position(|c| column >= c.col && column < c.col + c.width)
}

fn osc8_link(cells: &[Cell], column: usize) -> Option<RowLink> {
    let at = cell_at(cells, column)?;
    let url = cells[at].link?;
    let first = cells[..at]
        .iter()
        .rposition(|c| c.link != Some(url))
        .map_or(0, |p| p + 1);
    let last = cells[at..]
        .iter()
        .position(|c| c.link != Some(url))
        .map_or(cells.len(), |p| at + p);
    Some(RowLink {
        url: url.to_string(),
        start: cells[first].col,
        end: cells[last - 1].col + cells[last - 1].width,
    })
}

/// The `http://` or `https://` URL shown as plain text under `column`. Trailing punctuation and unbalanced closing
/// brackets end it, so `(see https://x.dev/a).` gives `https://x.dev/a`.
fn plain_url(cells: &[Cell], column: usize) -> Option<RowLink> {
    let at = cell_at(cells, column)?;
    let is_url_char = |g: &str| g.len() == 1 && g.as_bytes()[0].is_ascii_graphic() && !"<>\"'`".contains(g);
    if !is_url_char(cells[at].text) {
        return None;
    }
    let first = cells[..at]
        .iter()
        .rposition(|c| !is_url_char(c.text))
        .map_or(0, |p| p + 1);
    let last = cells[at..]
        .iter()
        .position(|c| !is_url_char(c.text))
        .map_or(cells.len(), |p| at + p);
    let token: String = cells[first..last].iter().map(|c| c.text).collect();
    let clicked = at - first;
    let start = (0..=clicked).rev().find(|&p| {
        let rest = token[p..].to_ascii_lowercase();
        rest.starts_with("https://") || rest.starts_with("http://")
    })?;
    let mut url = &token[start..];
    while let Some(tail) = url.chars().last() {
        let unbalanced = |open: char| url.matches(tail).count() > url.matches(open).count();
        let trim = match tail {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => unbalanced('('),
            ']' => unbalanced('['),
            '}' => unbalanced('{'),
            _ => false,
        };
        if !trim {
            break;
        }
        url = &url[..url.len() - 1];
    }
    let scheme_len = if url.to_ascii_lowercase().starts_with("https://") {
        8
    } else {
        7
    };
    (url.len() > scheme_len && clicked < start + url.len()).then(|| RowLink {
        url: url.to_string(),
        start: cells[first + start].col,
        end: cells[first + start].col + url.len(),
    })
}

/// The link under visible column `column` of `line`: its OSC 8 hyperlink, else with `plain` the http(s) URL shown
/// there as plain text (each of the URL's cells is one ASCII byte one column wide, so its byte offsets are columns).
pub(super) fn link_at(line: &str, column: usize, plain: bool) -> Option<RowLink> {
    let cells = cells(line);
    osc8_link(&cells, column).or_else(|| plain.then(|| plain_url(&cells, column)).flatten())
}

/// `getOsc8LinkAtColumn`: the OSC 8 hyperlink covering visible column `column` of `line` (tabs are 3 columns).
pub fn osc8_link_at_column(line: &str, column: usize) -> Option<String> {
    osc8_link(&cells(line), column).map(|link| link.url)
}

/// The `http://` or `https://` URL shown as plain text that covers visible column `column` of `line` (tabs are 3
/// columns). An Option/Alt click opens it where links reach the screen without OSC 8: tmux without its hyperlinks
/// feature, Terminal.app, a markdown link printed as `label (url)`, a URL in tool output. Trailing punctuation and
/// unbalanced closing brackets end the URL, so `(see https://x.dev/a).` gives `https://x.dev/a`.
pub fn plain_url_at_column(line: &str, column: usize) -> Option<String> {
    plain_url(&cells(line), column).map(|link| link.url)
}

/// `line` with its graphemes in columns `[start, end)` underlined. Only SGR 4 and 24 are added: every code of the
/// row stays where it was (OSC 8 links keep their extent), and SGR 4 follows each SGR code inside the span, since a
/// reset or an SGR 24 there would end the underline early.
pub(super) fn underline_columns(line: &str, start: usize, end: usize) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut open = false;
    for piece in pieces(line) {
        match piece {
            Piece::Code(code) => {
                out.push_str(code);
                if open && code.ends_with('m') {
                    out.push_str("\x1b[4m");
                }
            }
            Piece::Cell { text, col, width } => {
                if !open && col >= start && col < end {
                    out.push_str("\x1b[4m");
                    open = true;
                }
                out.push_str(text);
                if open && col + width >= end {
                    out.push_str("\x1b[24m");
                    open = false;
                }
            }
        }
    }
    if open {
        out.push_str("\x1b[24m");
    }
    out
}
