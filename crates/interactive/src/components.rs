//! Conversation components of packages/coding-agent/src/modes/interactive/components: user and assistant messages,
//! tool execution, bash execution, compaction and branch summaries, the footer, diff rendering, and the theme adapters
//! they use. Components render plain view data (no engine types) and are rebuilt on every render.

use std::rc::Rc;

use serde_json::Value;

use crate::word_diff::diff_words;
use jsrt::{number, string};
use theme::styles::TextStyle;
use theme::{Bg, Fg, Theme};
use tui::KeybindingsManager;
use tui::markdown_tokens;
use tui::{Component, Container, Panel, Spacer, Text};
use tui::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use tui::{Loader, StyleFn};
use tui::{image_dimensions, image_fallback};
use tui::{strip_terminal_sequences, truncate_to_width, visible_width};

const OSC133_ZONE_START: &str = "\x1b]133;A\x07";
const OSC133_ZONE_END_FINAL: &str = "\x1b]133;B\x07\x1b]133;C\x07";

/// chalk's modifiers at level > 0 ([`TextStyle::apply`]): the components are drawn only where colors are.
pub fn bold(s: &str) -> String {
    TextStyle::Bold.apply(s, 1)
}

pub fn italic(s: &str) -> String {
    TextStyle::Italic.apply(s, 1)
}

pub fn underline(s: &str) -> String {
    TextStyle::Underline.apply(s, 1)
}

pub fn inverse(s: &str) -> String {
    TextStyle::Inverse.apply(s, 1)
}

pub fn strikethrough(s: &str) -> String {
    TextStyle::Strikethrough.apply(s, 1)
}

fn fg_fn(theme: &Rc<Theme>, color: Fg) -> StyleFn {
    let t = Rc::clone(theme);
    Box::new(move |s| t.fg(color, s))
}

fn bg_fn(theme: &Rc<Theme>, color: Bg) -> StyleFn {
    let t = Rc::clone(theme);
    Box::new(move |s| t.bg(color, s))
}

/// `getMarkdownTheme()`. Code blocks are colored `mdCodeBlock` line by line; cli-highlight syntax coloring for known
/// languages is not ported.
pub fn markdown_theme(theme: &Rc<Theme>) -> MarkdownTheme {
    markdown_theme_with(theme, None)
}

/// [`markdown_theme`] with fenced code blocks drawn by `code_highlight` (the Rust-only syntect highlighting).
pub fn markdown_theme_with(theme: &Rc<Theme>, code_highlight: Option<&crate::qol::CodeHighlight>) -> MarkdownTheme {
    let t = Rc::clone(theme);
    let highlight_code: tui::HighlightFn = match code_highlight {
        Some(h) => {
            let h = Rc::clone(h);
            Box::new(move |code: &str, lang: Option<&str>| h(code, lang))
        }
        None => Box::new(move |code: &str, _lang: Option<&str>| {
            code.split('\n').map(|l| t.fg(Fg::MdCodeBlock, l)).collect()
        }),
    };
    MarkdownTheme {
        heading: fg_fn(theme, Fg::MdHeading),
        link: fg_fn(theme, Fg::MdLink),
        link_url: fg_fn(theme, Fg::MdLinkUrl),
        code: fg_fn(theme, Fg::MdCode),
        code_block: fg_fn(theme, Fg::MdCodeBlock),
        code_block_border: fg_fn(theme, Fg::MdCodeBlockBorder),
        quote: fg_fn(theme, Fg::MdQuote),
        quote_border: fg_fn(theme, Fg::MdQuoteBorder),
        hr: fg_fn(theme, Fg::MdHr),
        list_bullet: fg_fn(theme, Fg::MdListBullet),
        bold: Box::new(bold),
        italic: Box::new(italic),
        strikethrough: Box::new(strikethrough),
        underline: Box::new(underline),
        highlight_code: Some(highlight_code),
        code_block_indent: crate::render_settings::code_block_indent(),
    }
}

fn markdown(
    theme: &Rc<Theme>,
    text: &str,
    padding_x: usize,
    style: Option<DefaultTextStyle>,
    options: MarkdownOptions,
) -> Markdown {
    markdown_with(theme, text, padding_x, style, options, None)
}

