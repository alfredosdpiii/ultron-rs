//! Markdown rendering from marked's token tree: port of the renderer half of packages/tui/src/components/markdown.ts
//! (headings, paragraphs, code blocks with a highlight hook, lists, tables, blockquotes, links, inline styles,
//! wrapping and padding). Tokens use marked's shape; the lexer is not ported here.

use crate::components::Component;
use crate::frame::is_image_line;
use crate::image::hyperlink;
use crate::select::StyleFn;
use crate::utils::{apply_background_to_line, clean_link_url, link_target, visible_width, wrap_text_with_ansi};

/// One marked token (block or inline).
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Space,
    Heading {
        depth: usize,
        tokens: Vec<Token>,
    },
    Paragraph {
        tokens: Vec<Token>,
    },
    /// Block or inline text; list-item text carries nested inline tokens.
    Text {
        text: String,
        tokens: Option<Vec<Token>>,
    },
    Code {
        raw: String,
        lang: Option<String>,
        text: String,
    },
    List {
        ordered: bool,
        start: Option<i64>,
        loose: bool,
        items: Vec<ListItem>,
    },
    Table {
        raw: String,
        header: Vec<Vec<Token>>,
        rows: Vec<Vec<Vec<Token>>>,
    },
    Blockquote {
        tokens: Vec<Token>,
    },
    Hr,
    Html {
        raw: String,
    },
    /// `latexBlock` / `latex`: rendered through `crate::latex` (raw source when pending or unsupported).
    LatexBlock {
        raw: String,
    },
    Latex {
        raw: String,
    },
    Escape {
        raw: String,
        text: String,
    },
    Strong {
        tokens: Vec<Token>,
    },
    Em {
        tokens: Vec<Token>,
    },
    Codespan {
        text: String,
    },
    Link {
        href: String,
        text: String,
        tokens: Vec<Token>,
    },
    Br,
    Del {
        tokens: Vec<Token>,
    },
    /// Any other token: its `text`, if any.
    Other {
        kind: String,
        text: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    pub raw: String,
    pub task: bool,
    pub checked: bool,
    pub tokens: Vec<Token>,
}

pub type HighlightFn = Box<dyn Fn(&str, Option<&str>) -> Vec<String>>;

pub struct MarkdownTheme {
    pub heading: StyleFn,
    pub link: StyleFn,
    pub link_url: StyleFn,
    pub code: StyleFn,
    pub code_block: StyleFn,
    pub code_block_border: StyleFn,
    pub quote: StyleFn,
    pub quote_border: StyleFn,
    pub hr: StyleFn,
    pub list_bullet: StyleFn,
    pub bold: StyleFn,
    pub italic: StyleFn,
    pub strikethrough: StyleFn,
    pub underline: StyleFn,
    pub highlight_code: Option<HighlightFn>,
    pub code_block_indent: Option<String>,
    /// Links get `underline` inside `link`, as in TypeScript. Off, a link keeps only its colors (the Rust TUI
    /// underlines the one under the pointer instead).
    pub underline_links: bool,
    /// Without [`MarkdownOptions::hyperlinks`], each link still carries its URL as a [`crate::link_target`] (the text
    /// shown is the same), so the fullscreen viewport opens and underlines the whole link from any row it wraps onto.
    /// The application strips the targets from what it writes ([`crate::strip_link_targets`]). Off, as in TypeScript.
    pub link_targets: bool,
}

#[derive(Default)]
pub struct DefaultTextStyle {
    pub color: Option<StyleFn>,
    pub bg_color: Option<StyleFn>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MarkdownOptions {
    pub preserve_ordered_list_markers: bool,
    pub preserve_backslash_escapes: bool,
    /// Terminal supports OSC 8 (TS reads getCapabilities().hyperlinks).
    pub hyperlinks: bool,
    /// Show LaTeX math as its source instead of rendering it (TS `renderLatex: false`).
    pub raw_latex: bool,
}

/// Rendered markdown from a token tree, with padding and an optional default text style.
pub struct Markdown {
    pub tokens: Vec<Token>,
    pub padding_x: usize,
    pub padding_y: usize,
    pub theme: MarkdownTheme,
    pub style: Option<DefaultTextStyle>,
    pub options: MarkdownOptions,
}

struct Ctx<'a> {
    apply: &'a dyn Fn(&str) -> String,
    prefix: String,
}

