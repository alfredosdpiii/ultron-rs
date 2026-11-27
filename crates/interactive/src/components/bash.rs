//! User shell (`!`/`!!`) execution component and tail truncation.

use std::rc::Rc;

use theme::{Fg, Theme};
use tui::{Component, Loader, Text, strip_terminal_sequences};

use super::{bold, dynamic_border, fg_fn, key_hint, truncate_to_visual_lines};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BashStatus {
    Running,
    Complete,
    Cancelled,
    Error,
}

const PREVIEW_LINES: usize = 20;
const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// `truncateTail(content)` with the bash tool's limits: the kept tail and whether anything was cut.
pub fn truncate_tail(content: &str) -> (String, bool) {
    let mut lines: Vec<&str> = if content.is_empty() {
        Vec::new()
    } else {
        content.split('\n').collect()
    };
    if content.ends_with('\n') {
        lines.pop();
    }
    if lines.len() <= DEFAULT_MAX_LINES && content.len() <= DEFAULT_MAX_BYTES {
        return (content.to_string(), false);
    }
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        if kept.len() >= DEFAULT_MAX_LINES {
            break;
        }
        let add = line.len() + usize::from(!kept.is_empty());
        if bytes + add > DEFAULT_MAX_BYTES {
            if kept.is_empty() {
                let mut start = line.len() - DEFAULT_MAX_BYTES;
                while !line.is_char_boundary(start) {
                    start += 1;
                }
                kept.push(line[start..].to_string());
            }
            break;
        }
        kept.push((*line).to_string());
        bytes += add;
    }
    kept.reverse();
    (kept.join("\n"), true)
}

/// `BashExecutionComponent` (the `!`/`!!` user shell command).
pub struct BashExecution {
    pub theme: Rc<Theme>,
    pub command: String,
    pub exclude_from_context: bool,
    output_lines: Vec<String>,
    pub status: BashStatus,
    pub exit_code: Option<i64>,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    pub expanded: bool,
    /// Formatted `app.tools.expand` key text.
    pub expand_key: String,
    updated: bool,
    loader: Loader,
}

impl BashExecution {
    /// `cancel_key` is the formatted `tui.select.cancel` key text shown while running.
    pub fn new(
        theme: Rc<Theme>,
        command: &str,
        exclude_from_context: bool,
        expand_key: &str,
        cancel_key: &str,
    ) -> Self {
        let color = if exclude_from_context { Fg::Dim } else { Fg::BashMode };
        let message = format!("Running... ({cancel_key} to cancel)");
        let loader = Loader::new(fg_fn(&theme, color), fg_fn(&theme, Fg::Muted), &message, None);
        Self {
            theme,
            command: command.to_string(),
            exclude_from_context,
            output_lines: Vec::new(),
            status: BashStatus::Running,
            exit_code: None,
            truncated: false,
            full_output_path: None,
            expanded: false,
            expand_key: expand_key.to_string(),
            updated: false,
            loader,
        }
    }

    /// `appendOutput`: strip ANSI, normalize line endings, continue the last partial line.
    pub fn append_output(&mut self, chunk: &str) {
        let clean = strip_terminal_sequences(chunk)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let mut parts = clean.split('\n');
        if let Some(last) = self.output_lines.last_mut() {
            last.push_str(parts.next().unwrap_or(""));
        }
        self.output_lines.extend(parts.map(str::to_string));
        self.updated = true;
    }

    pub fn set_complete(
        &mut self,
        exit_code: Option<i64>,
        cancelled: bool,
        truncated: bool,
        full_output_path: Option<&str>,
    ) {
        self.exit_code = exit_code;
        self.status = if cancelled {
            BashStatus::Cancelled
        } else if exit_code.is_some_and(|c| c != 0) {
            BashStatus::Error
        } else {
            BashStatus::Complete
        };
        self.truncated = truncated;
        self.full_output_path = full_output_path.map(str::to_string);
        self.updated = true;
    }

    pub fn output(&self) -> String {
        self.output_lines.join("\n")
    }

    pub fn tick(&mut self) {
        self.loader.tick();
    }

    fn render_output_lines(&self, available: &[&str], hidden: usize, width: usize) -> Vec<String> {
        if available.is_empty() {
            return Vec::new();
        }
        let shown = if self.expanded { available } else { &available[hidden..] };
        let text = format!(
            "\n{}",
            shown
                .iter()
                .map(|l| self.theme.fg(Fg::Muted, l))
                .collect::<Vec<_>>()
                .join("\n")
        );
        if self.expanded {
            Text::new(&text, 1, 0, None).render(width)
        } else {
            truncate_to_visual_lines(&text, PREVIEW_LINES, width, 1).0
        }
    }

    fn expand_hint_line(&self, hidden: usize) -> String {
        let t = &self.theme;
        if self.expanded {
            format!(
                "{}{}{}",
                t.fg(Fg::Muted, "("),
                key_hint(t, &self.expand_key, "to collapse"),
                t.fg(Fg::Muted, ")")
            )
        } else {
            format!(
                "{}{}{}",
                t.fg(Fg::Muted, &format!("... {hidden} more lines (")),
                key_hint(t, &self.expand_key, "to expand"),
                t.fg(Fg::Muted, ")")
            )
        }
    }

    fn render_status_parts(&self, hidden: usize, context_truncated: bool) -> Vec<String> {
        let t = &self.theme;
        let mut parts = Vec::new();
        if hidden > 0 {
            parts.push(self.expand_hint_line(hidden));
        }
        match self.status {
            BashStatus::Cancelled => parts.push(t.fg(Fg::Warning, "(cancelled)")),
            BashStatus::Error => parts.push(t.fg(
                Fg::Error,
                &format!(
                    "(exit {})",
                    self.exit_code.map_or("undefined".to_string(), |c| c.to_string())
                ),
            )),
            _ => {}
        }
        if let Some(path) = self
            .full_output_path
            .as_deref()
            .filter(|_| self.truncated || context_truncated)
        {
            parts.push(t.fg(Fg::Warning, &format!("Output truncated. Full output: {path}")));
        }
        parts
    }
}

impl Component for BashExecution {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let color = if self.exclude_from_context {
            Fg::Dim
        } else {
            Fg::BashMode
        };
        let border = |s: &str| t.fg(color, s);
        let header_color = if self.updated { Fg::BashMode } else { color };
        let mut lines = vec![String::new()];
        lines.extend(dynamic_border(&border, width));
        let header = t.fg(header_color, &bold(&format!("$ {}", self.command)));
        lines.extend(Text::new(&header, 1, 0, None).render(width));
        let (tail, context_truncated) = truncate_tail(&self.output());
        let available: Vec<&str> = if tail.is_empty() {
            Vec::new()
        } else {
            tail.split('\n').collect()
        };
        let hidden = available.len().saturating_sub(PREVIEW_LINES);
        lines.extend(self.render_output_lines(&available, hidden, width));
        if self.status == BashStatus::Running {
            lines.extend(self.loader.render(width));
        } else {
            let parts = self.render_status_parts(hidden, context_truncated);
            if !parts.is_empty() {
                lines.extend(Text::new(&format!("\n{}", parts.join("\n")), 1, 0, None).render(width));
            }
        }
        lines.extend(dynamic_border(&border, width));
        lines
    }
}
