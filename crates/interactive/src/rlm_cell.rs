//! Transcript rendering of an `rlm` cell (packages/coding-agent/src/experimental/rlm-tool-renderer.ts): the cell's
//! Python with a line-number gutter and its output. Collapsed, the code shows its first lines and the output its last
//! visual lines, each with an expand-key hint; expanded, both are shown in full, then the diff of every file the cell
//! wrote (`details.fileChanges`, Rust-only; collapsed, nothing of it is drawn). Syntax highlighting is injected
//! (`highlightCode(code, "python")` in TS); the product's is [`crate::rlm_highlight::rlm_highlighter`].

use std::rc::Rc;

use jsrt::string;
use serde_json::Value;
use theme::{Fg, Theme};
use tui::{Component, Container, Text};
use tui::{truncate_to_width, visible_width, wrap_text_with_ansi};

use crate::components::{
    FileChange, ToolRenderState, ToolRenderers, ToolResultView, bold, key_hint, render_diff, tool_text_output_with,
    truncate_to_visual_lines,
};

/// Code lines shown while collapsed.
pub const RLM_CODE_PREVIEW_LINES: usize = 12;
/// Output visual lines shown while collapsed (the tail, where results and tracebacks end).
pub const RLM_OUTPUT_PREVIEW_LINES: usize = 10;

/// `highlightCode(code, "python")`: one styled string per line of `code`.
pub type Highlighter = Rc<dyn Fn(&str) -> Vec<String>>;

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `outputTail`: the last `max` visual lines of `output` (each styled by `color`) at `width`, and how many came before.
pub fn output_tail(output: &str, color: &dyn Fn(&str) -> String, max: usize, width: usize) -> (Vec<String>, usize) {
    if output.contains('\x1b') || output.contains('\r') {
        let styled = output.split('\n').map(color).collect::<Vec<_>>().join("\n");
        return truncate_to_visual_lines(&styled, max, width, 0);
    }
    let mut tail = Vec::new();
    let mut total = 0;
    for line in output.split('\n').rev() {
        let plain = line.replace('\t', "   ");
        let line_width = visible_width(&plain);
        if line_width <= width {
            total += 1;
            if tail.len() < max {
                tail.push(format!("{}{}", color(&plain), " ".repeat(width - line_width)));
            }
            continue;
        }
        let wrapped = wrap_text_with_ansi(&color(&plain), width);
        total += wrapped.len();
        for row in wrapped.iter().rev() {
            if tail.len() >= max {
                break;
            }
            tail.push(format!("{row}{}", " ".repeat(width.saturating_sub(visible_width(row)))));
        }
    }
    tail.reverse();
    let skipped = total - tail.len();
    (tail, skipped)
}

fn render_call(
    args: &Value,
    theme: &Theme,
    state: &ToolRenderState,
    highlight: &Highlighter,
    expand_key: &str,
) -> Box<dyn Component> {
    let code = args.get("code").and_then(Value::as_str);
    let meta = code.map_or(String::new(), |c| {
        let n = c.split('\n').count();
        theme.fg(Fg::Muted, &format!(" python · {n} line{}", plural(n)))
    });
    let mut children: Vec<Box<dyn Component>> = vec![Box::new(Text::new(
        &format!("{}{meta}", theme.fg(Fg::ToolTitle, &bold("rlm"))),
        0,
        0,
        None,
    ))];
    let Some(code) = code.filter(|c| !c.trim_matches(string::is_space).is_empty()) else {
        if !state.args_complete {
            children.push(Box::new(Text::new(&theme.fg(Fg::ToolOutput, "..."), 0, 0, None)));
        }
        return Box::new(Container { children });
    };
    let lines = highlight(&code.trim_end_matches(string::is_space).replace('\t', "    "));
    let shown = if state.expanded {
        lines.len()
    } else {
        lines.len().min(RLM_CODE_PREVIEW_LINES)
    };
    let gutter = lines.len().to_string().len();
    let body = lines[..shown]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{} {line}", theme.fg(Fg::Dim, &format!("{:>gutter$} │", i + 1))))
        .collect::<Vec<_>>()
        .join("\n");
    let mut text = format!("\n{body}");
    let hidden = lines.len() - shown;
    if hidden > 0 {
        text.push_str(&format!(
            "\n{} {}{}",
            theme.fg(Fg::Muted, &format!("... ({hidden} more line{},", plural(hidden))),
            key_hint(theme, expand_key, "to expand"),
            theme.fg(Fg::Muted, ")")
        ));
    }
    children.push(Box::new(Text::new(&text, 0, 0, None)));
    Box::new(Container { children })
}

