use std::time::Instant;

use crate::frame::is_image_line;
use crate::mouse::MouseEventType;
use crate::scroll_view::{grapheme_cell_range, slice_by_column};
use crate::utils::{extract_ansi_code, strip_terminal_sequences, visible_width};
use jsrt::string;

use super::{
    AUTO_SCROLL_INTERVAL, Click, DOUBLE_CLICK_INTERVAL, FullscreenViewport, Granularity, MouseTargets, Point, Range,
    ScreenPoint, Sgr, ViewportInput, links,
};

fn is_primary_or_release(ev: Sgr) -> bool {
    let button = ev.button & 3;
    button == 0 || (ev.release && button == 3)
}

fn is_anchor_click(anchor: Option<Point>, point: Point, dragged: bool) -> bool {
    !dragged && anchor.is_some_and(|a| a.scroll == point.scroll && a.row == point.row && a.col == point.col)
}

fn press_granularity(word: Option<Range>, line: Range, count: u32) -> (Option<Range>, Granularity) {
    match count {
        2 => (
            word,
            if word.is_some() {
                Granularity::Word
            } else {
                Granularity::Character
            },
        ),
        3 => (Some(line), Granularity::Line),
        _ => (None, Granularity::Character),
    }
}

impl FullscreenViewport {
    /// `getActiveSelectionText`.
    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.bounds()?;
        let source: &[String] = if start.scroll {
            self.rect?;
            &self.content
        } else {
            &self.screen
        };
        let mut lines = Vec::new();
        for row in start.row..=end.row {
            let line = source.get(row).map_or("", String::as_str);
            let (a, b) = selection_columns(
                line,
                row as i64,
                to_screen(start, 0),
                to_screen(end, 0),
                0,
                visible_width(line),
            );
            let text = slice_by_column(line, a, b.saturating_sub(a));
            lines.push(
                strip_terminal_sequences(&text)
                    .trim_end_matches(string::is_space)
                    .to_string(),
            );
        }
        let text = lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    pub(super) fn clear_text_selection(&mut self) {
        self.stop_auto_scroll();
        self.press_active = false;
        self.anchor = None;
        self.focus = None;
        self.granularity = Granularity::Character;
        self.initial = None;
        self.pressed_url = None;
        self.dragged = false;
    }

    /// `getScrollSelectionPoint`.
    fn scroll_point(&self, x: i64, y: i64) -> Option<Point> {
        let rect = self.rect?;
        if rect.height == 0 {
            return None;
        }
        let top = rect.y as i64;
        let bottom = (self.rows as i64 - 1).min((rect.y + rect.height) as i64 - 1);
        if bottom < top {
            return None;
        }
        let pointer = y.clamp(top, bottom);
        let max_row = self.content.len().max(1) as i64 - 1;
        Some(Point {
            row: (self.view.scroll_top() as i64 + pointer - top).clamp(0, max_row) as usize,
            col: (x - rect.x as i64).clamp(0, rect.width as i64 - 1) as usize,
            scroll: true,
            boundary: false,
        })
    }

    /// `getSelectionPoint`.
    fn point(&self, x: i64, y: i64, scroll: bool) -> Point {
        if scroll && let Some(p) = self.scroll_point(x, y) {
            return p;
        }
        Point {
            row: y.clamp(0, (self.rows as i64 - 1).max(0)) as usize,
            col: x.clamp(0, (self.cols as i64 - 1).max(0)) as usize,
            scroll: false,
            boundary: false,
        }
    }

    fn source_line(&self, p: Point) -> &str {
        let lines = if p.scroll && self.rect.is_some() {
            &self.content
        } else {
            &self.screen
        };
        lines.get(p.row).map_or("", String::as_str)
    }

    /// `getWordSelection`: the word (paths and kebab-case kept whole) under `p`.
    fn word(&self, p: Point) -> Option<Range> {
        let (from, to) = word_selection(&strip_terminal_sequences(self.source_line(p)), p.col)?;
        Some(Range {
            start: Point { col: from, ..p },
            end: Point {
                col: to,
                boundary: true,
                ..p
            },
        })
    }

    /// `getLineSelection`.
    fn line(&self, p: Point) -> Range {
        Range {
            start: Point { col: 0, ..p },
            end: Point {
                col: visible_width(self.source_line(p)),
                boundary: true,
                ..p
            },
        }
    }

