use unicode_segmentation::UnicodeSegmentation;

use super::{
    FindSpec, Motion, Operator, Pos, VimBuffer, VimCommand, VimMode, VimState, first_non_blank, last_grapheme_start,
    next_grapheme_start, pos, prev_grapheme_start, visual_range,
};

impl VimState {
    fn handle_find_pending(&mut self, key: &str, buf: &VimBuffer, spec: FindSpec) -> Vec<VimCommand> {
        self.desired = None;
        self.last_find = Some((spec, key.to_string()));
        let count = self.peek_count().max(1);
        match self.seek(key, buf, spec, count, buf.cursor_col) {
            None => {
                self.operator = None;
                self.count.clear();
                vec![]
            }
            Some(m) => self.apply_motion(buf, m),
        }
    }

    fn handle_replace_char(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        self.replace_char = false;
        let count = self.peek_count().max(1);
        let line = buf.line(buf.cursor_line);
        let end = line
            .grapheme_indices(true)
            .filter(|(i, _)| *i >= buf.cursor_col)
            .nth(count - 1)
            .map(|(i, g)| i + g.len());
        self.count.clear();
        let Some(end) = end else { return vec![] };
        vec![VimCommand::Replace {
            from: buf.cursor(),
            to: pos(buf.cursor_line, end),
            text: key.repeat(count),
        }]
    }

    fn handle_count_digit(&mut self, key: &str) -> Option<Vec<VimCommand>> {
        let is_digit = key.len() == 1 && key.as_bytes()[0].is_ascii_digit();
        if is_digit && (key != "0" || !self.count.is_empty()) {
            self.count.push_str(key);
            return Some(vec![]);
        }
        None
    }

    fn handle_pending_g(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        self.pending_g = false;
        if key != "g" {
            self.clear_pending();
            return vec![];
        }
        self.desired = None;
        let line = (self.take_count() - 1).min(buf.lines.len().saturating_sub(1));
        self.apply_motion(
            buf,
            Motion {
                to: pos(line, 0),
                inclusive: false,
                linewise: true,
            },
        )
    }

    fn start_find_prefix(&mut self, key: &str) -> Vec<VimCommand> {
        self.desired = None;
        self.find_pending = Some(FindSpec {
            forward: key == "f" || key == "t",
            till: key == "t" || key == "T",
        });
        vec![]
    }

    fn start_replace_prefix(&mut self, key: &str) -> Vec<VimCommand> {
        self.desired = None;
        if key == "R" {
            self.take_count();
            self.mode = VimMode::Replace;
            return vec![VimCommand::Mode { mode: VimMode::Replace }];
        }
        self.replace_char = true;
        vec![]
    }

    fn handle_prefix_key(&mut self, key: &str) -> Option<Vec<VimCommand>> {
        if (key == "i" || key == "a") && (self.operator.is_some() || self.visual()) {
            self.text_object = key.chars().next();
            return Some(vec![]);
        }
        if matches!(key, "f" | "F" | "t" | "T") {
            return Some(self.start_find_prefix(key));
        }
        if (key == "r" || key == "R") && self.operator.is_none() && !self.visual() {
            return Some(self.start_replace_prefix(key));
        }
        None
    }

    fn handle_pending_input(&mut self, key: &str, buf: &VimBuffer) -> Option<Vec<VimCommand>> {
        if let Some(spec) = self.find_pending.take() {
            return Some(self.handle_find_pending(key, buf, spec));
        }
        if self.replace_char {
            return Some(self.handle_replace_char(key, buf));
        }
        if let Some(cmds) = self.handle_count_digit(key) {
            return Some(cmds);
        }
        if self.pending_g {
            return Some(self.handle_pending_g(key, buf));
        }
        if let Some(kind) = self.text_object {
            self.desired = None;
            return Some(self.apply_text_object(kind, key, buf));
        }
        self.handle_prefix_key(key)
    }

