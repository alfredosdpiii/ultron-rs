use super::Editor;
use super::wrap::{VisualLine, byte_of_u16, find_markers, find_visual_line_at};
use crate::editing::{find_word_backward, find_word_forward};
use jsrt::{string, utf16};

fn is_last_visual_line(vls: &[VisualLine], i: usize) -> bool {
    i == vls.len() - 1 || vls[i + 1].logical != vls[i].logical
}

fn next_visual_after_grapheme(vls: &[VisualLine], tgt_logical: usize, target: usize, seg_end: usize) -> Option<usize> {
    let mut next = target + 1;
    while next < vls.len() && vls[next].logical == tgt_logical && vls[next].start < seg_end {
        next += 1;
    }
    (next < vls.len()).then_some(next)
}

fn snap_marker_boundary(spans: &[(usize, usize)], pos: usize, forward: bool) -> Option<usize> {
    for &(s, e) in spans {
        if forward && s == pos {
            return Some(e);
        }
        if !forward && e == pos {
            return Some(s);
        }
    }
    None
}

impl Editor {
    fn visual_max_col(&self, vls: &[VisualLine], i: usize) -> usize {
        let vl = vls[i];
        let len = utf16::len(&self.lines[vl.logical][vl.start..vl.start + vl.len]);
        if is_last_visual_line(vls, i) {
            len
        } else {
            len.saturating_sub(1)
        }
    }

    fn current_visual_col(&self, vls: &[VisualLine], cur: VisualLine) -> usize {
        let cur_line = &self.lines[cur.logical];
        match self.snapped_from {
            Some(snap) => {
                let vl = vls[find_visual_line_at(vls, cur.logical, byte_of_u16(cur_line, snap))];
                snap - utf16::len(&cur_line[..vl.start])
            }
            None => utf16::len(&cur_line[cur.start..self.cursor_col]),
        }
    }

    fn place_cursor_u16(&mut self, vls: &[VisualLine], current: usize, target: usize, tgt_logical: usize, col: usize) {
        let line = self.lines[tgt_logical].clone();
        let tgt = vls[target];
        let mut seg_u16 = 0;
        for (index, seg) in self.graphemes(&line) {
            if seg_u16 > col {
                break;
            }
            let len = utf16::len(seg);
            if len > 1 && col < seg_u16 + len {
                if index < tgt.start
                    && target > current
                    && let Some(next) = next_visual_after_grapheme(vls, tgt_logical, target, index + seg.len())
                {
                    self.move_to_visual_line(vls, current, next);
                    return;
                }
                self.snapped_from = Some(col);
                self.cursor_col = index;
                return;
            }
            seg_u16 += len;
        }
        self.cursor_col = byte_of_u16(&line, col);
        self.snapped_from = None;
    }

    pub(super) fn move_to_visual_line(&mut self, vls: &[VisualLine], current: usize, target: usize) {
        let (Some(&cur), Some(&tgt)) = (vls.get(current), vls.get(target)) else {
            return;
        };
        let current_col = self.current_visual_col(vls, cur);
        let move_to = self.vertical_move_column(
            current_col,
            self.visual_max_col(vls, current),
            self.visual_max_col(vls, target),
        );
        self.cursor_line = tgt.logical;
        let line = &self.lines[tgt.logical];
        let col = (utf16::len(&line[..tgt.start]) + move_to).min(utf16::len(line));
        self.place_cursor_u16(vls, current, target, tgt.logical, col);
    }

    /// Sticky-column decision table of `computeVerticalMoveColumn`.
    fn vertical_move_column(&mut self, current: usize, source_max: usize, target_max: usize) -> usize {
        let target_too_short = target_max < current;
        match self.preferred_visual_col {
            Some(preferred) if current >= source_max => {
                if target_too_short || target_max < preferred {
                    target_max
                } else {
                    self.preferred_visual_col = None;
                    preferred
                }
            }
            _ if target_too_short => {
                self.preferred_visual_col = Some(current);
                target_max
            }
            _ => {
                self.preferred_visual_col = None;
                current
            }
        }
    }