    /// `updateSelectionFocus`.
    fn update_focus(&mut self, p: Point) {
        let Some(initial) = self.initial.filter(|_| self.granularity != Granularity::Character) else {
            self.focus = Some(p);
            return;
        };
        let range = if self.granularity == Granularity::Word {
            self.word(p)
        } else {
            Some(self.line(p))
        };
        let Some(range) = range else {
            return;
        };
        let before = range.start.row < initial.start.row
            || (range.start.row == initial.start.row && range.start.col < initial.start.col);
        if before {
            self.anchor = Some(initial.end);
            self.focus = Some(range.start);
        } else {
            self.anchor = Some(initial.start);
            self.focus = Some(range.end);
        }
    }

    fn click_count(&mut self, p: Point, word: Option<Range>, now: Instant) -> u32 {
        let count = match (word, self.last_click) {
            (Some(w), Some(prev))
                if now.saturating_duration_since(prev.at) <= DOUBLE_CLICK_INTERVAL
                    && prev.row == p.row
                    && prev.scroll == p.scroll
                    && prev.word_start == w.start.col
                    && prev.word_end == w.end.col =>
            {
                prev.count % 3 + 1
            }
            _ => 1,
        };
        self.last_click = word.map(|w| Click {
            at: now,
            count,
            row: p.row,
            scroll: p.scroll,
            word_start: w.start.col,
            word_end: w.end.col,
        });
        count
    }

    /// `updateSelectionAutoScroll`.
    fn update_auto_scroll(&mut self, ev: Sgr, now: Instant) {
        let scroll = self.anchor.is_some_and(|a| a.scroll);
        let Some(rect) = self.rect.filter(|r| scroll && r.height > 0) else {
            self.stop_auto_scroll();
            return;
        };
        let top = rect.y as i64;
        let bottom = (self.rows as i64 - 1).min((rect.y + rect.height) as i64 - 1);
        self.drag_pointer = Some((ev.x, ev.y));
        self.auto_direction = if ev.y <= top { -1 } else { i64::from(ev.y >= bottom) };
        if self.auto_direction == 0 {
            self.stop_auto_scroll();
            return;
        }
        if self.auto_next.is_none() {
            self.auto_next = Some(now + AUTO_SCROLL_INTERVAL);
        }
    }

    /// One tick of the held drag's auto-scroll (`autoScrollSelection`). True when it scrolled.
    pub fn auto_scroll_tick(&mut self) -> bool {
        let scroll = self.anchor.is_some_and(|a| a.scroll);
        let (Some((x, y)), true) = (self.drag_pointer, scroll) else {
            self.stop_auto_scroll();
            return false;
        };
        let direction = self.auto_direction;
        if direction == 0 {
            self.stop_auto_scroll();
            return false;
        }
        if self.view.scroll_by(direction) == direction {
            self.stop_auto_scroll();
            return false;
        }
        if let Some(p) = self.scroll_point(x, y) {
            self.update_focus(p);
        }
        true
    }

    /// A held drag is auto-scrolling (its interval runs).
    pub fn is_auto_scrolling(&self) -> bool {
        self.auto_next.is_some()
    }

    pub(super) fn stop_auto_scroll(&mut self) {
        self.auto_next = None;
        self.auto_direction = 0;
        self.drag_pointer = None;
    }

    fn open_clicked_url(&mut self, is_click: bool) -> bool {
        let clicked = self.pressed_url.take().filter(|_| is_click);
        let (Some(url), Some(open)) = (clicked, self.open_url.clone()) else {
            return false;
        };
        self.anchor = None;
        self.focus = None;
        open(&url);
        true
    }

    fn dispatch_selection_click(&mut self, ev: Sgr, targets: &mut dyn MouseTargets) -> bool {
        let count = self.last_click.map_or(1, |c| c.count);
        let click = self.mouse_event(MouseEventType::Click, ev.button, ev.x, ev.y, Some(count));
        let Some(d) = targets.dispatch_at(&click) else {
            return false;
        };
        self.apply_dispatch(d);
        self.clear_text_selection();
        true
    }

