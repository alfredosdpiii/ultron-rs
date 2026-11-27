//! Tool execution view models and rendering.

use std::rc::Rc;

use serde_json::Value;
use theme::{Bg, Fg, Theme};
use tui::StyleFn;
use tui::{Component, Container, Panel, Text};
use tui::{image_dimensions, image_fallback, strip_terminal_sequences};

use super::{bg_fn, bold, fg_fn, key_hint};

/// One block of a tool result.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolContent {
    Text(String),
    Image { data: String, mime_type: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolResultView {
    pub content: Vec<ToolContent>,
    pub is_error: bool,
    /// What an rlm cell wrote (`details.fileChanges`, Rust-only): drawn under the expanded cell.
    pub file_changes: Vec<FileChange>,
}

/// One file an rlm cell wrote: `diff` in the edit tool's display format (None: too large or not text), `omitted`
/// diff lines the session left out.
#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub created: bool,
    pub diff: Option<String>,
    pub omitted: usize,
}

impl FileChange {
    /// `details.fileChanges` of a tool result message (nothing for anything malformed).
    pub fn from_details(details: &Value) -> Vec<FileChange> {
        details["fileChanges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|change| {
                Some(FileChange {
                    path: change["path"].as_str()?.to_string(),
                    created: change["created"] == true,
                    diff: change["diff"].as_str().map(str::to_string),
                    omitted: change["omitted"].as_u64().unwrap_or(0) as usize,
                })
            })
            .collect()
    }
}

/// What a tool renderer sees (`ToolRenderContext` minus the callbacks).
#[derive(Clone, Copy, Debug, Default)]
pub struct ToolRenderState {
    pub expanded: bool,
    pub is_partial: bool,
    pub is_error: bool,
    pub execution_started: bool,
    pub args_complete: bool,
    pub show_images: bool,
    /// Images are drawn inline (the terminal draws them and `show_images`): the text output names none.
    pub inline_images: bool,
}

/// A renderer returns `None` where the TS renderer throws; the component then uses its fallback.
pub type CallRenderer = Box<dyn Fn(&Value, &Theme, &ToolRenderState) -> Option<Box<dyn Component>>>;
pub type ResultRenderer = Box<dyn Fn(&ToolResultView, &Theme, &ToolRenderState) -> Option<Box<dyn Component>>>;

/// `ToolRenderers`: `self_shell` is `renderShell: "self"`.
pub struct ToolRenderers {
    pub self_shell: bool,
    pub call: Option<CallRenderer>,
    pub result: Option<ResultRenderer>,
}

/// One image a tool result draws inline: its (PNG on Kitty) data and its Kitty image id.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineImage {
    pub data: String,
    pub mime: String,
    pub image_id: Option<u32>,
}

/// How tool images are drawn: the terminal's protocol, `terminal.imageWidthCells`, the cell size, and the images.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineImages {
    pub protocol: tui::ImageProtocol,
    pub width_cells: u32,
    pub cell: tui::Dimensions,
    pub images: Vec<InlineImage>,
}

/// `ToolExecutionComponent`: the call and result, then each image of the result drawn inline after a blank line
/// (`inline`), or named in the text output when the terminal cannot draw it or `show_images` is off.
pub struct ToolExecution {
    pub theme: Rc<Theme>,
    pub tool_name: String,
    pub args: Value,
    pub result: Option<ToolResultView>,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub execution_started: bool,
    pub args_complete: bool,
    pub renderers: Option<ToolRenderers>,
    /// Formatted `app.tools.expand` key text.
    pub expand_key: String,
    /// The images drawn inline; `None` on a terminal without image support.
    pub inline: Option<InlineImages>,
}

/// `stripAnsi` + `sanitizeBinaryOutput` + drop `\r`.
fn clean_output(text: &str) -> String {
    strip_terminal_sequences(text)
        .chars()
        .filter(|c| {
            let u = *c as u32;
            u == 9 || u == 10 || (u > 0x1f && !(0xfff9..=0xfffb).contains(&u))
        })
        .collect()
}

/// `getTextOutput(result, showImages)`: the text blocks, then a line naming each image unless images are drawn
/// inline (`getCapabilities().images && showImages`).
pub fn tool_text_output_with(result: Option<&ToolResultView>, inline_images: bool) -> String {
    let Some(result) = result else {
        return String::new();
    };
    let mut out = result
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Text(t) => Some(clean_output(t)),
            ToolContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<String> = result
        .content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Image { data, mime_type } => {
                let mime = mime_type.as_deref().unwrap_or("image/unknown");
                let dims = mime_type
                    .as_deref()
                    .filter(|_| !data.is_empty())
                    .and_then(|m| image_dimensions(data, m));
                Some(image_fallback(mime, dims, None, false, None))
            }
            ToolContent::Text(_) => None,
        })
        .collect();
    if !images.is_empty() && !inline_images {
        let ind = images.join("\n");
        out = if out.is_empty() { ind } else { format!("{out}\n{ind}") };
    }
    out
}

const FALLBACK_PREVIEW_LINES: usize = 10;

impl ToolExecution {
    fn state(&self) -> ToolRenderState {
        ToolRenderState {
            expanded: self.expanded,
            is_partial: self.is_partial,
            is_error: self.result.as_ref().is_some_and(|r| r.is_error),
            execution_started: self.execution_started,
            args_complete: self.args_complete,
            show_images: self.show_images,
            inline_images: self.inline_images(),
        }
    }