    fn move_cursor_right(&mut self, vls: &[VisualLine], current: usize) {
        let line = self.line().to_string();
        if self.cursor_col < line.len() {
            let len = self
                .graphemes(&line[self.cursor_col..])
                .first()
                .map_or(1, |g| g.1.len());
            self.set_cursor_col(self.cursor_col + len);
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.set_cursor_col(0);
        } else if let Some(vl) = vls.get(current) {
            self.preferred_visual_col = Some(utf16::len(&line[vl.start..self.cursor_col]));
        }
    }

    fn move_cursor_left(&mut self) {
        let line = self.line().to_string();
        if self.cursor_col > 0 {
            let len = self.graphemes(&line[..self.cursor_col]).last().map_or(1, |g| g.1.len());
            self.set_cursor_col(self.cursor_col - len);
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.set_cursor_col(self.line().len());
        }
    }

    pub(super) fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let (current, vls) = self.current_visual_line();
        if delta_line != 0 {
            let target = current as isize + delta_line;
            if target >= 0 && (target as usize) < vls.len() {
                self.move_to_visual_line(&vls, current, target as usize);
            }
        }
        if delta_col > 0 {
            self.move_cursor_right(&vls, current);
        } else if delta_col < 0 {
            self.move_cursor_left();
        }
        if self.picker.is_some() {
            self.update_autocomplete();
        }
    }

    pub(super) fn page_scroll(&mut self, direction: isize) {
        self.last_action = None;
        let page = self.max_visible_lines() as isize;
        let (current, vls) = self.current_visual_line();
        let target = (current as isize + direction * page).clamp(0, vls.len() as isize - 1) as usize;
        self.move_to_visual_line(&vls, current, target);
    }

    /// Valid paste-marker spans of the cursor line.
    fn marker_spans(&self) -> Vec<(usize, usize)> {
        let ids = self.valid_ids();
        find_markers(self.line())
            .into_iter()
            .filter(|m| m.id.parse::<u64>().is_ok_and(|id| ids.contains(&id)))
            .map(|m| (m.start, m.end))
            .collect()
    }

    fn word_target(&self, forward: bool) -> usize {
        let line = self.line();
        let spans = self.marker_spans();
        let edge = if forward {
            let rest = &line[self.cursor_col..];
            self.cursor_col + rest.len() - rest.trim_start_matches(string::is_space).len()
        } else {
            line[..self.cursor_col].trim_end_matches(string::is_space).len()
        };
        if let Some(snapped) = snap_marker_boundary(&spans, edge, forward) {
            return snapped;
        }
        let r = if forward {
            find_word_forward(line, self.cursor_col)
        } else {
            find_word_backward(line, self.cursor_col)
        };
        for &(s, e) in &spans {
            if s < r && r < e {
                return if forward { s } else { e };
            }
        }
        r
    }

    /// `findWordBackward` with paste markers as atomic segments.
    pub(super) fn word_backward_target(&self) -> usize {
        self.word_target(false)
    }

    /// `findWordForward` with paste markers as atomic segments.
    pub(super) fn word_forward_target(&self) -> usize {
        self.word_target(true)
    }

    pub(super) fn move_word_backward(&mut self) {
        self.last_action = None;
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.set_cursor_col(self.line().len());
            }
            return;
        }
        let target = self.word_backward_target();
        self.set_cursor_col(target);
    }

    pub(super) fn move_word_forward(&mut self) {
        self.last_action = None;
        if self.cursor_col >= self.line().len() {
            if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }
        let target = self.word_forward_target();
        self.set_cursor_col(target);
    }

    /// Jump to the next (or previous) occurrence of `ch`, across lines, skipping the cursor position.
    pub(super) fn jump_to_char(&mut self, ch: &str, forward: bool) {
        self.last_action = None;
        let n = self.lines.len();
        let mut idx = self.cursor_line;
        loop {
            let line = &self.lines[idx];
            let found = if idx != self.cursor_line {
                if forward { line.find(ch) } else { line.rfind(ch) }
            } else if forward {
                line.match_indices(ch).map(|(i, _)| i).find(|&i| i > self.cursor_col)
            } else {
                line.match_indices(ch)
                    .map(|(i, _)| i)
                    .filter(|&i| i < self.cursor_col || i == 0)
                    .last()
            };
            if let Some(i) = found {
                self.cursor_line = idx;
                self.set_cursor_col(i);
                return;
            }
            match (forward, idx) {
                (true, i) if i + 1 < n => idx += 1,
                (false, i) if i > 0 => idx -= 1,
                _ => return,
            }
        }
    }
}