    /// Handle one key: `"escape"` or a single grapheme. `None` when the key is not Vim's (the editor handles it).
    pub fn handle_key(&mut self, key: &str, buf: &VimBuffer) -> Option<Vec<VimCommand>> {
        if key == "escape" {
            return self.handle_escape(buf);
        }
        if self.mode == VimMode::Insert {
            return None;
        }
        if let Some(cmds) = self.handle_pending_input(key, buf) {
            return Some(cmds);
        }
        if key != "j" && key != "k" {
            self.desired = None;
        }
        if let Some(m) = self.resolve_motion(key, buf) {
            return Some(self.apply_motion(buf, m));
        }
        if self.visual() {
            Some(self.handle_visual_key(key, buf))
        } else {
            Some(self.handle_normal_key(key, buf))
        }
    }

    fn handle_escape(&mut self, buf: &VimBuffer) -> Option<Vec<VimCommand>> {
        if self.pending() {
            self.clear_pending();
            return Some(vec![]);
        }
        if self.visual() {
            self.mode = VimMode::Normal;
            self.anchor = None;
            return Some(vec![
                VimCommand::Mode { mode: VimMode::Normal },
                VimCommand::Move {
                    to: Self::clamp_normal(buf, buf.cursor()),
                },
            ]);
        }
        if matches!(self.mode, VimMode::Insert | VimMode::Replace) {
            self.mode = VimMode::Normal;
            let text = buf.line(buf.cursor_line);
            return Some(vec![
                VimCommand::Mode { mode: VimMode::Normal },
                VimCommand::Move {
                    to: pos(buf.cursor_line, prev_grapheme_start(text, buf.cursor_col)),
                },
            ]);
        }
        None
    }

    fn apply_motion(&mut self, buf: &VimBuffer, m: Motion) -> Vec<VimCommand> {
        let op = self.operator.take();
        self.take_count();
        let to_line = m.to.line.min(buf.lines.len().saturating_sub(1));
        let to = pos(to_line, m.to.col.min(buf.line(to_line).len()));
        if let Some(op @ (Operator::IndentIn | Operator::IndentOut)) = op {
            return vec![VimCommand::Indent {
                from_line: buf.cursor_line.min(m.to.line),
                to_line: buf.cursor_line.max(m.to.line),
                out: op == Operator::IndentOut,
            }];
        }
        let Some(op) = op else {
            let to = if self.visual() || self.mode == VimMode::Replace {
                to
            } else {
                Self::clamp_normal(buf, to)
            };
            return vec![VimCommand::Move { to }];
        };
        let (mut start, mut end) = (buf.cursor(), m.to);
        if end < start {
            std::mem::swap(&mut start, &mut end);
        }
        if m.inclusive {
            end = pos(end.line, next_grapheme_start(buf.line(end.line), end.col));
        }
        self.operate(op, start, end, m.linewise)
    }

    fn operate(&mut self, op: Operator, from: Pos, to: Pos, linewise: bool) -> Vec<VimCommand> {
        match op {
            Operator::IndentIn | Operator::IndentOut => vec![VimCommand::Indent {
                from_line: from.line.min(to.line),
                to_line: from.line.max(to.line),
                out: op == Operator::IndentOut,
            }],
            Operator::Yank => vec![VimCommand::Yank { from, to, linewise }, VimCommand::Move { to: from }],
            Operator::Delete => vec![VimCommand::Delete {
                from,
                to,
                linewise,
                insert: false,
            }],
            Operator::Change => {
                self.mode = VimMode::Insert;
                vec![
                    VimCommand::Move { to: from },
                    VimCommand::Delete {
                        from,
                        to,
                        linewise: false,
                        insert: true,
                    },
                    VimCommand::Mode { mode: VimMode::Insert },
                ]
            }
        }
    }