    fn selection_release(&mut self, ev: Sgr, point: Point, targets: &mut dyn MouseTargets) -> ViewportInput {
        if !self.press_active {
            return ViewportInput::Consumed;
        }
        self.press_active = false;
        self.stop_auto_scroll();
        if self.anchor.is_none() {
            return ViewportInput::Consumed;
        }
        self.update_focus(point);
        let is_click = is_anchor_click(self.anchor, point, self.dragged);
        if self.open_clicked_url(is_click) {
            return ViewportInput::Consumed;
        }
        if is_click && self.dispatch_selection_click(ev, targets) {
            return ViewportInput::Consumed;
        }
        if self.copy_on_select
            && let Some(text) = self.selection_text()
        {
            return ViewportInput::Copy(text);
        }
        ViewportInput::Consumed
    }

    fn selection_drag(&mut self, ev: Sgr, point: Point, now: Instant) -> ViewportInput {
        if !self.press_active || self.anchor.is_none() {
            return ViewportInput::Consumed;
        }
        self.dragged = true;
        self.last_click = None;
        self.pressed_url = None;
        self.update_focus(point);
        self.update_auto_scroll(ev, now);
        ViewportInput::Consumed
    }

    /// The link a click at `ev` opens: an OSC 8 hyperlink, and with Option/Alt (SGR modifier bit 8) also a URL shown
    /// as plain text. Without OSC 8 (tmux without its hyperlinks feature, Terminal.app) the screen has no link to
    /// find, and the captured mouse keeps the terminal's own URL detection away.
    fn hovered_url_at(&self, ev: Sgr, has_range: bool) -> Option<String> {
        if has_range {
            return None;
        }
        let row = ev.y.clamp(0, (self.rows as i64 - 1).max(0)) as usize;
        let col = ev.x.clamp(0, (self.cols as i64 - 1).max(0)) as usize;
        links::link_at(self.screen.get(row)?, col, ev.button & 8 != 0).map(|link| link.url)
    }

    fn selection_press(&mut self, ev: Sgr, now: Instant) -> ViewportInput {
        self.stop_auto_scroll();
        self.press_active = true;
        let in_view = self.rect.is_some_and(|r| r.contains(ev.x, ev.y));
        let anchor = self.point(ev.x, ev.y, in_view);
        let word = self.word(anchor);
        let count = self.click_count(anchor, word, now);
        let (range, granularity) = press_granularity(word, self.line(anchor), count);
        self.granularity = granularity;
        self.initial = range;
        self.anchor = Some(range.map_or(anchor, |r| r.start));
        self.focus = Some(range.map_or(anchor, |r| r.end));
        self.dragged = false;
        self.pressed_url = self.hovered_url_at(ev, range.is_some());
        ViewportInput::Consumed
    }

    /// `handleSelectionMouseEvent`.
    pub(super) fn selection_mouse(&mut self, ev: Sgr, now: Instant, targets: &mut dyn MouseTargets) -> ViewportInput {
        if !is_primary_or_release(ev) {
            return ViewportInput::Consumed;
        }
        let anchor_scroll = self.anchor.is_some_and(|a| a.scroll);
        let point = self.point(ev.x, ev.y, anchor_scroll);
        if ev.release {
            return self.selection_release(ev, point, targets);
        }
        if ev.button & 32 != 0 {
            return self.selection_drag(ev, point, now);
        }
        self.selection_press(ev, now)
    }

    /// `getSelectionBounds`: ordered, or `None` when empty or across views.
    fn bounds(&self) -> Option<(Point, Point)> {
        let (a, f) = (self.anchor?, self.focus?);
        if a.scroll != f.scroll || (a.row == f.row && a.col == f.col) {
            return None;
        }
        let a_first = a.row < f.row || (a.row == f.row && a.col < f.col);
        Some(if a_first { (a, f) } else { (f, a) })
    }

