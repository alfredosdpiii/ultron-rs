use super::Editor;
use crate::frame::CURSOR_MARKER;
use crate::utils::{slice_with_width, visible_width};

fn scroll_border(direction: &str, hidden: usize, width: usize) -> String {
    let label = format!(" {direction} {hidden} more ");
    let label_width = visible_width(&label);
    if label_width + 2 <= width {
        let left = (width - label_width) / 2;
        return format!("{}{label}{}", "─".repeat(left), "─".repeat(width - left - label_width));
    }
    let indicator = format!("─── {direction} {hidden} more ");
    let iw = visible_width(&indicator);
    if iw <= width {
        return format!("{indicator}{}", "─".repeat(width - iw));
    }
    let ellipsis = &"..."[..width.min(3)];
    slice_with_width(&indicator, 0, width - ellipsis.len(), true).0 + ellipsis
}

impl Editor {
    fn update_scroll_offset(&mut self, cursor_index: usize, max_visible: usize, layout_len: usize) {
        if cursor_index < self.scroll_offset {
            self.scroll_offset = cursor_index;
        } else if cursor_index >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor_index + 1 - max_visible;
        }
        self.scroll_offset = self.scroll_offset.min(layout_len.saturating_sub(max_visible));
    }

    fn render_border(&self, hidden: usize, dir: &str, width: usize) -> String {
        let raw = if hidden > 0 {
            scroll_border(dir, hidden, width)
        } else {
            "─".repeat(width)
        };
        (self.border_color)(&raw)
    }

    fn format_visible_line(
        &self,
        text: &str,
        cursor: Option<usize>,
        content_width: usize,
        padding_x: usize,
        pad: &str,
    ) -> String {
        let mut display = text.to_string();
        let mut line_width = visible_width(text);
        let mut cursor_in_padding = false;
        if let Some(pos) = cursor {
            let (before, after) = text.split_at(pos);
            let marker = if self.focused { CURSOR_MARKER } else { "" };
            if after.is_empty() {
                display = format!("{before}{marker}\x1b[7m \x1b[0m");
                line_width += 1;
                cursor_in_padding = line_width > content_width && padding_x > 0;
            } else {
                let first = self.graphemes(after).first().map_or("", |g| g.1);
                display = format!("{before}{marker}\x1b[7m{first}\x1b[0m{}", &after[first.len()..]);
            }
        }
        let fill = " ".repeat(content_width.saturating_sub(line_width));
        let right = if cursor_in_padding { &pad[1..] } else { pad };
        format!("{pad}{display}{fill}{right}")
    }

    fn append_picker(&mut self, result: &mut Vec<String>, content_width: usize, pad: &str) {
        self.rendered_picker_height = 0;
        let Some(picker) = &mut self.picker else {
            return;
        };
        let lines = crate::components::Component::render(&mut picker.list, content_width);
        self.rendered_picker_height = lines.len();
        for line in lines {
            let fill = " ".repeat(content_width.saturating_sub(visible_width(&line)));
            result.push(format!("{pad}{line}{fill}{pad}"));
        }
    }

    pub fn render(&mut self, width: usize) -> Vec<String> {
        let padding_x = self.padding_x.min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let layout_width = content_width.saturating_sub(usize::from(padding_x == 0)).max(1);
        self.last_width = layout_width;
        let layout = self.layout_text(layout_width);
        let max_visible = self.max_visible_lines();
        let cursor_index = layout.iter().position(|(_, c)| c.is_some()).unwrap_or(0);
        self.update_scroll_offset(cursor_index, max_visible, layout.len());
        let visible = &layout[self.scroll_offset..(self.scroll_offset + max_visible).min(layout.len())];
        let pad = " ".repeat(padding_x);
        let mut result = vec![self.render_border(self.scroll_offset, "↑", width)];
        for (text, cursor) in visible {
            result.push(self.format_visible_line(text, *cursor, content_width, padding_x, &pad));
        }
        let below = layout.len() - (self.scroll_offset + visible.len());
        result.push(self.render_border(below, "↓", width));
        self.rendered_visible_lines = visible.len();
        self.append_picker(&mut result, content_width, &pad);
        result
    }
}