    fn apply_text_object(&mut self, kind: char, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let around = kind == 'a';
        let op = self.operator.take();
        self.text_object = None;
        let count = self.take_count();
        let Some(range) = self.resolve_text_object(key, around, buf, count) else {
            return vec![];
        };
        if let Some(op) = op {
            return self.operate(op, range.from, range.to, range.linewise);
        }
        self.anchor = Some(range.from);
        let mut cmds = Vec::new();
        if range.linewise && self.mode == VimMode::Visual {
            self.mode = VimMode::VisualLine;
            cmds.push(VimCommand::Mode { mode: self.mode });
        }
        let to = if range.to.col > 0 {
            pos(
                range.to.line,
                prev_grapheme_start(buf.line(range.to.line), range.to.col),
            )
        } else if range.to.line > range.from.line {
            pos(range.to.line - 1, last_grapheme_start(buf.line(range.to.line - 1)))
        } else {
            range.from
        };
        cmds.push(VimCommand::Move { to });
        cmds
    }

    fn enter_insert(&mut self) {
        self.take_count();
        self.mode = VimMode::Insert;
    }

    fn enter_insert_col(key: &str, line: &str, cursor_col: usize) -> usize {
        match key {
            "a" => next_grapheme_start(line, cursor_col),
            "I" => first_non_blank(line),
            _ => line.len(),
        }
    }

    fn delete_chars_forward(&mut self, line: &str, buf: &VimBuffer, count: usize) -> Vec<VimCommand> {
        self.take_count();
        let mut col = buf.cursor_col;
        for _ in 0..count {
            if col >= line.len() {
                break;
            }
            col = next_grapheme_start(line, col);
        }
        if col == buf.cursor_col {
            return vec![];
        }
        vec![VimCommand::Delete {
            from: buf.cursor(),
            to: pos(buf.cursor_line, col),
            linewise: false,
            insert: false,
        }]
    }

    fn double_line_span(&mut self, buf: &VimBuffer) -> (usize, usize) {
        let span = self.take_count();
        let last = (buf.cursor_line + span - 1).min(buf.lines.len().saturating_sub(1));
        self.operator = None;
        (buf.cursor_line, last)
    }

    fn line_operator(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let op = Operator::from_key(key).unwrap_or(Operator::Delete);
        if self.operator == Some(op) {
            let (first, last) = self.double_line_span(buf);
            return self.operate(op, pos(first, 0), pos(last, buf.line(last).len()), true);
        }
        self.operator = Some(op);
        vec![]
    }

    fn indent_operator(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let op = Operator::from_key(key).unwrap_or(Operator::IndentIn);
        if self.operator == Some(op) {
            let (from_line, to_line) = self.double_line_span(buf);
            return vec![VimCommand::Indent {
                from_line,
                to_line,
                out: key == "<",
            }];
        }
        if self.operator.is_some() {
            self.clear_pending();
            return vec![];
        }
        self.operator = Some(op);
        vec![]
    }

    fn handle_normal_insert_or_mode(&mut self, key: &str, buf: &VimBuffer) -> Option<Vec<VimCommand>> {
        let line = buf.line(buf.cursor_line);
        match key {
            "g" => {
                self.pending_g = true;
                Some(vec![])
            }
            "i" => {
                self.enter_insert();
                Some(vec![VimCommand::Mode { mode: VimMode::Insert }])
            }
            "a" | "I" | "A" => {
                self.enter_insert();
                let col = Self::enter_insert_col(key, line, buf.cursor_col);
                Some(vec![
                    VimCommand::Mode { mode: VimMode::Insert },
                    VimCommand::Move {
                        to: pos(buf.cursor_line, col),
                    },
                ])
            }
            "o" | "O" => {
                self.enter_insert();
                Some(vec![
                    VimCommand::OpenLine { below: key == "o" },
                    VimCommand::Mode { mode: VimMode::Insert },
                ])
            }
            "v" | "V" => {
                self.take_count();
                self.mode = if key == "v" {
                    VimMode::Visual
                } else {
                    VimMode::VisualLine
                };
                self.anchor = Some(buf.cursor());
                Some(vec![VimCommand::Mode { mode: self.mode }])
            }
            _ => None,
        }
    }