fn style_prefix(f: &dyn Fn(&str) -> String) -> String {
    let styled = f("\0");
    styled.find('\0').map_or(String::new(), |i| styled[..i].to_string())
}

fn sub(a: usize, b: usize) -> usize {
    a.saturating_sub(b)
}

/// `^ {0,3}(\d{1,9}[.)])[ \t]+`
fn ordered_marker(raw: &str) -> Option<String> {
    let rest = raw.trim_start_matches(' ');
    if raw.len() - rest.len() > 3 {
        return None;
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let delim = *rest.as_bytes().get(digits)?;
    let after = rest.as_bytes().get(digits + 1)?;
    ((1..=9).contains(&digits) && (delim == b'.' || delim == b')') && (*after == b' ' || *after == b'\t'))
        .then(|| format!("{} ", &rest[..=digits]))
}

/// `^ {0,3}([-+*])(?:[ \t]+|(?=\r?\n|$))`
fn unordered_marker(raw: &str) -> Option<String> {
    let rest = raw.trim_start_matches(' ');
    if raw.len() - rest.len() > 3 {
        return None;
    }
    let mut chars = rest.chars();
    let m = chars.next().filter(|c| matches!(c, '-' | '+' | '*'))?;
    let after = chars.as_str();
    (after.is_empty() || after.starts_with([' ', '\t', '\n']) || after.starts_with("\r\n")).then(|| format!("{m} "))
}

impl Markdown {
    fn apply_default(&self, text: &str) -> String {
        let Some(s) = &self.style else {
            return text.to_string();
        };
        let mut t = s.color.as_ref().map_or_else(|| text.to_string(), |c| c(text));
        for (on, f) in [
            (s.bold, &self.theme.bold),
            (s.italic, &self.theme.italic),
            (s.strikethrough, &self.theme.strikethrough),
            (s.underline, &self.theme.underline),
        ] {
            if on {
                t = f(&t);
            }
        }
        t
    }

    fn render_lines(&self, width: usize) -> Vec<String> {
        let content_width = sub(width, self.padding_x * 2).max(1);
        let mut rendered = Vec::new();
        for (i, t) in self.tokens.iter().enumerate() {
            rendered.extend(self.render_token(t, content_width, self.tokens.get(i + 1), None));
        }
        let mut wrapped = Vec::new();
        for line in rendered {
            if is_image_line(&line) {
                wrapped.push(line);
            } else {
                wrapped.extend(wrap_text_with_ansi(&line, content_width));
            }
        }
        let margin = " ".repeat(self.padding_x);
        let bg = self.style.as_ref().and_then(|s| s.bg_color.as_ref());
        let pad = |line: &str| match bg {
            Some(bg) => apply_background_to_line(line, width, bg.as_ref()),
            None => format!("{line}{}", " ".repeat(sub(width, visible_width(line)))),
        };
        let content: Vec<String> = wrapped
            .iter()
            .map(|l| {
                if is_image_line(l) {
                    l.clone()
                } else {
                    pad(&format!("{margin}{l}{margin}"))
                }
            })
            .collect();
        let empty = " ".repeat(width);
        let empty = match bg {
            Some(bg) => apply_background_to_line(&empty, width, bg.as_ref()),
            None => empty,
        };
        let mut out = vec![empty.clone(); self.padding_y];
        out.extend(content);
        out.extend(std::iter::repeat_n(empty, self.padding_y));
        if out.is_empty() { vec![String::new()] } else { out }
    }

    fn render_heading(&self, depth: usize, tokens: &[Token], spaced: bool) -> Vec<String> {
        let th = &self.theme;
        let style: Box<dyn Fn(&str) -> String + '_> = if depth == 1 {
            Box::new(|t: &str| (th.heading)(&(th.bold)(&(th.underline)(t))))
        } else {
            Box::new(|t: &str| (th.heading)(&(th.bold)(t)))
        };
        let hctx = Ctx {
            apply: &*style,
            prefix: style_prefix(&*style),
        };
        let text = self.render_inline(tokens, Some(&hctx));
        let mut lines = vec![if depth >= 3 {
            format!("{}{text}", style(&format!("{} ", "#".repeat(depth))))
        } else {
            text
        }];
        if spaced {
            lines.push(String::new());
        }
        lines
    }

    fn render_latex_block(&self, raw: &str, spaced: bool) -> Vec<String> {
        let mut lines: Vec<String> = crate::latex::render_latex_token(raw, true, !self.options.raw_latex)
            .split('\n')
            .map(|line| self.apply_default(line))
            .collect();
        if spaced {
            lines.push(String::new());
        }
        lines
    }

    fn render_code_block(&self, lang: Option<&str>, text: &str, spaced: bool) -> Vec<String> {
        let indent = self.theme.code_block_indent.as_deref().unwrap_or("  ");
        let mut lines = vec![(self.theme.code_block_border)(&format!("```{}", lang.unwrap_or("")))];
        match &self.theme.highlight_code {
            Some(h) => lines.extend(h(text, lang).iter().map(|l| format!("{indent}{l}"))),
            None => lines.extend(
                text.split('\n')
                    .map(|l| format!("{indent}{}", (self.theme.code_block)(l))),
            ),
        }
        lines.push((self.theme.code_block_border)("```"));
        if spaced {
            lines.push(String::new());
        }
        lines
    }

    fn render_blockquote(&self, tokens: &[Token], width: usize, spaced: bool) -> Vec<String> {
        let th = &self.theme;
        let quote = |t: &str| (th.quote)(&(th.italic)(t));
        let prefix = style_prefix(&quote);
        let qwidth = sub(width, 2).max(1);
        let identity = |t: &str| t.to_string();
        let qctx = Ctx {
            apply: &identity,
            prefix: prefix.clone(),
        };
        let mut ql = Vec::new();
        for (i, t) in tokens.iter().enumerate() {
            ql.extend(self.render_token(t, qwidth, tokens.get(i + 1), Some(&qctx)));
        }
        while ql.last().is_some_and(String::is_empty) {
            ql.pop();
        }
        let mut lines = Vec::new();
        for line in ql {
            let styled = if prefix.is_empty() {
                quote(&line)
            } else {
                quote(&line.replace("\x1b[0m", &format!("\x1b[0m{prefix}")))
            };
            for w in wrap_text_with_ansi(&styled, qwidth) {
                lines.push(format!("{}{w}", (th.quote_border)("│ ")));
            }
        }
        if spaced {
            lines.push(String::new());
        }
        lines
    }

    fn render_leaf_token(&self, token: &Token, width: usize, spaced: bool) -> Vec<String> {
        match token {
            Token::Hr => {
                let mut lines = vec![(self.theme.hr)(&"─".repeat(width.min(80)))];
                if spaced {
                    lines.push(String::new());
                }
                lines
            }
            Token::Html { raw } => vec![self.apply_default(raw.trim())],
            Token::Space => vec![String::new()],
            Token::Other { text: Some(t), .. }
            | Token::Escape { text: t, .. }
            | Token::Codespan { text: t }
            | Token::Link { text: t, .. } => vec![t.clone()],
            Token::Latex { raw } => vec![raw.clone()],
            _ => Vec::new(),
        }
    }

    fn render_token(&self, token: &Token, width: usize, next: Option<&Token>, ctx: Option<&Ctx>) -> Vec<String> {
        let spaced = next.is_some_and(|n| !matches!(n, Token::Space));
        match token {
            Token::Heading { depth, tokens } => self.render_heading(*depth, tokens, spaced),
            Token::Paragraph { tokens } => {
                let mut lines = vec![self.render_inline(tokens, ctx)];
                if next.is_some_and(|n| !matches!(n, Token::List { .. } | Token::Space)) {
                    lines.push(String::new());
                }
                lines
            }
            Token::Text { .. } => vec![self.render_inline(std::slice::from_ref(token), ctx)],
            Token::LatexBlock { raw } => self.render_latex_block(raw, spaced),
            Token::Code { lang, text, .. } => self.render_code_block(lang.as_deref(), text, spaced),
            Token::List { .. } => self.render_list(token, 0, width, ctx),
            Token::Table { .. } => self.render_table(token, width, next, ctx),
            Token::Blockquote { tokens } => self.render_blockquote(tokens, width, spaced),
            _ => self.render_leaf_token(token, width, spaced),
        }
    }

    fn apply_ctx_lines(ctx: &Ctx, text: &str) -> String {
        text.split('\n').map(|s| (ctx.apply)(s)).collect::<Vec<_>>().join("\n")
    }

    /// A link: its label as an OSC 8 link, or with the URL after it, or alone where the label is the URL. An `href`
    /// with a control character shows the label alone, since the href would end an OSC 8 sequence early or put raw
    /// escapes on the row.
    fn render_inline_link(&self, href: &str, text: &str, tokens: &[Token], ctx: &Ctx) -> String {
        let p = &ctx.prefix;
        let label = self.render_inline(tokens, Some(ctx));
        let styled = if self.theme.underline_links {
            (self.theme.link)(&(self.theme.underline)(&label))
        } else {
            (self.theme.link)(&label)
        };
        if !clean_link_url(href) {
            return format!("{styled}{p}");
        }
        if self.options.hyperlinks {
            return format!("{}{p}", hyperlink(&styled, href));
        }
        let shown = if text == href || Some(text) == href.strip_prefix("mailto:") {
            styled
        } else {
            format!("{styled}{}", (self.theme.link_url)(&format!(" ({href})")))
        };
        if self.theme.link_targets {
            format!("{}{p}", link_target(&shown, href))
        } else {
            format!("{shown}{p}")
        }
    }

    fn render_inline_text_token(&self, token: &Token, ctx: &Ctx) -> Option<String> {
        match token {
            Token::Latex { raw } => Some(Self::apply_ctx_lines(
                ctx,
                &crate::latex::render_latex_token(raw, false, !self.options.raw_latex),
            )),
            Token::Escape { raw, text } => Some(Self::apply_ctx_lines(
                ctx,
                if self.options.preserve_backslash_escapes {
                    raw
                } else {
                    text
                },
            )),
            Token::Text { tokens: Some(t), .. } if !t.is_empty() => Some(self.render_inline(t, Some(ctx))),
            Token::Text { text, .. } => Some(Self::apply_ctx_lines(ctx, text)),
            Token::Br => Some("\n".to_string()),
            Token::Html { raw } => Some(Self::apply_ctx_lines(ctx, raw)),
            Token::Other { text: Some(t), .. } | Token::Code { text: t, .. } => Some(Self::apply_ctx_lines(ctx, t)),
            _ => None,
        }
    }

    fn render_inline_styled_token(&self, token: &Token, ctx: &Ctx) -> String {
        let p = &ctx.prefix;
        match token {
            Token::Paragraph { tokens } => self.render_inline(tokens, Some(ctx)),
            Token::Strong { tokens } => format!("{}{p}", (self.theme.bold)(&self.render_inline(tokens, Some(ctx)))),
            Token::Em { tokens } => format!("{}{p}", (self.theme.italic)(&self.render_inline(tokens, Some(ctx)))),
            Token::Codespan { text } => format!("{}{p}", (self.theme.code)(text)),
            Token::Link { href, text, tokens } => self.render_inline_link(href, text, tokens, ctx),
            Token::Del { tokens } => format!(
                "{}{p}",
                (self.theme.strikethrough)(&self.render_inline(tokens, Some(ctx)))
            ),
            _ => String::new(),
        }
    }

    fn render_inline(&self, tokens: &[Token], ctx: Option<&Ctx>) -> String {
        let default_apply = |t: &str| self.apply_default(t);
        let owned;
        let ctx = match ctx {
            Some(c) => c,
            None => {
                let prefix = match &self.style {
                    Some(_) => style_prefix(&default_apply),
                    None => String::new(),
                };
                owned = Ctx {
                    apply: &default_apply,
                    prefix,
                };
                &owned
            }
        };
        let p = &ctx.prefix;
        let mut out = String::new();
        for token in tokens {
            if let Some(piece) = self.render_inline_text_token(token, ctx) {
                out += &piece;
            } else {
                out += &self.render_inline_styled_token(token, ctx);
            }
        }
        while !p.is_empty() && out.ends_with(p.as_str()) {
            out.truncate(out.len() - p.len());
        }
        out
    }

    fn list_item_marker(&self, ordered: bool, start: i64, index: usize, item: &ListItem) -> String {
        let preserve = self.options.preserve_ordered_list_markers;
        let bullet = match (ordered, preserve) {
            (true, true) => ordered_marker(&item.raw).unwrap_or_else(|| format!("{}. ", start + index as i64)),
            (true, false) => format!("{}. ", start + index as i64),
            (false, true) => unordered_marker(&item.raw).unwrap_or_else(|| "- ".into()),
            (false, false) => "- ".into(),
        };
        let check = if item.task {
            if item.checked { "[x] " } else { "[ ] " }
        } else {
            ""
        };
        format!("{bullet}{check}")
    }

    #[expect(clippy::too_many_arguments, reason = "internal list item rendering helper")]
    fn render_list_item(
        &self,
        item: &ListItem,
        first: &str,
        cont: &str,
        depth: usize,
        width: usize,
        ctx: Option<&Ctx>,
        lines: &mut Vec<String>,
    ) {
        let item_width = sub(width, visible_width(first)).max(1);
        let mut any = false;
        for t in &item.tokens {
            if let Token::List { .. } = t {
                lines.extend(self.render_list(t, depth + 1, width, ctx));
                any = true;
                continue;
            }
            for line in self.render_token(t, item_width, None, ctx) {
                for w in wrap_text_with_ansi(&line, item_width) {
                    lines.push(format!("{}{w}", if any { cont } else { first }));
                    any = true;
                }
            }
        }
        if !any {
            lines.push(first.to_string());
        }
    }

    fn render_list(&self, token: &Token, depth: usize, width: usize, ctx: Option<&Ctx>) -> Vec<String> {
        let Token::List {
            ordered,
            start,
            loose,
            items,
        } = token
        else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        let indent = "    ".repeat(depth);
        let start = start.unwrap_or(1);
        for (i, item) in items.iter().enumerate() {
            let marker = self.list_item_marker(*ordered, start, i, item);
            let first = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let cont = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            self.render_list_item(item, &first, &cont, depth, width, ctx, &mut lines);
            if *loose && i + 1 < items.len() {
                lines.push(String::new());
            }
        }
        lines
    }

    fn wrap_cell(text: &str, width: usize, prefix: &str) -> Vec<String> {
        let ls = wrap_text_with_ansi(text, width.max(1));
        let n = ls.len();
        ls.into_iter()
            .enumerate()
            .map(|(i, l)| {
                format!(
                    "{l}{}{prefix}",
                    if i + 1 < n { "\x1b[22;23;24;25;27;28;29;39m" } else { "" }
                )
            })
            .collect()
    }

    fn longest_word(t: &str) -> usize {
        t.split(char::is_whitespace)
            .map(visible_width)
            .max()
            .unwrap_or(0)
            .min(30)
    }

    fn measure_table_columns(header_text: &[String], row_text: &[Vec<String>]) -> (Vec<usize>, Vec<usize>) {
        let mut natural: Vec<usize> = header_text.iter().map(|t| visible_width(t)).collect();
        let mut min_word: Vec<usize> = header_text.iter().map(|t| Self::longest_word(t).max(1)).collect();
        for r in row_text {
            for (i, t) in r.iter().enumerate() {
                if i >= natural.len() {
                    natural.push(0);
                    min_word.push(1);
                }
                natural[i] = natural[i].max(visible_width(t));
                min_word[i] = min_word[i].max(Self::longest_word(t));
            }
        }
        (natural, min_word)
    }

    fn fit_min_columns(min_word: &[usize], for_cells: usize) -> (Vec<usize>, usize) {
        let mut min_cols = min_word.to_vec();
        let min_cells: usize = min_cols.iter().sum();
        if min_cells <= for_cells {
            return (min_cols, min_cells);
        }
        let cols = min_word.len();
        min_cols = vec![1; cols];
        let remaining = for_cells - cols;
        if remaining > 0 {
            let total: usize = min_word.iter().map(|w| w.saturating_sub(1)).sum();
            let growth: Vec<usize> = min_word
                .iter()
                .map(|w| {
                    if total > 0 {
                        ((w.saturating_sub(1) as f64 / total as f64) * remaining as f64).floor() as usize
                    } else {
                        0
                    }
                })
                .collect();
            for (m, g) in min_cols.iter_mut().zip(&growth) {
                *m += g;
            }
            let mut left = remaining - growth.iter().sum::<usize>();
            let mut i = 0;
            while left > 0 && i < cols {
                min_cols[i] += 1;
                left -= 1;
                i += 1;
            }
        }
        let sum = min_cols.iter().sum();
        (min_cols, sum)
    }

    fn compute_table_widths(
        natural: &[usize],
        min_cols: &[usize],
        min_cells: usize,
        avail: usize,
        overhead: usize,
        for_cells: usize,
    ) -> Vec<usize> {
        let cols = natural.len();
        let total_natural = natural.iter().sum::<usize>() + overhead;
        if total_natural <= avail {
            return (0..cols).map(|i| natural[i].max(min_cols[i])).collect();
        }
        let potential: usize = (0..cols).map(|i| natural[i].saturating_sub(min_cols[i])).sum();
        let extra = for_cells.saturating_sub(min_cells);
        let mut w: Vec<usize> = (0..cols)
            .map(|i| {
                let delta = natural[i].saturating_sub(min_cols[i]);
                min_cols[i]
                    + if potential > 0 {
                        ((delta as f64 / potential as f64) * extra as f64).floor() as usize
                    } else {
                        0
                    }
            })
            .collect();
        let mut remaining = for_cells as i64 - w.iter().sum::<usize>() as i64;
        while remaining > 0 && Self::grow_table_widths(&mut w, natural, &mut remaining) {}
        w
    }

    fn grow_table_widths(w: &mut [usize], natural: &[usize], remaining: &mut i64) -> bool {
        let mut grew = false;
        for i in 0..w.len() {
            if *remaining > 0 && w[i] < natural[i] {
                w[i] += 1;
                *remaining -= 1;
                grew = true;
            }
        }
        grew
    }

    fn table_border(widths: &[usize], l: &str, m: &str, r: &str) -> String {
        format!(
            "{l}─{}─{r}",
            widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join(&format!("─{m}─"))
        )
    }

    fn emit_table_row(&self, cells: &[String], widths: &[usize], prefix: &str, bold: bool, lines: &mut Vec<String>) {
        let wrapped: Vec<Vec<String>> = cells
            .iter()
            .enumerate()
            .map(|(i, t)| Self::wrap_cell(t, widths[i], prefix))
            .collect();
        let count = wrapped.iter().map(Vec::len).max().unwrap_or(0);
        for li in 0..count {
            let parts: Vec<String> = wrapped
                .iter()
                .enumerate()
                .map(|(c, cl)| {
                    let t = cl.get(li).map_or("", String::as_str);
                    let padded = format!("{t}{}", " ".repeat(sub(widths[c], visible_width(t))));
                    if bold { (self.theme.bold)(&padded) } else { padded }
                })
                .collect();
            lines.push(format!("│ {} │", parts.join(" │ ")));
        }
    }

    fn table_fallback(raw: &str, avail: usize, spaced: bool) -> Vec<String> {
        let mut fb = if raw.is_empty() {
            Vec::new()
        } else {
            wrap_text_with_ansi(raw, avail)
        };
        if spaced {
            fb.push(String::new());
        }
        fb
    }

    fn render_table(&self, token: &Token, avail: usize, next: Option<&Token>, ctx: Option<&Ctx>) -> Vec<String> {
        let Token::Table { raw, header, rows } = token else {
            return Vec::new();
        };
        let cols = header.len();
        if cols == 0 {
            return Vec::new();
        }
        let spaced = next.is_some_and(|n| !matches!(n, Token::Space));
        let overhead = 3 * cols + 1;
        let for_cells = avail as i64 - overhead as i64;
        if for_cells < cols as i64 {
            return Self::table_fallback(raw, avail, spaced);
        }
        let for_cells = for_cells as usize;
        let header_text: Vec<String> = header.iter().map(|c| self.render_inline(c, ctx)).collect();
        let row_text: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| self.render_inline(c, ctx)).collect())
            .collect();
        let (natural, min_word) = Self::measure_table_columns(&header_text, &row_text);
        let (min_cols, min_cells) = Self::fit_min_columns(&min_word, for_cells);
        let widths = Self::compute_table_widths(&natural, &min_cols, min_cells, avail, overhead, for_cells);
        let prefix = ctx.map_or("", |c| c.prefix.as_str());
        let mut lines = vec![Self::table_border(&widths, "┌", "┬", "┐")];
        self.emit_table_row(&header_text, &widths, prefix, true, &mut lines);
        let sep = Self::table_border(&widths, "├", "┼", "┤");
        lines.push(sep.clone());
        for (ri, r) in row_text.iter().enumerate() {
            self.emit_table_row(r, &widths, prefix, false, &mut lines);
            if ri + 1 < row_text.len() {
                lines.push(sep.clone());
            }
        }
        lines.push(Self::table_border(&widths, "└", "┴", "┘"));
        if spaced {
            lines.push(String::new());
        }
        lines
    }
}