    fn inline_images(&self) -> bool {
        self.show_images && self.inline.is_some()
    }

    /// The result's images as `Image` components, each after a blank line.
    fn image_lines(&self, width: usize) -> Vec<String> {
        let Some(inline) = self.inline.as_ref().filter(|_| self.show_images) else {
            return Vec::new();
        };
        let caps = tui::Capabilities {
            images: Some(inline.protocol),
            true_color: true,
            hyperlinks: false,
        };
        let mut lines = Vec::new();
        for image in &inline.images {
            let mut component = tui::Image::new(&image.data, &image.mime, fg_fn(&self.theme, Fg::ToolOutput), caps);
            component.max_width_cells = Some(inline.width_cells);
            component.cell = inline.cell;
            component.image_id = image.image_id;
            lines.push(String::new());
            lines.extend(component.render(width));
        }
        lines
    }

    fn bg(&self) -> StyleFn {
        let token = if self.is_partial {
            Bg::ToolPendingBg
        } else if self.result.as_ref().is_some_and(|r| r.is_error) {
            Bg::ToolErrorBg
        } else {
            Bg::ToolSuccessBg
        };
        bg_fn(&self.theme, token)
    }

    fn title(&self) -> String {
        self.theme.fg(Fg::ToolTitle, &bold(&self.tool_name))
    }

    fn result_fallback(&self) -> Option<Box<dyn Component>> {
        let output = tool_text_output_with(self.result.as_ref(), self.inline_images());
        if output.is_empty() {
            return None;
        }
        let lines: Vec<&str> = output.split('\n').collect();
        let shown = if self.expanded {
            lines.len()
        } else {
            lines.len().min(FALLBACK_PREVIEW_LINES)
        };
        let t = &self.theme;
        let mut text = lines[..shown]
            .iter()
            .map(|l| t.fg(Fg::ToolOutput, l))
            .collect::<Vec<_>>()
            .join("\n");
        if lines.len() > shown {
            let more = format!("\n... ({} more lines,", lines.len() - shown);
            text.push_str(&format!(
                "{} {}{}",
                t.fg(Fg::Muted, &more),
                key_hint(t, &self.expand_key, "to expand"),
                t.fg(Fg::Muted, ")")
            ));
        }
        Some(Box::new(Text::new(&text, 0, 0, None)))
    }

    fn format_generic(&self) -> String {
        let mut text = self.title();
        if let Ok(args) = serde_json::to_string_pretty(&self.args) {
            text.push_str(&format!("\n\n{args}"));
        }
        let output = tool_text_output_with(self.result.as_ref(), self.inline_images());
        if !output.is_empty() {
            text.push_str(&format!("\n{output}"));
        }
        text
    }
}

/// The cells a tool's call and result take in its rendered rows (`createResultRegion` around each): once there is a
/// result, a left click there toggles the tool's expansion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowRegion {
    pub start: usize,
    pub len: usize,
    pub x_start: usize,
    pub x_end: usize,
}

impl Component for ToolExecution {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.render_with_region(width).0
    }
}

impl ToolExecution {
    /// [`Component::render`], with the region of the call and result (none before a result).
    pub fn render_with_region(&mut self, width: usize) -> (Vec<String>, Option<RowRegion>) {
        let images = self.image_lines(width);
        let has_result = self.result.is_some();
        let region = |start: usize, len: usize, x_start: usize, x_end: usize| {
            (has_result && len > 0).then_some(RowRegion {
                start,
                len,
                x_start,
                x_end,
            })
        };
        let Some(r) = &self.renderers else {
            let mut lines = vec![String::new()];
            lines.extend(Text::new(&self.format_generic(), 1, 1, Some(self.bg())).render(width));
            let text_rows = lines.len() - 1;
            lines.extend(images);
            return (lines, region(1, text_rows, 0, width));
        };
        let state = self.state();
        let call_fallback = || -> Box<dyn Component> { Box::new(Text::new(&self.title(), 0, 0, None)) };
        let mut children = vec![
            r.call
                .as_ref()
                .and_then(|f| f(&self.args, &self.theme, &state))
                .unwrap_or_else(call_fallback),
        ];
        if let Some(result) = &self.result {
            let rendered = r.result.as_ref().and_then(|f| f(result, &self.theme, &state));
            if let Some(c) = rendered.or_else(|| self.result_fallback()) {
                children.push(c);
            }
        }
        if r.self_shell {
            let content = Container { children }.render(width);
            if content.is_empty() && images.is_empty() {
                return (content, None);
            }
            let rows = content.len();
            let mut lines = vec![String::new()];
            lines.extend(content);
            lines.extend(images);
            return (lines, region(1, rows, 0, width));
        }
        let mut lines = vec![String::new()];
        let panel = Panel {
            children,
            padding_x: 1,
            padding_y: 1,
            bg: Some(self.bg()),
        }
        .render(width);
        let content_rows = panel.len().saturating_sub(2);
        let content_width = width.saturating_sub(2).max(1);
        lines.extend(panel);
        lines.extend(images);
        (lines, region(2, content_rows, 1, 1 + content_width))
    }
}