/// The collapsed output preview, laid out per width.
struct OutputPreview {
    theme: Rc<Theme>,
    output: String,
    color: Fg,
    label: String,
    expand_key: String,
}

impl Component for OutputPreview {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = &self.theme;
        let color = |s: &str| t.fg(self.color, s);
        let (visual, skipped) = output_tail(&self.output, &color, RLM_OUTPUT_PREVIEW_LINES, width);
        let mut lines = vec![String::new(), self.label.clone()];
        if skipped > 0 {
            let hint = format!(
                "{} {}{}",
                t.fg(Fg::Muted, &format!("... ({skipped} earlier line{},", plural(skipped))),
                key_hint(t, &self.expand_key, "to expand"),
                t.fg(Fg::Muted, ")")
            );
            lines.push(truncate_to_width(&hint, width, "...", false));
        }
        lines.extend(visual);
        lines
    }
}

/// The files a cell wrote, each with its diff (word-level for a single changed line, `renderDiff`). Shown only
/// expanded: the collapsed cell stays as TypeScript draws it.
pub fn file_changes_text(theme: &Theme, changes: &[FileChange]) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    let n = changes.len();
    let mut lines = vec![
        String::new(),
        theme.fg(Fg::Muted, &format!("{n} file{} changed", plural(n))),
    ];
    for change in changes {
        let created = if change.created {
            theme.fg(Fg::Muted, " (new)")
        } else {
            String::new()
        };
        lines.push(format!("{}{created}", theme.fg(Fg::Accent, &change.path)));
        match &change.diff {
            Some(diff) => lines.push(render_diff(theme, diff)),
            None => lines.push(theme.fg(Fg::Muted, "(too large or not text to show)")),
        }
        if change.omitted > 0 {
            lines.push(theme.fg(
                Fg::Muted,
                &format!("... ({} more diff line{})", change.omitted, plural(change.omitted)),
            ));
        }
    }
    Some(lines.join("\n"))
}

fn render_result(
    result: &ToolResultView,
    theme: &Rc<Theme>,
    state: &ToolRenderState,
    expand_key: &str,
) -> Box<dyn Component> {
    let output = tool_text_output_with(Some(result), state.inline_images)
        .trim_end_matches(string::is_space)
        .to_string();
    let mut children: Vec<Box<dyn Component>> = Vec::new();
    let changes = state
        .expanded
        .then(|| file_changes_text(theme, &result.file_changes))
        .flatten();
    if output.is_empty() {
        if state.is_partial {
            children.push(Box::new(Text::new(
                &format!("\n{}", theme.fg(Fg::Muted, "running…")),
                0,
                0,
                None,
            )));
        }
        if let Some(changes) = changes {
            children.push(Box::new(Text::new(&changes, 0, 0, None)));
        }
        return Box::new(Container { children });
    }
    let color = if state.is_error { Fg::Error } else { Fg::ToolOutput };
    let label = theme.fg(
        Fg::Muted,
        if state.is_error {
            "error"
        } else if state.is_partial {
            "output (running)"
        } else {
            "output"
        },
    );
    if state.expanded {
        let styled = output
            .split('\n')
            .map(|l| theme.fg(color, l))
            .collect::<Vec<_>>()
            .join("\n");
        children.push(Box::new(Text::new(&format!("\n{label}\n{styled}"), 0, 0, None)));
        if let Some(changes) = changes {
            children.push(Box::new(Text::new(&changes, 0, 0, None)));
        }
    } else {
        children.push(Box::new(OutputPreview {
            theme: Rc::clone(theme),
            output,
            color,
            label,
            expand_key: expand_key.to_string(),
        }));
    }
    Box::new(Container { children })
}

/// `rlmToolRenderers` for a `ToolExecution` of the `rlm` tool, drawing with `theme`.
pub fn rlm_renderers(theme: &Rc<Theme>, highlight: Highlighter, expand_key: &str) -> ToolRenderers {
    let call_key = expand_key.to_string();
    let result_key = expand_key.to_string();
    let result_theme = Rc::clone(theme);
    ToolRenderers {
        self_shell: false,
        call: Some(Box::new(move |args, theme, state| {
            Some(render_call(args, theme, state, &highlight, &call_key))
        })),
        result: Some(Box::new(move |result, _theme, state| {
            Some(render_result(result, &result_theme, state, &result_key))
        })),
    }
}