fn trim_trailing_code_fence(raw: &str, text: &mut String) {
    let fence_char = match raw.chars().next() {
        Some(c @ ('`' | '~')) => c,
        _ => return,
    };
    let marker_len = raw.chars().take_while(|c| *c == fence_char).count();
    if marker_len < 3 {
        return;
    }
    let last = raw.rsplit('\n').next().unwrap_or("");
    if last.is_empty() || last.len() >= marker_len || last.chars().any(|c| c != fence_char) {
        return;
    }
    let cut = text.len().saturating_sub(last.len());
    let mut t = text[..cut].to_string();
    if t.ends_with('\n') {
        t.pop();
    }
    *text = t;
}

/// Trims a streamed partial closing fence (`` ` `` or `~` shorter than the opening fence) from a trailing code block,
/// descending into the last list item and blockquote.
pub fn trim_partial_closing_fences(tokens: &mut [Token]) {
    match tokens.last_mut() {
        Some(Token::List { items, .. }) => {
            if let Some(item) = items.last_mut() {
                trim_partial_closing_fences(&mut item.tokens);
            }
        }
        Some(Token::Blockquote { tokens }) => trim_partial_closing_fences(tokens),
        Some(Token::Code { raw, text, .. }) => trim_trailing_code_fence(raw, text),
        _ => {}
    }
}

impl Component for Markdown {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.tokens.is_empty() {
            return Vec::new();
        }
        self.render_lines(width)
    }
}
