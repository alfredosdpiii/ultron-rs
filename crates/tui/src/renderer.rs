//! Main-screen differential renderer (port of packages/tui/src/tui-main-screen.ts `doRender`): repaints only
//! the changed line range inside synchronized output and tracks the hardware cursor row.

use crate::frame::{SYNC_BEGIN, SYNC_END, extract_cursor, is_image_line};
use crate::utils::{normalize_terminal_output, visible_width};

const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

/// A rendered frame overflowed the terminal width.
#[derive(Debug, PartialEq, Eq)]
pub struct LineTooWide {
    pub line: usize,
    pub width: usize,
    pub terminal_width: usize,
}

/// Renderer state between frames. `render` returns the bytes to write to the terminal.
#[derive(Default)]
pub struct Renderer {
    previous_lines: Vec<String>,
    previous_width: usize,
    previous_height: usize,
    cursor_row: usize,
    hardware_cursor_row: i64,
    max_lines_rendered: usize,
    previous_viewport_top: i64,
    pub show_hardware_cursor: bool,
    pub clear_on_shrink: bool,
    pub full_redraws: usize,
}

fn mv(out: &mut String, diff: i64) {
    if diff > 0 {
        out.push_str(&format!("\x1b[{diff}B"));
    } else if diff < 0 {
        out.push_str(&format!("\x1b[{}A", -diff));
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lines of the last frame as written (after cursor extraction and line resets).
    pub fn previous_lines(&self) -> &[String] {
        &self.previous_lines
    }

    fn compute_viewport_top(&self, height: usize, height_changed: bool) -> i64 {
        let h = height as i64;
        let previous_buffer_len = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height as i64
        } else {
            h
        };
        if height_changed {
            (previous_buffer_len - h).max(0)
        } else {
            self.previous_viewport_top
        }
    }

    fn prepare_lines(lines: &mut [String]) {
        for line in lines {
            if !is_image_line(line) {
                *line = normalize_terminal_output(line) + SEGMENT_RESET;
            }
        }
    }

    fn should_full_render(&self, lines: &[String], width_changed: bool, height_changed: bool) -> bool {
        self.previous_lines.is_empty()
            || width_changed
            || height_changed
            || (self.clear_on_shrink && lines.len() < self.max_lines_rendered)
    }

    fn find_diff_bounds(prev: &[String], lines: &[String]) -> Option<(usize, usize)> {
        let prev_len = prev.len();
        let mut first: Option<usize> = None;
        let mut last = 0;
        for i in 0..lines.len().max(prev_len) {
            let old = prev.get(i).map_or("", String::as_str);
            let new = lines.get(i).map_or("", String::as_str);
            if old != new {
                first.get_or_insert(i);
                last = i;
            }
        }
        if lines.len() > prev_len {
            first.get_or_insert(prev_len);
            last = lines.len() - 1;
        }
        first.map(|f| (f, last))
    }

    fn clear_extra_rows(buf: &mut String, extra: usize, has_lines: bool) {
        let clear_start = usize::from(has_lines);
        if extra > 0 && clear_start > 0 {
            buf.push_str(&format!("\x1b[{clear_start}B"));
        }
        for i in 0..extra {
            buf.push_str("\r\x1b[2K");
            if i + 1 < extra {
                buf.push_str("\x1b[1B");
            }
        }
        let move_back = (extra + clear_start).saturating_sub(1);
        if move_back > 0 {
            buf.push_str(&format!("\x1b[{move_back}A"));
        }
    }

    #[expect(clippy::too_many_arguments, reason = "internal shrunk-tail renderer")]
    fn render_shrunk_tail(
        &mut self,
        out: &mut String,
        lines: Vec<String>,
        cursor: Option<(usize, usize)>,
        width: usize,
        height: usize,
        prev_viewport_top: i64,
        hardware_cursor_row: i64,
    ) {
        let prev_len = self.previous_lines.len();
        if prev_len > lines.len() {
            let target_row = lines.len().saturating_sub(1) as i64;
            let extra = prev_len - lines.len();
            if target_row < prev_viewport_top || extra > height {
                self.full_render(out, lines, true, cursor, width, height);
                return;
            }
            let mut buf = String::from(SYNC_BEGIN);
            mv(&mut buf, target_row - hardware_cursor_row);
            buf.push('\r');
            Self::clear_extra_rows(&mut buf, extra, !lines.is_empty());
            buf.push_str(SYNC_END);
            out.push_str(&buf);
            self.cursor_row = target_row as usize;
            self.hardware_cursor_row = target_row;
        }
        self.position_cursor(out, cursor, lines.len());
        self.previous_lines = lines;
        self.previous_width = width;
        self.previous_height = height;
        self.previous_viewport_top = prev_viewport_top;
    }

    fn scroll_into_view(
        buf: &mut String,
        move_target: i64,
        prev_viewport_top: &mut i64,
        viewport_top: &mut i64,
        hardware_cursor_row: &mut i64,
        h: i64,
    ) {
        let prev_viewport_bottom = *prev_viewport_top + h - 1;
        if move_target > prev_viewport_bottom {
            let current_screen_row = (*hardware_cursor_row - *prev_viewport_top).clamp(0, h - 1);
            let to_bottom = h - 1 - current_screen_row;
            if to_bottom > 0 {
                buf.push_str(&format!("\x1b[{to_bottom}B"));
            }
            let scroll = move_target - prev_viewport_bottom;
            buf.push_str(&"\r\n".repeat(scroll as usize));
            *prev_viewport_top += scroll;
            *viewport_top += scroll;
            *hardware_cursor_row = move_target;
        }
    }

    fn write_changed_lines(
        buf: &mut String,
        lines: &[String],
        first: usize,
        render_end: usize,
        width: usize,
    ) -> Result<(), LineTooWide> {
        for (i, line) in lines.iter().enumerate().take(render_end + 1).skip(first) {
            if i > first {
                buf.push_str("\r\n");
            }
            buf.push_str("\x1b[2K");
            if !is_image_line(line) && visible_width(line) > width {
                return Err(LineTooWide {
                    line: i,
                    width: visible_width(line),
                    terminal_width: width,
                });
            }
            buf.push_str(line);
        }
        Ok(())
    }

    fn clear_trailing_lines(buf: &mut String, prev_len: usize, lines_len: usize, render_end: usize) -> usize {
        let mut final_row = render_end;
        if prev_len > lines_len {
            if render_end < lines_len - 1 {
                buf.push_str(&format!("\x1b[{}B", lines_len - 1 - render_end));
                final_row = lines_len - 1;
            }
            let extra = prev_len - lines_len;
            buf.push_str(&"\r\n\x1b[2K".repeat(extra));
            buf.push_str(&format!("\x1b[{extra}A"));
        }
        final_row
    }

    #[expect(clippy::too_many_arguments, reason = "internal diff range renderer")]
    fn render_diff_range(
        &mut self,
        out: &mut String,
        lines: Vec<String>,
        first: usize,
        last: usize,
        append_start: bool,
        cursor: Option<(usize, usize)>,
        width: usize,
        height: usize,
        mut prev_viewport_top: i64,
    ) -> Result<(), LineTooWide> {
        let h = height as i64;
        let prev_len = self.previous_lines.len();
        let mut viewport_top = prev_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;
        let mut buf = String::from(SYNC_BEGIN);
        let move_target = if append_start { first - 1 } else { first } as i64;
        Self::scroll_into_view(
            &mut buf,
            move_target,
            &mut prev_viewport_top,
            &mut viewport_top,
            &mut hardware_cursor_row,
            h,
        );
        mv(
            &mut buf,
            (move_target - viewport_top) - (hardware_cursor_row - prev_viewport_top),
        );
        buf.push_str(if append_start { "\r\n" } else { "\r" });

        let render_end = last.min(lines.len() - 1);
        Self::write_changed_lines(&mut buf, &lines, first, render_end, width)?;
        let final_row = Self::clear_trailing_lines(&mut buf, prev_len, lines.len(), render_end);
        buf.push_str(SYNC_END);
        out.push_str(&buf);

        self.cursor_row = lines.len().saturating_sub(1);
        self.hardware_cursor_row = final_row as i64;
        self.max_lines_rendered = self.max_lines_rendered.max(lines.len());
        self.previous_viewport_top = prev_viewport_top.max(final_row as i64 - h + 1);
        self.position_cursor(out, cursor, lines.len());
        self.previous_lines = lines;
        self.previous_width = width;
        self.previous_height = height;
        Ok(())
    }

    fn prepare_render_pass(
        &mut self,
        lines: &mut [String],
        width: usize,
        height: usize,
    ) -> (bool, bool, i64, Option<(usize, usize)>) {
        let width_changed = self.previous_width != 0 && self.previous_width != width;
        let height_changed = self.previous_height != 0 && self.previous_height != height;
        let prev_viewport_top = self.compute_viewport_top(height, height_changed);
        let cursor = extract_cursor_position(lines, height);
        Self::prepare_lines(lines);
        (width_changed, height_changed, prev_viewport_top, cursor)
    }

    /// Paint one frame of `lines` on a `width` x `height` terminal.
    pub fn render(&mut self, mut lines: Vec<String>, width: usize, height: usize) -> Result<String, LineTooWide> {
        let mut out = String::new();
        let (width_changed, height_changed, prev_viewport_top, cursor) =
            self.prepare_render_pass(&mut lines, width, height);

        if self.should_full_render(&lines, width_changed, height_changed) {
            let clear = !self.previous_lines.is_empty() || width_changed || height_changed;
            self.full_render(&mut out, lines, clear, cursor, width, height);
            return Ok(out);
        }

        let prev_len = self.previous_lines.len();
        let Some((first, last)) = Self::find_diff_bounds(&self.previous_lines, &lines) else {
            self.position_cursor(&mut out, cursor, lines.len());
            self.previous_viewport_top = prev_viewport_top;
            self.previous_height = height;
            return Ok(out);
        };

        if first >= lines.len() {
            let hardware_cursor_row = self.hardware_cursor_row;
            self.render_shrunk_tail(
                &mut out,
                lines,
                cursor,
                width,
                height,
                prev_viewport_top,
                hardware_cursor_row,
            );
            return Ok(out);
        }

        if (first as i64) < prev_viewport_top {
            self.full_render(&mut out, lines, true, cursor, width, height);
            return Ok(out);
        }

        let append_start = lines.len() > prev_len && first == prev_len && first > 0;
        self.render_diff_range(
            &mut out,
            lines,
            first,
            last,
            append_start,
            cursor,
            width,
            height,
            prev_viewport_top,
        )?;
        Ok(out)
    }

    fn full_render(
        &mut self,
        out: &mut String,
        lines: Vec<String>,
        clear: bool,
        cursor: Option<(usize, usize)>,
        width: usize,
        height: usize,
    ) {
        self.full_redraws += 1;
        out.push_str(SYNC_BEGIN);
        if clear {
            out.push_str("\x1b[2J\x1b[H\x1b[3J");
        }
        out.push_str(&lines.join("\r\n"));
        out.push_str(SYNC_END);
        self.cursor_row = lines.len().saturating_sub(1);
        self.hardware_cursor_row = self.cursor_row as i64;
        self.max_lines_rendered = if clear {
            lines.len()
        } else {
            self.max_lines_rendered.max(lines.len())
        };
        self.previous_viewport_top = (height.max(lines.len()) - height) as i64;
        self.position_cursor(out, cursor, lines.len());
        self.previous_lines = lines;
        self.previous_width = width;
        self.previous_height = height;
    }

    fn position_cursor(&mut self, out: &mut String, cursor: Option<(usize, usize)>, total: usize) {
        let Some((row, col)) = cursor.filter(|_| total > 0) else {
            out.push_str(HIDE_CURSOR);
            return;
        };
        let target = row.min(total - 1) as i64;
        mv(out, target - self.hardware_cursor_row);
        out.push_str(&format!("\x1b[{}G", col + 1));
        self.hardware_cursor_row = target;
        out.push_str(if self.show_hardware_cursor {
            SHOW_CURSOR
        } else {
            HIDE_CURSOR
        });
    }
}

/// Find, strip and locate the last cursor marker within the bottom `height` lines.
fn extract_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    let top = lines.len().saturating_sub(height);
    extract_cursor(&mut lines[top..]).map(|(row, col)| (top + row, col))
}