    fn handle_normal_edit(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let line = buf.line(buf.cursor_line);
        let count = self.peek_count();
        match key {
            "x" => self.delete_chars_forward(line, buf, count),
            "D" | "C" => {
                let span = self.take_count();
                let last = (buf.cursor_line + span - 1).min(buf.lines.len().saturating_sub(1));
                let op = if key == "C" { Operator::Change } else { Operator::Delete };
                self.operate(op, buf.cursor(), pos(last, buf.line(last).len()), false)
            }
            "d" | "y" | "c" => self.line_operator(key, buf),
            "p" | "P" => vec![VimCommand::Paste {
                after: key == "p",
                count: self.take_count(),
            }],
            "u" => {
                self.take_count();
                vec![VimCommand::Undo]
            }
            "J" => {
                if self.operator.is_some() {
                    self.clear_pending();
                    return vec![];
                }
                vec![VimCommand::Join {
                    from_line: buf.cursor_line,
                    lines: self.take_count().max(2),
                }]
            }
            ">" | "<" => self.indent_operator(key, buf),
            _ => {
                self.clear_pending();
                vec![]
            }
        }
    }

    fn handle_normal_key(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        if let Some(cmds) = self.handle_normal_insert_or_mode(key, buf) {
            return cmds;
        }
        self.handle_normal_edit(key, buf)
    }

    fn toggle_visual_mode(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let next = if key == "v" {
            VimMode::Visual
        } else {
            VimMode::VisualLine
        };
        if self.mode == next {
            self.mode = VimMode::Normal;
            self.anchor = None;
            return vec![
                VimCommand::Mode { mode: VimMode::Normal },
                VimCommand::Move {
                    to: Self::clamp_normal(buf, buf.cursor()),
                },
            ];
        }
        self.mode = next;
        vec![VimCommand::Mode { mode: next }]
    }

    fn visual_operate(&mut self, key: &str, buf: &VimBuffer, anchor: Pos, linewise: bool) -> Vec<VimCommand> {
        let op = match key {
            "y" => Operator::Yank,
            "c" | "s" => Operator::Change,
            _ => Operator::Delete,
        };
        let (from, to) = visual_range(buf, anchor, linewise);
        self.clear_pending();
        self.anchor = None;
        if op != Operator::Change {
            self.mode = VimMode::Normal;
        }
        let mut cmds = self.operate(op, from, to, linewise);
        if op != Operator::Change {
            cmds.push(VimCommand::Mode { mode: VimMode::Normal });
        }
        cmds
    }

    fn visual_line_action(&mut self, key: &str, buf: &VimBuffer, anchor: Pos, linewise: bool) -> Vec<VimCommand> {
        let (from, to) = visual_range(buf, anchor, linewise);
        self.clear_pending();
        self.anchor = None;
        self.mode = VimMode::Normal;
        let edit = if key == "J" {
            VimCommand::Join {
                from_line: from.line,
                lines: (to.line - from.line + 1).max(2),
            }
        } else {
            VimCommand::Indent {
                from_line: from.line,
                to_line: to.line,
                out: key == "<",
            }
        };
        vec![edit, VimCommand::Mode { mode: VimMode::Normal }]
    }

    fn handle_visual_key(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let anchor = self.anchor.unwrap_or(buf.cursor());
        let linewise = self.mode == VimMode::VisualLine;
        match key {
            "v" | "V" => self.toggle_visual_mode(key, buf),
            "o" => {
                self.anchor = Some(buf.cursor());
                vec![VimCommand::Move { to: anchor }]
            }
            "y" | "d" | "x" | "c" | "s" => self.visual_operate(key, buf, anchor, linewise),
            ">" | "<" | "J" => self.visual_line_action(key, buf, anchor, linewise),
            _ => {
                self.clear_pending();
                vec![]
            }
        }
    }
}