fn markdown_with(
    theme: &Rc<Theme>,
    text: &str,
    padding_x: usize,
    style: Option<DefaultTextStyle>,
    options: MarkdownOptions,
    code_highlight: Option<&crate::qol::CodeHighlight>,
) -> Markdown {
    Markdown {
        tokens: markdown_tokens(text),
        padding_x,
        padding_y: 0,
        theme: markdown_theme_with(theme, code_highlight),
        style,
        options: MarkdownOptions {
            hyperlinks: options.hyperlinks || crate::render_settings::hyperlinks(),
            ..options
        },
    }
}

fn colored(theme: &Rc<Theme>, color: Fg) -> Option<DefaultTextStyle> {
    Some(DefaultTextStyle {
        color: Some(fg_fn(theme, color)),
        ..DefaultTextStyle::default()
    })
}

fn zone(mut lines: Vec<String>) -> Vec<String> {
    if let Some(first) = lines.first_mut() {
        first.insert_str(0, OSC133_ZONE_START);
    }
    if let Some(last) = lines.last_mut() {
        last.insert_str(0, OSC133_ZONE_END_FINAL);
    }
    lines
}

/// `formatKeyText`: `alt` reads `option` on macOS; `capitalize` upper-cases each part's first letter.
pub fn format_key_text(key: &str, capitalize: bool, darwin: bool) -> String {
    key.split('/')
        .map(|k| {
            k.split('+')
                .map(|part| {
                    let p = if darwin && part.eq_ignore_ascii_case("alt") {
                        "option"
                    } else {
                        part
                    };
                    let mut c = p.chars();
                    match c.next() {
                        Some(f) if capitalize => f.to_uppercase().chain(c).collect(),
                        _ => p.to_string(),
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `keyText(keybinding)`.
pub fn key_text(keys: &KeybindingsManager, keybinding: &str, darwin: bool) -> String {
    format_key_text(&keys.get_keys(keybinding).join("/"), false, darwin)
}

/// `keyHint(keybinding, description)` given the formatted key text.
pub fn key_hint(theme: &Theme, key: &str, description: &str) -> String {
    format!(
        "{}{}",
        theme.fg(Fg::Dim, key),
        theme.fg(Fg::Muted, &format!(" {description}"))
    )
}

/// `truncateToVisualLines`: the last `max` lines of `text` wrapped at `width`, and how many were dropped.
pub fn truncate_to_visual_lines(text: &str, max: usize, width: usize, padding_x: usize) -> (Vec<String>, usize) {
    if text.is_empty() {
        return (Vec::new(), 0);
    }
    let all = Text::new(text, padding_x, 0, None).render(width);
    if all.len() <= max {
        return (all, 0);
    }
    let skipped = all.len() - max;
    (all[skipped..].to_vec(), skipped)
}

/// `DynamicBorder`: one full-width rule.
pub fn dynamic_border(color: &dyn Fn(&str) -> String, width: usize) -> Vec<String> {
    vec![color(&"─".repeat(width.max(1)))]
}

/// `UserMessageComponent`.
pub struct UserMessage {
    pub theme: Rc<Theme>,
    pub text: String,
    pub output_pad: usize,
}

impl Component for UserMessage {
    fn render(&mut self, width: usize) -> Vec<String> {
        let options = MarkdownOptions {
            preserve_ordered_list_markers: true,
            preserve_backslash_escapes: true,
            hyperlinks: false,
            raw_latex: false,
        };
        let md = markdown(
            &self.theme,
            &self.text,
            0,
            colored(&self.theme, Fg::UserMessageText),
            options,
        );
        let mut panel = Panel {
            children: vec![Box::new(md)],
            padding_x: self.output_pad,
            padding_y: 1,
            bg: Some(bg_fn(&self.theme, Bg::UserMessageBg)),
        };
        zone(panel.render(width))
    }
}

/// One block of an assistant message.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistantBlock {
    Text(String),
    Thinking(String),
    ToolCall,
}

/// What `AssistantMessageComponent` reads from an assistant message.
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantView {
    pub content: Vec<AssistantBlock>,
    /// `stop`, `length`, `toolUse`, `error` or `aborted`.
    pub stop_reason: String,
    pub error_message: Option<String>,
}

/// `AssistantMessageComponent`: text, thinking runs (hidden behind a label when `hide_thinking`), and the
/// truncation/abort/error notice. `thinking_overrides` holds the runs a click toggled.
pub struct AssistantMessage {
    pub theme: Rc<Theme>,
    pub message: AssistantView,
    pub hide_thinking: bool,
    pub hidden_thinking_label: String,
    pub output_pad: usize,
    pub thinking_overrides: Vec<(usize, bool)>,
    /// Fenced code highlighting (Rust-only; `None` draws code blocks in `mdCodeBlock` like TS).
    pub code_highlight: Option<crate::qol::CodeHighlight>,
}

fn visible(b: &AssistantBlock) -> bool {
    match b {
        AssistantBlock::Text(t) | AssistantBlock::Thinking(t) => !string::trim(t).is_empty(),
        AssistantBlock::ToolCall => false,
    }
}

/// The rows of one thinking run in a rendered assistant message (its `MouseRegion`): a left click shows or hides it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThinkingRows {
    pub run: usize,
    pub start: usize,
    pub len: usize,
    /// Hidden behind the label when rendered.
    pub hidden: bool,
}

impl Component for AssistantMessage {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.render_with_thinking_rows(width).0
    }
}

impl AssistantMessage {
    /// [`Component::render`], with the rows each thinking run took.
    pub fn render_with_thinking_rows(&mut self, width: usize) -> (Vec<String>, Vec<ThinkingRows>) {
        let theme = &self.theme;
        let pad = self.output_pad;
        let content = &self.message.content;
        let mut c = Container::default();
        // The child index of each thinking run's component.
        let mut runs: Vec<(usize, usize, bool)> = Vec::new();
        if content.iter().any(visible) {
            c.children.push(Box::new(Spacer(1)));
        }
        let mut run = 0;
        let mut i = 0;
        while i < content.len() {
            match &content[i] {
                AssistantBlock::Text(t) if !string::trim(t).is_empty() => {
                    c.children.push(Box::new(markdown_with(
                        theme,
                        string::trim(t),
                        pad,
                        None,
                        MarkdownOptions::default(),
                        self.code_highlight.as_ref(),
                    )));
                }
                AssistantBlock::Thinking(_) => {
                    let mut blocks = Vec::new();
                    while let Some(AssistantBlock::Thinking(t)) = content.get(i) {
                        if !string::trim(t).is_empty() {
                            blocks.push(string::trim(t));
                        }
                        i += 1;
                    }
                    i -= 1;
                    if !blocks.is_empty() {
                        let index = run;
                        run += 1;
                        let hidden = self
                            .thinking_overrides
                            .iter()
                            .find(|(r, _)| *r == index)
                            .map_or(self.hide_thinking, |(_, h)| *h);
                        runs.push((c.children.len(), index, hidden));
                        if hidden {
                            let label = italic(&theme.fg(Fg::ThinkingText, &self.hidden_thinking_label));
                            c.children.push(Box::new(Text::new(&label, pad, 0, None)));
                        } else {
                            let style = DefaultTextStyle {
                                color: Some(fg_fn(theme, Fg::ThinkingText)),
                                italic: true,
                                ..DefaultTextStyle::default()
                            };
                            let md = markdown(
                                theme,
                                &blocks.join("\n\n"),
                                pad,
                                Some(style),
                                MarkdownOptions::default(),
                            );
                            c.children.push(Box::new(md));
                        }
                        if content[i + 1..].iter().any(visible) {
                            c.children.push(Box::new(Spacer(1)));
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let has_tool_calls = content.contains(&AssistantBlock::ToolCall);
        let error = |text: String| Text::new(&theme.fg(Fg::Error, &text), pad, 0, None);
        let notice = match self.message.stop_reason.as_str() {
            "length" => Some(error("Response was truncated before completion.".into())),
            "aborted" if !has_tool_calls => Some(error(
                match self.message.error_message.as_deref() {
                    Some(m) if !m.is_empty() && m != "Request was aborted" => m,
                    _ => "Operation aborted",
                }
                .to_string(),
            )),
            "error" if !has_tool_calls => Some(error(format!(
                "Error: {}",
                self.message
                    .error_message
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("Unknown error")
            ))),
            _ => None,
        };
        if let Some(n) = notice {
            c.children.push(Box::new(Spacer(1)));
            c.children.push(Box::new(n));
        }
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        for (i, child) in c.children.iter_mut().enumerate() {
            let rendered = child.render(width);
            if let Some(&(_, run, hidden)) = runs.iter().find(|(child, ..)| *child == i) {
                rows.push(ThinkingRows {
                    run,
                    start: lines.len(),
                    len: rendered.len(),
                    hidden,
                });
            }
            lines.extend(rendered);
        }
        if has_tool_calls || lines.is_empty() {
            (lines, rows)
        } else {
            (zone(lines), rows)
        }
    }
}

/// `CompactionSummaryMessageComponent` and `BranchSummaryMessageComponent`: a labelled box, collapsed to a one-line
/// hint or expanded to the Markdown summary.
pub struct SummaryMessage {
    pub theme: Rc<Theme>,
    /// `Some(tokensBefore)` for a compaction summary, `None` for a branch summary.
    pub tokens_before: Option<u64>,
    pub summary: String,
    pub expanded: bool,
    /// Formatted `app.tools.expand` key text.
    pub expand_key: String,
}

/// `Number.prototype.toLocaleString()` in en-US for an integer.
pub fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Component for SummaryMessage {
    fn render(&mut self, width: usize) -> Vec<String> {
        let theme = &self.theme;
        let tokens = self.tokens_before.map(group_thousands);
        let tag = if tokens.is_some() { "compaction" } else { "branch" };
        let label = theme.fg(Fg::CustomMessageLabel, &format!("\x1b[1m[{tag}]\x1b[22m"));
        let mut children: Vec<Box<dyn Component>> = vec![Box::new(Text::new(&label, 0, 0, None)), Box::new(Spacer(1))];
        if self.expanded {
            let header = match &tokens {
                Some(t) => format!("**Compacted from {t} tokens**\n\n"),
                None => "**Branch Summary**\n\n".to_string(),
            };
            let text = format!("{header}{}", self.summary);
            let md = markdown(
                theme,
                &text,
                0,
                colored(theme, Fg::CustomMessageText),
                MarkdownOptions::default(),
            );
            children.push(Box::new(md));
        } else {
            let lead = match &tokens {
                Some(t) => format!("Compacted from {t} tokens ("),
                None => "Branch summary (".to_string(),
            };
            let text = format!(
                "{}{}{}",
                theme.fg(Fg::CustomMessageText, &lead),
                theme.fg(Fg::Dim, &self.expand_key),
                theme.fg(Fg::CustomMessageText, " to expand)")
            );
            children.push(Box::new(Text::new(&text, 0, 0, None)));
        }
        Panel {
            children,
            padding_x: 1,
            padding_y: 1,
            bg: Some(bg_fn(theme, Bg::CustomMessageBg)),
        }
        .render(width)
    }
}

/// `^([+-\s])(\s*\d*)\s(.*)$` on one diff line: prefix, line number, content.
fn parse_diff_line(line: &str) -> Option<(char, &str, &str)> {
    let mut chars = line.chars();
    let prefix = chars
        .next()
        .filter(|c| *c == '+' || *c == '-' || string::is_space(*c))?;
    let rest = chars.as_str();
    let ws = rest.len() - rest.trim_start_matches(string::is_space).len();
    let digits = rest[ws..].bytes().take_while(u8::is_ascii_digit).count();
    let after = rest[ws + digits..].chars().next();
    let split = if after.is_some_and(string::is_space) {
        ws + digits
    } else if ws > 0 {
        rest[..ws].char_indices().last().map(|(i, _)| i)?
    } else {
        return None;
    };
    let sep = rest[split..].chars().next()?;
    let content = &rest[split + sep.len_utf8()..];
    if content.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((prefix, &rest[..split], content))
}

fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start_matches(string::is_space).len()]
}

fn intra_line_diff(old: &str, new: &str) -> (String, String) {
    let (mut removed, mut added) = (String::new(), String::new());
    let (mut first_removed, mut first_added) = (true, true);
    for part in diff_words(old, new) {
        let (line, first) = if part.removed {
            (&mut removed, &mut first_removed)
        } else if part.added {
            (&mut added, &mut first_added)
        } else {
            removed.push_str(&part.value);
            added.push_str(&part.value);
            continue;
        };
        let mut value = part.value.as_str();
        if *first {
            let ws = leading_ws(value);
            line.push_str(ws);
            value = &value[ws.len()..];
            *first = false;
        }
        if !value.is_empty() {
            line.push_str(&inverse(value));
        }
    }
    (removed, added)
}

/// `renderDiff`: colored diff lines, with word-level inverse highlighting for a single changed line.
pub fn render_diff(theme: &Theme, diff: &str) -> String {
    let lines: Vec<&str> = diff.split('\n').collect();
    let tabs = |s: &str| s.replace('\t', "   ");
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((prefix, num, content)) = parse_diff_line(lines[i]) else {
            out.push(theme.fg(Fg::ToolDiffContext, lines[i]));
            i += 1;
            continue;
        };
        if prefix == '-' {
            let mut take = |want: char| {
                let mut run = Vec::new();
                while let Some((p, n, c)) = lines.get(i).and_then(|l| parse_diff_line(l)) {
                    if p != want {
                        break;
                    }
                    run.push((n, c));
                    i += 1;
                }
                run
            };
            let removed = take('-');
            let added = take('+');
            if let ([(rn, rc)], [(an, ac)]) = (removed.as_slice(), added.as_slice()) {
                let (r, a) = intra_line_diff(&tabs(rc), &tabs(ac));
                out.push(theme.fg(Fg::ToolDiffRemoved, &format!("-{rn} {r}")));
                out.push(theme.fg(Fg::ToolDiffAdded, &format!("+{an} {a}")));
            } else {
                for (n, c) in &removed {
                    out.push(theme.fg(Fg::ToolDiffRemoved, &format!("-{n} {}", tabs(c))));
                }
                for (n, c) in &added {
                    out.push(theme.fg(Fg::ToolDiffAdded, &format!("+{n} {}", tabs(c))));
                }
            }
        } else if prefix == '+' {
            out.push(theme.fg(Fg::ToolDiffAdded, &format!("+{num} {}", tabs(content))));
            i += 1;
        } else {
            out.push(theme.fg(Fg::ToolDiffContext, &format!(" {num} {}", tabs(content))));
            i += 1;
        }
    }
    out.join("\n")
}

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
        // `Box.handleMouse`: inside the padding only.
        let content_rows = panel.len().saturating_sub(2);
        let content_width = width.saturating_sub(2).max(1);
        lines.extend(panel);
        lines.extend(images);
        (lines, region(2, content_rows, 1, 1 + content_width))
    }
}

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

    /// `setComplete`.
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

    /// Advance the running spinner one frame.
    pub fn tick(&mut self) {
        self.loader.tick();
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
        if !available.is_empty() {
            let shown = if self.expanded {
                &available[..]
            } else {
                &available[hidden..]
            };
            let text = format!(
                "\n{}",
                shown.iter().map(|l| t.fg(Fg::Muted, l)).collect::<Vec<_>>().join("\n")
            );
            if self.expanded {
                lines.extend(Text::new(&text, 1, 0, None).render(width));
            } else {
                lines.extend(truncate_to_visual_lines(&text, PREVIEW_LINES, width, 1).0);
            }
        }
        if self.status == BashStatus::Running {
            lines.extend(self.loader.render(width));
        } else {
            let mut parts = Vec::new();
            if hidden > 0 {
                parts.push(if self.expanded {
                    format!(
                        "{}{}{}",
                        t.fg(Fg::Muted, "("),
                        key_hint(&t, &self.expand_key, "to collapse"),
                        t.fg(Fg::Muted, ")")
                    )
                } else {
                    format!(
                        "{}{}{}",
                        t.fg(Fg::Muted, &format!("... {hidden} more lines (")),
                        key_hint(&t, &self.expand_key, "to expand"),
                        t.fg(Fg::Muted, ")")
                    )
                });
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
            if !parts.is_empty() {
                lines.extend(Text::new(&format!("\n{}", parts.join("\n")), 1, 0, None).render(width));
            }
        }
        lines.extend(dynamic_border(&border, width));
        lines
    }
}

/// The model as the footer shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FooterModel {
    pub id: String,
    pub provider: String,
    pub reasoning: bool,
    pub context_window: u64,
}

/// What `FooterComponent` reads from the session: usage totals summed over all entries, the latest assistant cache
/// hit rate, context usage, and the footer-data provider's branch, provider count and extension statuses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FooterView {
    pub cwd: String,
    pub home: Option<String>,
    pub git_branch: Option<String>,
    pub session_name: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub latest_cache_hit_rate: Option<f64>,
    /// `None` when there is no context usage; `Some(None)` when its percent is unknown (after compaction).
    pub context_percent: Option<Option<f64>>,
    /// `contextUsage.contextWindow` when known.
    pub context_window: Option<u64>,
    pub model: Option<FooterModel>,
    pub thinking_level: String,
    pub using_subscription: bool,
    pub provider_count: usize,
    pub auto_compact: bool,
    pub experimental: bool,
    /// Extension statuses by key.
    pub statuses: Vec<(String, String)>,
}

/// `formatTokens`.
pub fn format_tokens(count: u64) -> String {
    let c = count as f64;
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{}k", number::to_fixed(c / 1000.0, 1)),
        10_000..1_000_000 => format!("{}k", (c / 1000.0).round()),
        1_000_000..10_000_000 => format!("{}M", number::to_fixed(c / 1_000_000.0, 1)),
        _ => format!("{}M", (c / 1_000_000.0).round()),
    }
}

