use crate::markdown::{ListItem, Token};

use super::inline::checkbox;
use super::{
    Caps, Lexer, NextAt, contains, exec, from, g, group, item_rule, replace, rtrim, rules, search, second_char, test,
};

#[derive(Clone, Debug, Default)]
pub(super) struct Block {
    pub kind: &'static str,
    pub raw: String,
    pub text: String,
    pub depth: usize,
    pub lang: Option<String>,
    pub tokens: Vec<Block>,
    pub ordered: bool,
    pub start: Option<i64>,
    pub loose: bool,
    pub items: Vec<Item>,
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Loose task item: a checkbox token precedes the paragraph's inline tokens.
    pub checkbox: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Item {
    pub raw: String,
    pub task: bool,
    pub checked: bool,
    pub text: String,
    pub tokens: Vec<Block>,
}

fn block(kind: &'static str, raw: &str, text: &str) -> Block {
    Block {
        kind,
        raw: raw.to_string(),
        text: text.to_string(),
        ..Block::default()
    }
}

fn trim_trailing_blank_lines(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut t = lines.len() as isize - 1;
    while t >= 0 && lines[t as usize].chars().all(|c| c == ' ' || c == '\t') {
        t -= 1;
    }
    if lines.len() as isize - t <= 2 {
        s.to_string()
    } else {
        lines[..(t + 1) as usize].join("\n")
    }
}

/// marked `expandTabs` (tab stops of 4 from column `start`).
fn expand_tabs(s: &str, start: usize) -> String {
    let mut col = start;
    let mut out = String::new();
    for c in s.chars() {
        if c == '\t' {
            let n = 4 - col % 4;
            out.push_str(&" ".repeat(n));
            col += n;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

fn first_non_space(s: &str) -> Option<usize> {
    s.find(|c| c != ' ')
}

fn split_cells(row: &str, count: Option<usize>) -> Vec<String> {
    let mut t = String::new();
    let b = row.as_bytes();
    for (i, c) in row.char_indices() {
        if c == '|' {
            let escaped = b[..i].iter().rev().take_while(|&&x| x == b'\\').count() % 2 == 1;
            t.push_str(if escaped { "|" } else { " |" });
        } else {
            t.push(c);
        }
    }
    let mut cells: Vec<String> = t.split(" |").map(str::to_string).collect();
    if cells[0].trim().is_empty() {
        cells.remove(0);
    }
    if cells.last().is_some_and(|c| c.trim().is_empty()) {
        cells.pop();
    }
    if let Some(n) = count {
        cells.resize(n, String::new());
    }
    cells.iter().map(|c| c.trim().replace("\\|", "|")).collect()
}

fn indent_code_compensation(raw: &str, text: &str) -> String {
    let Some(c) = exec(&rules::INDENT_COMP, raw) else {
        return text.to_string();
    };
    let indent = c[1].len();
    text.split('\n')
        .map(|line| {
            let lead = line.len() - line.trim_start().len();
            if lead == 0 {
                line
            } else if lead >= indent {
                &line[indent..]
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn table(src: &str) -> Option<Block> {
    let c = exec(&rules::TABLE, src)?;
    if !c[2].contains([':', '|']) {
        return None;
    }
    let header = split_cells(&c[1], None);
    let align_row = c[2].strip_prefix('|').unwrap_or(&c[2]);
    let align_row = align_row.trim_end_matches(' ');
    let align_row = align_row.strip_suffix('|').map_or(align_row, |r| r);
    let aligns = align_row.split('|').count();
    if header.len() != aligns {
        return None;
    }
    let body = group(&c, 3).unwrap_or("");
    let rows: Vec<&str> = if body.trim().is_empty() {
        Vec::new()
    } else {
        let t = body.trim_end_matches([' ', '\t']);
        t.strip_suffix('\n').unwrap_or(body).split('\n').collect()
    };
    Some(Block {
        kind: "table",
        raw: rtrim(&c[0], '\n').to_string(),
        rows: rows.iter().map(|r| split_cells(r, Some(header.len()))).collect(),
        header,
        ..Block::default()
    })
}

fn tokenize_block_latex(src: &str) -> Option<Block> {
    for r in [&*rules::LATEX_DOLLAR, &*rules::LATEX_BRACKET] {
        if let Some(c) = exec(r, src)
            && !g(&c, 1).is_empty()
        {
            return Some(block("latexBlock", &g(&c, 0), g(&c, 1).trim()));
        }
    }
    if let Some(c) = exec(&rules::LATEX_PENDING_BRACKET, src) {
        return Some(block("latexBlock", &g(&c, 0), &g(&c, 1)));
    }
    if let Some(c) = exec(&rules::LATEX_PENDING_DOLLAR, src)
        && !g(&c, 1).is_empty()
        && contains(&rules::PENDING_MATH, &g(&c, 1))
    {
        return Some(block("latexBlock", &g(&c, 0), &g(&c, 1)));
    }
    None
}

/// Length of the paragraph cut before a block LaTeX start (`(?:^|\n) {0,3}(?:\$\$|\\\[)` searched in
/// `src.slice(1)`), where `src` is the unlexed suffix of `whole`.
fn latex_block_start(whole: &str, src: &str, next: &mut NextAt) -> Option<usize> {
    let skip = second_char(src);
    let rest = &src[skip..];
    if test(&rules::LATEX_BLOCK_AT_START, rest) {
        return Some(skip);
    }
    let start = whole.len() - rest.len();
    let at = next.get(whole, start, |s| {
        rules::LATEX_BLOCK_AFTER_NEWLINE
            .find(s)
            .ok()
            .flatten()
            .map(|m| m.start())
    })?;
    Some(at - (whole.len() - src.len()) + 1)
}

fn append_block_line(tokens: &mut [Block], kinds: &[&str], raw: &str, text: &str) -> bool {
    let Some(last) = tokens.last_mut().filter(|last| kinds.contains(&last.kind)) else {
        return false;
    };
    if !last.raw.ends_with('\n') {
        last.raw.push('\n');
    }
    last.raw += raw;
    last.text.push('\n');
    last.text.push_str(text);
    true
}

fn strip_trailing_hashes(text: &str) -> String {
    let trimmed_all = text.trim();
    if trimmed_all.ends_with('#') {
        let trimmed = rtrim(trimmed_all, '#');
        if trimmed.is_empty() || trimmed.ends_with(' ') {
            return trimmed.trim().to_string();
        }
    }
    trimmed_all.to_string()
}

fn parse_def_href_title(c: &Caps<'_>) -> (String, String) {
    let href = group(c, 2).map_or(String::new(), |h| {
        let h = h.strip_prefix('<').and_then(|h| h.strip_suffix('>')).unwrap_or(h);
        replace(&rules::ANY_PUNCT, h, "$1")
    });
    let title = group(c, 3).map_or(String::new(), |t| replace(&rules::ANY_PUNCT, &t[1..t.len() - 1], "$1"));
    (href, title)
}

fn drain_blockquote_chunk(lines: &mut Vec<String>) -> String {
    let mut in_quote = false;
    let mut current = Vec::new();
    let mut a = 0;
    while a < lines.len() {
        if test(&rules::BQ_START, &lines[a]) {
            current.push(lines[a].clone());
            in_quote = true;
        } else if !in_quote {
            current.push(lines[a].clone());
        } else {
            break;
        }
        a += 1;
    }
    lines.drain(..a);
    current.join("\n")
}

struct ListItemHead {
    raw: String,
    indent: usize,
    line: String,
    blank: bool,
    item_contents: String,
    end_early: bool,
}

fn read_list_item_head(item_re: &fancy_regex::Regex, src: &mut &str) -> Option<ListItemHead> {
    let c = exec(item_re, src)?;
    if test(&rules::HR, src) {
        return None;
    }
    let mut raw = g(&c, 0);
    let marker_len = c[1].len();
    let line = expand_tabs(c[2].split('\n').next().unwrap_or(""), marker_len);
    *src = &src[raw.len()..];
    let next_line = src.split('\n').next().unwrap_or("");
    let blank = line.trim().is_empty();
    let (item_contents, indent) = if blank {
        (String::new(), marker_len + 1)
    } else {
        let i = first_non_space(&line).map_or(-1, |i| i as isize);
        let i = if i > 4 { 1 } else { i };
        let contents = if i < 0 {
            line.clone()
        } else {
            from(&line, i as usize).to_string()
        };
        (contents, (i + marker_len as isize).max(0) as usize)
    };
    let mut end_early = false;
    if blank && next_line.chars().all(|ch| ch == ' ' || ch == '\t') {
        raw += next_line;
        raw.push('\n');
        *src = from(src, next_line.len() + 1);
        end_early = true;
    }
    Some(ListItemHead {
        raw,
        indent,
        line,
        blank,
        item_contents,
        end_early,
    })
}

fn list_item_line_ends(r: &rules::ItemEnds, next_line: &str) -> bool {
    [&r.fences, &r.heading, &r.html, &r.quote, &r.next_bullet, &r.hr]
        .iter()
        .any(|rule| test(rule, next_line))
}

fn should_break_list_item(r: &rules::ItemEnds, line: &str, blank: bool) -> bool {
    let line_x = line.replace('\t', "    ");
    blank
        || first_non_space(&line_x).is_some_and(|i| i >= 4)
        || test(&r.fences, line)
        || test(&r.heading, line)
        || test(&r.hr, line)
}

fn consume_list_item_body(src: &mut &str, head: &mut ListItemHead) {
    if head.end_early {
        return;
    }
    let r = &rules::LIST_ITEM_ENDS[head.indent.saturating_sub(1).min(3)];
    while !src.is_empty() {
        let raw_line = src.split('\n').next().unwrap_or("");
        let expanded = raw_line.replace('\t', "    ");
        if list_item_line_ends(r, raw_line) {
            break;
        }
        if first_non_space(&expanded).is_some_and(|i| i >= head.indent) || raw_line.trim().is_empty() {
            head.item_contents.push('\n');
            head.item_contents.push_str(from(&expanded, head.indent));
        } else if should_break_list_item(r, &head.line, head.blank) {
            break;
        } else {
            head.item_contents.push('\n');
            head.item_contents.push_str(raw_line);
        }
        head.blank = raw_line.trim().is_empty();
        head.raw += raw_line;
        head.raw.push('\n');
        *src = from(src, raw_line.len() + 1);
        head.line = from(&expanded, head.indent).to_string();
    }
}

fn apply_item_task_checkbox(item: &mut Item, loose: bool) {
    let first_kind = item.tokens.first().map(|t| t.kind);
    if !item.task || !matches!(first_kind, Some("text" | "paragraph")) {
        item.task = false;
        return;
    }
    item.text = replace(&rules::TASK, &item.text, "");
    let first = &mut item.tokens[0];
    first.raw = replace(&rules::TASK, &first.raw, "");
    first.text = replace(&rules::TASK, &first.text, "");
    if let Some(c) = search(&rules::CHECKBOX, &item.raw) {
        let checked = &c[0] != "[ ]";
        item.checked = checked;
        if loose {
            item.tokens[0].checkbox = Some(checked);
        } else {
            item.tokens.insert(0, block("checkbox", "", ""));
        }
    }
}

fn apply_list_task_checkboxes(list: &mut Block) {
    let loose = list.loose;
    for item in &mut list.items {
        apply_item_task_checkbox(item, loose);
    }
}

fn upgrade_loose_text_blocks(list: &mut Block) {
    if !list.loose {
        return;
    }
    for item in &mut list.items {
        for t in &mut item.tokens {
            if t.kind == "text" {
                t.kind = "paragraph";
            }
        }
    }
}

fn safe_prefix(s: &str, trim_bytes: usize) -> &str {
    let mut end = s.len().saturating_sub(trim_bytes);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn safe_suffix(s: &str, start_bytes: usize) -> &str {
    let mut start = start_bytes.min(s.len());
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

impl Lexer {
    fn try_block_newline(&self, src: &str, tokens: &mut Vec<Block>) -> Option<usize> {
        let c = exec(&rules::NEWLINE, src)?;
        if c[0].is_empty() {
            return None;
        }
        let raw = g(&c, 0);
        let len = raw.len();
        match tokens.last_mut() {
            Some(last) if len == 1 => last.raw.push('\n'),
            _ => tokens.push(block("space", &raw, "")),
        }
        Some(len)
    }

    fn try_block_code(&self, src: &str, tokens: &mut Vec<Block>) -> Option<usize> {
        let c = exec(&rules::CODE, src)?;
        let raw = trim_trailing_blank_lines(&c[0]);
        let text = replace(&rules::CODE_INDENT, &raw, "");
        let len = raw.len();
        if !append_block_line(tokens, &["paragraph", "text"], &raw, &text) {
            tokens.push(Block {
                kind: "code",
                raw,
                text,
                ..Block::default()
            });
        }
        Some(len)
    }

    fn try_block_fences_or_heading(&self, src: &str, tokens: &mut Vec<Block>) -> Option<usize> {
        if let Some(c) = exec(&rules::FENCES, src) {
            let raw = g(&c, 0);
            let text = indent_code_compensation(&raw, group(&c, 3).unwrap_or(""));
            let lang = group(&c, 2).map(|l| replace(&rules::ANY_PUNCT, l.trim(), "$1"));
            let len = raw.len();
            tokens.push(Block {
                kind: "code",
                raw,
                text,
                lang,
                ..Block::default()
            });
            return Some(len);
        }
        if let Some(c) = exec(&rules::HEADING, src) {
            let text = strip_trailing_hashes(&g(&c, 2));
            tokens.push(Block {
                depth: c[1].len(),
                ..block("heading", rtrim(&c[0], '\n'), &text)
            });
            return Some(c[0].len());
        }
        if let Some(c) = exec(&rules::HR, src) {
            tokens.push(block("hr", rtrim(&c[0], '\n'), ""));
            return Some(c[0].len());
        }
        None
    }

    fn try_block_def(&mut self, src: &str, tokens: &mut Vec<Block>) -> Option<usize> {
        let c = exec(&rules::DEF, src)?;
        let raw = rtrim(&c[0], '\n').to_string();
        let consumed = c[0].len();
        if !append_block_line(tokens, &["paragraph", "text"], &raw, &raw) {
            let tag = c[1].to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
            let (href, title) = parse_def_href_title(&c);
            if let std::collections::hash_map::Entry::Vacant(e) = self.links.entry(tag) {
                e.insert((href, title));
                tokens.push(block("def", &raw, ""));
            }
        }
        Some(consumed)
    }

    fn try_block_paragraph(
        &self,
        whole: &str,
        src: &str,
        latex_start: &mut NextAt,
        tokens: &mut Vec<Block>,
        last_clipped: &mut bool,
    ) -> Option<usize> {
        let mut cut = src;
        if let Some(at) = latex_block_start(whole, src, latex_start) {
            cut = &src[..at];
        }
        if !self.top {
            return None;
        }
        let c = exec(&rules::PARAGRAPH, cut)?;
        let raw = g(&c, 0);
        let text = c[1].strip_suffix('\n').unwrap_or(&c[1]).to_string();
        let clipped = cut.len() != src.len();
        let appended = *last_clipped && append_block_line(tokens, &["paragraph"], &raw, &text);
        if !appended {
            tokens.push(block("paragraph", &raw, &text));
        }
        *last_clipped = clipped;
        Some(raw.len())
    }

    fn try_block_text(&self, src: &str, tokens: &mut Vec<Block>) -> Option<usize> {
        let c = exec(&rules::TEXT, src)?;
        let raw = g(&c, 0);
        let len = raw.len();
        if !append_block_line(tokens, &["text"], &raw, &raw) {
            tokens.push(block("text", &raw, &raw));
        }
        Some(len)
    }

    fn try_block_step_early(&mut self, src: &str, tokens: &mut Vec<Block>) -> Option<usize> {
        if let Some(t) = tokenize_block_latex(src) {
            let len = t.raw.len();
            tokens.push(t);
            return Some(len);
        }
        if let Some(len) = self
            .try_block_newline(src, tokens)
            .or_else(|| self.try_block_code(src, tokens))
            .or_else(|| self.try_block_fences_or_heading(src, tokens))
        {
            return Some(len);
        }
        if let Some(t) = self.blockquote(src).or_else(|| self.list(src)) {
            let len = t.raw.len();
            tokens.push(t);
            return Some(len);
        }
        None
    }

    fn try_block_step_late(
        &mut self,
        whole: &str,
        src: &str,
        latex_start: &mut NextAt,
        tokens: &mut Vec<Block>,
        last_clipped: &mut bool,
    ) -> Option<usize> {
        if let Some(c) = exec(&rules::HTML, src) {
            let raw = trim_trailing_blank_lines(&c[0]);
            let len = raw.len();
            tokens.push(block("html", &raw, &raw));
            return Some(len);
        }
        if let Some(len) = self.try_block_def(src, tokens) {
            return Some(len);
        }
        if let Some(t) = table(src) {
            let len = t.raw.len();
            tokens.push(t);
            return Some(len);
        }
        if let Some(c) = exec(&rules::LHEADING, src) {
            tokens.push(Block {
                depth: if c[2].starts_with('=') { 1 } else { 2 },
                ..block("heading", rtrim(&c[0], '\n'), c[1].trim())
            });
            return Some(c[0].len());
        }
        self.try_block_paragraph(whole, src, latex_start, tokens, last_clipped)
            .or_else(|| self.try_block_text(src, tokens))
    }

    pub(super) fn block_tokens(&mut self, src: &str, tokens: &mut Vec<Block>, mut last_paragraph_clipped: bool) {
        let whole = src;
        let mut src = src;
        let mut latex_start = NextAt::default();
        let mut shortest = usize::MAX;
        while !src.is_empty() {
            if src.len() < shortest {
                shortest = src.len();
            } else {
                break;
            }
            if let Some(len) = self
                .try_block_step_early(src, tokens)
                .or_else(|| self.try_block_step_late(whole, src, &mut latex_start, tokens, &mut last_paragraph_clipped))
            {
                if len >= src.len() {
                    break;
                }
                src = safe_suffix(src, len);
                continue;
            }
            break;
        }
        self.top = true;
    }

    fn merge_blockquote_tail(
        &mut self,
        lines: &mut Vec<String>,
        tokens: &mut [Block],
        raw: &mut String,
        text: &mut String,
    ) -> Option<bool> {
        match tokens.last().map(|t| t.kind) {
            Some("code") => Some(false),
            Some("blockquote") => {
                let last = tokens.last()?;
                let (last_raw, last_text_len) = (last.raw.clone(), last.text.len());
                let rest = lines.join("\n");
                let w = format!("{last_raw}\n{}", replace(&rules::BQ_PREFIX, &rest, ""));
                let quote = self.blockquote(&w)?;
                *text = format!("{}{}", safe_prefix(text, last_text_len), quote.text);
                if let Some(slot) = tokens.last_mut() {
                    *slot = quote;
                }
                *raw = format!("{raw}\n{rest}");
                Some(false)
            }
            Some("list") => {
                let last = tokens.last()?;
                let last_raw = last.raw.clone();
                let joined = format!("{last_raw}\n{}", lines.join("\n"));
                let list = self.list(&joined)?;
                *raw = format!("{}{}", safe_prefix(raw, last_raw.len()), list.raw);
                *text = format!("{}{}", safe_prefix(text, last_raw.len()), list.raw);
                *lines = safe_suffix(&joined, list.raw.len())
                    .split('\n')
                    .map(str::to_string)
                    .collect();
                if let Some(slot) = tokens.last_mut() {
                    *slot = list;
                }
                Some(true)
            }
            _ => Some(true),
        }
    }

    fn blockquote(&mut self, src: &str) -> Option<Block> {
        let c = exec(&rules::BLOCKQUOTE, src)?;
        let mut lines: Vec<String> = rtrim(&c[0], '\n').split('\n').map(str::to_string).collect();
        let (mut raw, mut text) = (String::new(), String::new());
        let mut tokens: Vec<Block> = Vec::new();
        while !lines.is_empty() {
            let p = drain_blockquote_chunk(&mut lines);
            let inner = replace(&rules::BQ_PREFIX, &replace(&rules::BQ_SETEXT, &p, "\n    $1"), "");
            raw = if raw.is_empty() { p } else { format!("{raw}\n{p}") };
            text = if text.is_empty() {
                inner.clone()
            } else {
                format!("{text}\n{inner}")
            };
            let top = self.top;
            self.top = true;
            self.block_tokens(&inner, &mut tokens, true);
            self.top = top;
            if lines.is_empty() || !self.merge_blockquote_tail(&mut lines, &mut tokens, &mut raw, &mut text)? {
                break;
            }
        }
        Some(Block {
            tokens,
            ..block("blockquote", &raw, &text)
        })
    }

    fn populate_list_items(&mut self, list: &mut Block) {
        for i in 0..list.items.len() {
            self.top = false;
            let mut tokens = Vec::new();
            let text = list.items[i].text.clone();
            self.block_tokens(&text, &mut tokens, false);
            if !list.loose {
                list.loose = tokens
                    .iter()
                    .any(|t| t.kind == "space" && contains(&rules::ANY_LINE, &t.raw));
            }
            list.items[i].tokens = tokens;
        }
    }

    fn list(&mut self, src: &str) -> Option<Block> {
        let c = exec(&rules::LIST, src)?;
        let bull = c[1].trim().to_string();
        let ordered = bull.len() > 1;
        let mut list = Block {
            kind: "list",
            ordered,
            start: ordered.then(|| bull[..bull.len() - 1].parse().ok()).flatten(),
            ..Block::default()
        };
        let item_re = item_rule(&bull);
        let mut src = src;
        let mut ends_with_blank = false;
        while !src.is_empty() {
            let Some(mut head) = read_list_item_head(item_re, &mut src) else {
                break;
            };
            consume_list_item_body(&mut src, &mut head);
            if !list.loose {
                if ends_with_blank {
                    list.loose = true;
                } else if contains(&rules::DOUBLE_BLANK, &head.raw) {
                    ends_with_blank = true;
                }
            }
            list.raw += &head.raw;
            list.items.push(Item {
                task: test(&rules::BULLET_DEPTH, &head.item_contents),
                raw: head.raw,
                text: head.item_contents,
                ..Item::default()
            });
        }
        let last = list.items.last_mut()?;
        last.raw = last.raw.trim_end().to_string();
        last.text = last.text.trim_end().to_string();
        list.raw = list.raw.trim_end().to_string();
        self.populate_list_items(&mut list);
        apply_list_task_checkboxes(&mut list);
        upgrade_loose_text_blocks(&mut list);
        Some(list)
    }

    pub(super) fn convert_all(&mut self, blocks: Vec<Block>) -> Vec<Token> {
        blocks.into_iter().map(|b| self.convert(b)).collect()
    }

    fn convert_compound(&mut self, b: Block) -> Token {
        match b.kind {
            "blockquote" => Token::Blockquote {
                tokens: self.convert_all(b.tokens),
            },
            "list" => Token::List {
                ordered: b.ordered,
                start: b.start,
                loose: b.loose,
                items: b
                    .items
                    .into_iter()
                    .map(|i| ListItem {
                        raw: i.raw,
                        task: i.task,
                        checked: i.checked,
                        tokens: self.convert_all(i.tokens),
                    })
                    .collect(),
            },
            "table" => Token::Table {
                raw: b.raw,
                header: b.header.iter().map(|c| self.inline_tokens(c)).collect(),
                rows: b
                    .rows
                    .iter()
                    .map(|r| r.iter().map(|c| self.inline_tokens(c)).collect())
                    .collect(),
            },
            "paragraph" => {
                let mut tokens = Vec::new();
                if b.checkbox.is_some() {
                    tokens.push(checkbox());
                }
                tokens.extend(self.inline_tokens(&b.text));
                Token::Paragraph { tokens }
            }
            "text" => Token::Text {
                tokens: Some(self.inline_tokens(&b.text)),
                text: b.text,
            },
            kind => Token::Other {
                kind: kind.to_string(),
                text: None,
            },
        }
    }

    fn convert(&mut self, b: Block) -> Token {
        match b.kind {
            "space" => Token::Space,
            "code" => Token::Code {
                raw: b.raw,
                lang: b.lang,
                text: b.text,
            },
            "heading" => Token::Heading {
                depth: b.depth,
                tokens: self.inline_tokens(&b.text),
            },
            "hr" => Token::Hr,
            "html" => Token::Html { raw: b.raw },
            "latexBlock" => Token::LatexBlock { raw: b.raw },
            "checkbox" => checkbox(),
            _ => self.convert_compound(b),
        }
    }
}
