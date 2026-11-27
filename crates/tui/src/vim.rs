//! Modal editing state machine (port of omp packages/tui/src/vim.ts).
//!
//! Pure: it reads a snapshot of the buffer and returns the edits it wants applied, so the editor keeps sole
//! ownership of undo, atomic placeholder tokens and change notification ([`crate::vim_editor`] applies them).
//! Columns are UTF-8 byte offsets (omp uses UTF-16 offsets; positions map one to one on grapheme boundaries).

use unicode_segmentation::UnicodeSegmentation;

mod actions;
mod motions;
mod objects;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimMode {
    Insert,
    Normal,
    Visual,
    VisualLine,
    Replace,
}

impl VimMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Normal => "normal",
            Self::Visual => "visual",
            Self::VisualLine => "visual-line",
            Self::Replace => "replace",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

pub const fn pos(line: usize, col: usize) -> Pos {
    Pos { line, col }
}

/// Read-only view of the buffer.
#[derive(Clone, Copy, Debug)]
pub struct VimBuffer<'a> {
    pub lines: &'a [String],
    pub cursor_line: usize,
    pub cursor_col: usize,
}

impl VimBuffer<'_> {
    pub(super) fn line(&self, i: usize) -> &str {
        self.lines.get(i).map_or("", String::as_str)
    }
    pub(super) fn cursor(&self) -> Pos {
        pos(self.cursor_line, self.cursor_col)
    }
}

/// An edit for the editor to apply. `to` is always an exclusive end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VimCommand {
    Move {
        to: Pos,
    },
    Mode {
        mode: VimMode,
    },
    Yank {
        from: Pos,
        to: Pos,
        linewise: bool,
    },
    Delete {
        from: Pos,
        to: Pos,
        linewise: bool,
        insert: bool,
    },
    OpenLine {
        below: bool,
    },
    Paste {
        after: bool,
        count: usize,
    },
    Undo,
    Replace {
        from: Pos,
        to: Pos,
        text: String,
    },
    Join {
        from_line: usize,
        lines: usize,
    },
    Indent {
        from_line: usize,
        to_line: usize,
        out: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operator {
    Delete,
    Yank,
    Change,
    IndentIn,
    IndentOut,
}

impl Operator {
    fn key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Yank => 'y',
            Self::Change => 'c',
            Self::IndentIn => '>',
            Self::IndentOut => '<',
        }
    }
    fn from_key(k: &str) -> Option<Self> {
        Some(match k {
            "d" => Self::Delete,
            "y" => Self::Yank,
            "c" => Self::Change,
            ">" => Self::IndentIn,
            "<" => Self::IndentOut,
            _ => return None,
        })
    }
}

/// Start of the grapheme after `col`, clamped to the line length.
pub fn next_grapheme_start(text: &str, col: usize) -> usize {
    if col >= text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .find(|(i, _)| *i >= col)
        .map_or(text.len(), |(i, g)| (i + g.len()).min(text.len()))
}

/// Start of the grapheme before `col`, clamped to 0.
pub fn prev_grapheme_start(text: &str, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    text.grapheme_indices(true)
        .take_while(|(i, _)| *i < col)
        .last()
        .map_or(0, |(i, _)| i)
}

/// Start of the final grapheme: where a Normal-mode cursor rests.
pub fn last_grapheme_start(text: &str) -> usize {
    prev_grapheme_start(text, text.len())
}

fn first_non_blank(text: &str) -> usize {
    text.grapheme_indices(true)
        .find(|(_, g)| !g.trim().is_empty())
        .map_or(0, |(i, _)| i)
}

fn is_space_at(text: &str, i: usize) -> bool {
    text[i..].chars().next().is_some_and(char::is_whitespace)
}

#[derive(Clone, Copy)]
struct Motion {
    to: Pos,
    inclusive: bool,
    linewise: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FindSpec {
    forward: bool,
    till: bool,
}

/// Desired column for `j`/`k`; `Eol` is `$`'s sticky end of line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Desired {
    Col(usize),
    Eol,
}

struct Range {
    from: Pos,
    to: Pos,
    linewise: bool,
}

/// The modal state.
#[derive(Clone)]
pub struct VimState {
    pub mode: VimMode,
    /// Fixed end of a Visual selection; the cursor is the moving end.
    pub anchor: Option<Pos>,
    count: String,
    operator: Option<Operator>,
    pending_g: bool,
    text_object: Option<char>,
    find_pending: Option<FindSpec>,
    last_find: Option<(FindSpec, String)>,
    replace_char: bool,
    desired: Option<Desired>,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            mode: VimMode::Normal,
            anchor: None,
            count: String::new(),
            operator: None,
            pending_g: false,
            text_object: None,
            find_pending: None,
            last_find: None,
            replace_char: false,
            desired: None,
        }
    }
}

impl VimState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A count, operator, `g`, text object, find or `r` prefix is half-typed.
    pub fn pending(&self) -> bool {
        !self.count.is_empty()
            || self.operator.is_some()
            || self.pending_g
            || self.text_object.is_some()
            || self.find_pending.is_some()
            || self.replace_char
    }

    /// The half-typed command as Vim echoes it (`2d`, `di`, `df`, `r`).
    pub fn pending_text(&self) -> String {
        let find = match self.find_pending {
            None => "",
            Some(FindSpec {
                forward: true,
                till: true,
            }) => "t",
            Some(FindSpec {
                forward: false,
                till: true,
            }) => "T",
            Some(FindSpec {
                forward: true,
                till: false,
            }) => "f",
            Some(FindSpec {
                forward: false,
                till: false,
            }) => "F",
        };
        format!(
            "{}{}{}{}{}{}",
            self.count,
            self.operator.map_or_default(|o| o.key().to_string()),
            if self.pending_g { "g" } else { "" },
            self.text_object.map_or_default(String::from),
            find,
            if self.replace_char { "r" } else { "" }
        )
    }

    pub fn visual(&self) -> bool {
        matches!(self.mode, VimMode::Visual | VimMode::VisualLine)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn clear_pending(&mut self) {
        self.count.clear();
        self.operator = None;
        self.pending_g = false;
        self.text_object = None;
        self.find_pending = None;
        self.replace_char = false;
    }

    fn peek_count(&self) -> usize {
        self.count.parse::<usize>().unwrap_or(1)
    }

    fn take_count(&mut self) -> usize {
        let c = self.peek_count().max(1);
        self.count.clear();
        c
    }

    fn clamp_normal(buf: &VimBuffer, p: Pos) -> Pos {
        let line = p.line.min(buf.lines.len().saturating_sub(1));
        pos(line, p.col.min(last_grapheme_start(buf.line(line))))
    }
}

/// Normalized, end-exclusive span of a Visual selection (charwise includes the grapheme under the cursor).
pub fn visual_range(buf: &VimBuffer, anchor: Pos, linewise: bool) -> (Pos, Pos) {
    let (mut from, mut to) = (anchor, buf.cursor());
    if to < from {
        std::mem::swap(&mut from, &mut to);
    }
    if linewise {
        let last = to.line.min(buf.lines.len().saturating_sub(1));
        return (pos(from.line, 0), pos(last, buf.line(last).len()));
    }
    (from, pos(to.line, next_grapheme_start(buf.line(to.line), to.col)))
}