/// `formatCwdForFooter` for absolute POSIX paths: `~` for the home directory and paths under it.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return cwd.to_string();
    };
    let norm = |p: &str| {
        let t = p.trim_end_matches('/');
        if t.is_empty() { "/".to_string() } else { t.to_string() }
    };
    let (c, h) = (norm(cwd), norm(home));
    if c == h {
        return "~".to_string();
    }
    let base = if h == "/" { String::new() } else { h };
    match c.strip_prefix(&format!("{base}/")) {
        Some(rest) => format!("~/{rest}"),
        None => cwd.to_string(),
    }
}

fn sanitize_status(text: &str) -> String {
    let spaced = text.replace(['\r', '\n', '\t'], " ");
    let mut out = String::new();
    for c in spaced.chars() {
        if !(c == ' ' && out.ends_with(' ')) {
            out.push(c);
        }
    }
    out.trim_matches(string::is_space).to_string()
}

/// `FooterComponent.render`: the cwd line, the stats line with the model on the right, and extension statuses.
pub fn render_footer(theme: &Theme, v: &FooterView, width: usize) -> Vec<String> {
    let dim = |s: &str| theme.fg(Fg::Dim, s);
    let mut pwd = format_cwd_for_footer(&v.cwd, v.home.as_deref());
    if let Some(b) = v.git_branch.as_deref().filter(|b| !b.is_empty()) {
        pwd = format!("{pwd} ({b})");
    }
    if let Some(n) = v.session_name.as_deref().filter(|n| !n.is_empty()) {
        pwd = format!("{pwd} • {n}");
    }
    let mut stats = Vec::new();
    for (n, tag) in [
        (v.input, "↑"),
        (v.output, "↓"),
        (v.cache_read, "R"),
        (v.cache_write, "W"),
    ] {
        if n > 0 {
            stats.push(format!("{tag}{}", format_tokens(n)));
        }
    }
    if let Some(rate) = v
        .latest_cache_hit_rate
        .filter(|_| v.cache_read > 0 || v.cache_write > 0)
    {
        stats.push(format!("CH{}%", number::to_fixed(rate, 1)));
    }
    let model = v.model.as_ref();
    let subscription = model.is_some_and(|m| m.provider == "kimi-coding" || v.using_subscription);
    if v.cost != 0.0 || subscription {
        stats.push(format!(
            "${}{}",
            number::to_fixed(v.cost, 3),
            if subscription { " (sub)" } else { "" }
        ));
    }
    let window = v.context_window.or(model.map(|m| m.context_window)).unwrap_or(0);
    let percent_value = v.context_percent.flatten().unwrap_or(0.0);
    let claude_code = model.is_some_and(|m| m.provider == "claude-code");
    let auto = if v.auto_compact && !claude_code { " (auto)" } else { "" };
    let display = match v.context_percent {
        Some(None) => format!("?/{}{auto}", format_tokens(window)),
        _ => format!(
            "{}%/{}{auto}",
            number::to_fixed(percent_value, 1),
            format_tokens(window)
        ),
    };
    stats.push(if percent_value > 90.0 {
        theme.fg(Fg::Error, &display)
    } else if percent_value > 70.0 {
        theme.fg(Fg::Warning, &display)
    } else {
        display
    });
    if v.experimental {
        stats.push(format!("{} {}", dim("•"), bold(&theme.fg(Fg::Warning, "xp"))));
    }
    let mut left = stats.join(" ");
    let model_name = match model {
        Some(m) if !m.id.is_empty() && claude_code => format!("{} (Claude Code)", m.id),
        Some(m) if !m.id.is_empty() => m.id.clone(),
        _ => "no-model".to_string(),
    };
    let mut left_width = visible_width(&left);
    if left_width > width {
        left = truncate_to_width(&left, width, "...", false);
        left_width = visible_width(&left);
    }
    let mut right_plain = model_name.clone();
    if model.is_some_and(|m| m.reasoning) {
        let level = if v.thinking_level.is_empty() {
            "off"
        } else {
            v.thinking_level.as_str()
        };
        right_plain = if level == "off" {
            format!("{model_name} • thinking off")
        } else {
            format!("{model_name} • {level}")
        };
    }
    let mut right = right_plain.clone();
    if let Some(m) = model.filter(|_| v.provider_count > 1 && !claude_code) {
        right = format!("({}) {right_plain}", m.provider);
        if left_width + 2 + visible_width(&right) > width {
            right = right_plain;
        }
    }
    let right_width = visible_width(&right);
    let line = if left_width + 2 + right_width <= width {
        format!("{left}{}{right}", " ".repeat(width - left_width - right_width))
    } else if width > left_width + 2 {
        let r = truncate_to_width(&right, width - left_width - 2, "", false);
        format!(
            "{left}{}{r}",
            " ".repeat(width.saturating_sub(left_width + visible_width(&r)))
        )
    } else {
        left.clone()
    };
    let remainder = &line[left.len()..];
    let mut lines = vec![
        truncate_to_width(&dim(&pwd), width, &dim("..."), false),
        format!("{}{}", dim(&left), dim(remainder)),
    ];
    if !v.statuses.is_empty() {
        let mut sorted = v.statuses.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let line = sorted
            .iter()
            .map(|(_, t)| sanitize_status(t))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(truncate_to_width(&line, width, &dim("..."), false));
    }
    lines
}

/// A select list's theme: accented selection, muted descriptions, and the scroll and no-match lines in their colors.
pub(crate) fn select_theme(t: &Rc<Theme>, scroll: Fg, no_match: Fg) -> tui::SelectListTheme {
    let style = |color: Fg| {
        let t = Rc::clone(t);
        Box::new(move |s: &str| t.fg(color, s)) as Box<dyn Fn(&str) -> String>
    };
    tui::SelectListTheme {
        selected_prefix: style(Fg::Accent),
        selected_text: style(Fg::Accent),
        description: style(Fg::Muted),
        scroll_info: style(scroll),
        no_match: style(no_match),
    }
}