    pub(super) fn apply_selection(&self, screen: Vec<String>) -> Vec<String> {
        let Some((start, end)) = self.bounds() else {
            return screen;
        };
        let (mut min_row, mut max_row) = (0i64, screen.len() as i64 - 1);
        let (mut min_col, mut max_col) = (0, self.cols);
        let (mut s, mut e) = (to_screen(start, 0), to_screen(end, 0));
        if start.scroll {
            let Some(rect) = self.rect else {
                return screen;
            };
            min_row = rect.y as i64;
            max_row = max_row.min((rect.y + rect.height) as i64 - 1);
            min_col = rect.x;
            max_col = self.cols.min(rect.x + rect.width);
            let shift = rect.y as i64 - self.view.scroll_top() as i64;
            s = ScreenPoint {
                row: start.row as i64 + shift,
                col: rect.x + start.col,
                boundary: start.boundary,
            };
            e = ScreenPoint {
                row: end.row as i64 + shift,
                col: rect.x + end.col,
                boundary: end.boundary,
            };
        }
        screen
            .into_iter()
            .enumerate()
            .map(|(row, line)| {
                let r = row as i64;
                if r < min_row || r > max_row || r < s.row || r > e.row || is_image_line(&line) {
                    return line;
                }
                let width = visible_width(&line);
                let (a, b) = selection_columns(&line, r, s, e, min_col, max_col);
                if b <= a {
                    return line;
                }
                let before = slice_by_column(&line, 0, a);
                let selected = slice_by_column(&line, a, b - a);
                let after = slice_by_column(&line, b, width.saturating_sub(b));
                format!("{before}{}{after}", selection_highlight(&selected))
            })
            .collect()
    }
}

fn word_selection_segments(line: &str) -> Vec<(usize, usize, bool, bool)> {
    let mut segments: Vec<(usize, usize, bool, bool)> = Vec::new();
    let mut start = 0;
    for (segment, word_like) in crate::word_segment::word_segments(line) {
        let end = start + visible_width(segment);
        let joiner = segment == "/" || segment == "-";
        segments.push((start, end, word_like || joiner, joiner));
        start = end;
    }
    segments
}

fn segments_join(left: (usize, usize, bool, bool), right: (usize, usize, bool, bool)) -> bool {
    left.2 && right.2 && (left.3 || right.3)
}

fn expand_word_left(segments: &[(usize, usize, bool, bool)], mut index: usize) -> usize {
    while index > 0 && segments_join(segments[index - 1], segments[index]) {
        index -= 1;
    }
    segments[index].0
}

fn expand_word_right(segments: &[(usize, usize, bool, bool)], mut index: usize) -> usize {
    while index + 1 < segments.len() && segments_join(segments[index], segments[index + 1]) {
        index += 1;
    }
    segments[index].1
}

/// `getWordSelection` on a plain `line`: the columns `[start, end)` of the word under column `col` (`/` and `-`
/// join selectable neighbours, so paths and kebab-case stay whole), or `None` past the line's end.
pub fn word_selection(line: &str, col: usize) -> Option<(usize, usize)> {
    let segments = word_selection_segments(line);
    let clicked = segments.iter().position(|s| col >= s.0 && col < s.1)?;
    Some((
        expand_word_left(&segments, clicked),
        expand_word_right(&segments, clicked),
    ))
}

fn to_screen(p: Point, shift: i64) -> ScreenPoint {
    ScreenPoint {
        row: p.row as i64 + shift,
        col: p.col,
        boundary: p.boundary,
    }
}

/// `getSelectionColumns`: the columns of `line` (screen or content row `row`) inside the selection `s`..`e`.
fn selection_columns(
    line: &str,
    row: i64,
    s: ScreenPoint,
    e: ScreenPoint,
    min_col: usize,
    max_col: usize,
) -> (usize, usize) {
    let width = visible_width(line);
    let mut start = min_col;
    let mut end = width.min(max_col);
    if row == s.row {
        start = grapheme_cell_range(line, s.col).map_or(s.col.min(width), |r| r.0);
    }
    if row == e.row {
        end = if e.boundary {
            e.col.min(width)
        } else {
            grapheme_cell_range(line, e.col).map_or((e.col + 1).min(width), |r| r.1)
        };
    }
    (start.max(min_col), end.min(max_col))
}

/// `applySelectionHighlight`: inverse video that survives the text's own SGR codes.
fn selection_highlight(text: &str) -> String {
    let mut out = String::from("\x1b[7m");
    let mut i = 0;
    while i < text.len() {
        match extract_ansi_code(text, i) {
            Some(code) => {
                out.push_str(code);
                if code.ends_with('m') {
                    out.push_str("\x1b[7m");
                }
                i += code.len();
            }
            None => {
                let c = text[i..].chars().next().unwrap_or('\0');
                out.push(c);
                i += c.len_utf8().max(1);
            }
        }
    }
    out.push_str("\x1b[27m");
    out
}
