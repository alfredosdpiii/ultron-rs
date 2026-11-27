//! Conversation components of packages/coding-agent/src/modes/interactive/components: user and assistant messages,
//! tool execution, bash execution, compaction and branch summaries, the footer, diff rendering, and the theme adapters
//! they use. Components render plain view data (no engine types) and are rebuilt on every render.

mod bash;
mod diff;
mod footer;
mod tool;

pub use bash::{BashExecution, BashStatus, truncate_tail};
pub use diff::render_diff;
pub use footer::{FooterModel, FooterView, format_cwd_for_footer, format_tokens, render_footer};
pub use tool::{
    CallRenderer, FileChange, InlineImage, InlineImages, ResultRenderer, RowRegion, ToolContent, ToolExecution,
    ToolRenderState, ToolRenderers, ToolResultView, tool_text_output_with,
};

use std::rc::Rc;

use jsrt::string;
use theme::styles::TextStyle;
use theme::{Bg, Fg, Theme};
use tui::KeybindingsManager;
use tui::StyleFn;
use tui::markdown_tokens;
use tui::{Component, Container, Panel, Spacer, Text};
use tui::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};

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

pub(crate) fn fg_fn(theme: &Rc<Theme>, color: Fg) -> StyleFn {
    let t = Rc::clone(theme);
    Box::new(move |s| t.fg(color, s))
}

pub(crate) fn bg_fn(theme: &Rc<Theme>, color: Bg) -> StyleFn {
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
        underline_links: !crate::render_settings::plain_links(),
        link_targets: crate::render_settings::link_targets(),
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

/// `formatKeyText`: `alt` reads `option` on macOS (`process.platform`); `capitalize` upper-cases each part's first
/// letter.
pub fn format_key_text(key: &str, capitalize: bool) -> String {
    let darwin = jsrt::process::platform() == "darwin";
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

pub fn key_text(keys: &KeybindingsManager, keybinding: &str) -> String {
    format_key_text(&keys.get_keys(keybinding).join("/"), false)
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

fn collect_thinking_run(content: &[AssistantBlock], start: usize) -> (Vec<&str>, usize) {
    let mut blocks = Vec::new();
    let mut i = start;
    while let Some(AssistantBlock::Thinking(t)) = content.get(i) {
        if !string::trim(t).is_empty() {
            blocks.push(string::trim(t));
        }
        i += 1;
    }
    (blocks, i - 1)
}

fn render_assistant_children(
    children: &mut [Box<dyn Component>],
    runs: &[(usize, usize, bool)],
    width: usize,
) -> (Vec<String>, Vec<ThinkingRows>) {
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    for (i, child) in children.iter_mut().enumerate() {
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
    (lines, rows)
}

impl AssistantMessage {
    fn is_thinking_hidden(&self, run_index: usize) -> bool {
        self.thinking_overrides
            .iter()
            .find(|(r, _)| *r == run_index)
            .map_or(self.hide_thinking, |(_, h)| *h)
    }

    fn thinking_component(&self, blocks: &[&str], hidden: bool) -> Box<dyn Component> {
        if hidden {
            let label = italic(&self.theme.fg(Fg::ThinkingText, &self.hidden_thinking_label));
            return Box::new(Text::new(&label, self.output_pad, 0, None));
        }
        let style = DefaultTextStyle {
            color: Some(fg_fn(&self.theme, Fg::ThinkingText)),
            italic: true,
            ..DefaultTextStyle::default()
        };
        Box::new(markdown(
            &self.theme,
            &blocks.join("\n\n"),
            self.output_pad,
            Some(style),
            MarkdownOptions::default(),
        ))
    }

    fn stop_notice(&self, has_tool_calls: bool) -> Option<Text> {
        let message = match self.message.stop_reason.as_str() {
            "length" => "Response was truncated before completion.".to_string(),
            "aborted" if !has_tool_calls => match self.message.error_message.as_deref() {
                Some(m) if !m.is_empty() && m != "Request was aborted" => m.to_string(),
                _ => "Operation aborted".to_string(),
            },
            "error" if !has_tool_calls => format!(
                "Error: {}",
                self.message
                    .error_message
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("Unknown error")
            ),
            _ => return None,
        };
        Some(Text::new(&self.theme.fg(Fg::Error, &message), self.output_pad, 0, None))
    }

    fn push_thinking_run(
        &self,
        c: &mut Container,
        runs: &mut Vec<(usize, usize, bool)>,
        run: &mut usize,
        blocks: &[&str],
        has_more_visible: bool,
    ) {
        if blocks.is_empty() {
            return;
        }
        let hidden = self.is_thinking_hidden(*run);
        runs.push((c.children.len(), *run, hidden));
        *run += 1;
        c.children.push(self.thinking_component(blocks, hidden));
        if has_more_visible {
            c.children.push(Box::new(Spacer(1)));
        }
    }

    /// [`Component::render`], with the rows each thinking run took.
    pub fn render_with_thinking_rows(&mut self, width: usize) -> (Vec<String>, Vec<ThinkingRows>) {
        let content = &self.message.content;
        let mut c = Container::default();
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
                        &self.theme,
                        string::trim(t),
                        self.output_pad,
                        None,
                        MarkdownOptions::default(),
                        self.code_highlight.as_ref(),
                    )));
                }
                AssistantBlock::Thinking(_) => {
                    let (blocks, last) = collect_thinking_run(content, i);
                    i = last;
                    let more = content[i + 1..].iter().any(visible);
                    self.push_thinking_run(&mut c, &mut runs, &mut run, &blocks, more);
                }
                _ => {}
            }
            i += 1;
        }
        let has_tool_calls = content.contains(&AssistantBlock::ToolCall);
        if let Some(n) = self.stop_notice(has_tool_calls) {
            c.children.push(Box::new(Spacer(1)));
            c.children.push(Box::new(n));
        }
        let (lines, rows) = render_assistant_children(&mut c.children, &runs, width);
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

/// A select list's theme: accented selection, muted descriptions, and the scroll and no-match lines in their colors.
pub fn select_theme(t: &Rc<Theme>, scroll: Fg, no_match: Fg) -> tui::SelectListTheme {
    let style = |color: Fg| {
        let t = Rc::clone(t);
        Box::new(move |s: &str| t.fg(color, s)) as Box<dyn Fn(&str) -> String>
    };
    tui::SelectListTheme {
        selected_text: style(Fg::Accent),
        description: style(Fg::Muted),
        scroll_info: style(scroll),
        no_match: style(no_match),
    }
}
