use fancy_regex::RegexInput;

use crate::markdown::Token;

use super::delim::DelimiterRuns;
use super::{Caps, Lexer, NextAt, contains, exec, g, group, replace, rtrim, rules, search, second_char, test, work};

/// marked `findClosingBracket` for `()`: index of the unmatched `)`, -1 when absent, -2 when unbalanced.
fn find_closing_paren(s: &str) -> isize {
    if !s.contains(')') {
        return -1;
    }
    let b = s.as_bytes();
    let mut level = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'(' => level += 1,
            b')' => {
                level -= 1;
                if level < 0 {
                    return i as isize;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if level > 0 { -2 } else { -1 }
}

fn is_escaped(s: &str, index: usize) -> bool {
    s.as_bytes()[..index].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

fn find_closing_delimiter(s: &str, closing: &str, start: usize) -> Option<usize> {
    let mut index = s.get(start..)?.find(closing).map(|i| i + start);
    while let Some(i) = index
        && is_escaped(s, i)
    {
        index = s
            .get(i + closing.len()..)
            .and_then(|r| r.find(closing))
            .map(|j| j + i + closing.len());
    }
    work(index.unwrap_or(s.len()).saturating_sub(start));
    index
}

fn inline_latex_delimiters(src: &str) -> Option<(&'static str, &'static str)> {
    if src.starts_with("$$") {
        Some(("$$", "$$"))
    } else if src.starts_with("\\(") {
        Some(("\\(", "\\)"))
    } else if src.starts_with("\\[") {
        Some(("\\[", "\\]"))
    } else if src.starts_with('$') && !src[1..].starts_with(char::is_whitespace) {
        Some(("$", "$"))
    } else {
        None
    }
}

fn is_rejected_dollar_latex(src: &str, ci: usize) -> bool {
    let inner = &src[1..ci];
    let after = &src[ci + 1..];
    inner.ends_with(char::is_whitespace)
        || after.starts_with(|c: char| c.is_ascii_digit())
        || (test(&rules::ENV_LIKE, inner) && test(&rules::IDENT_START, after))
        || inner.contains('`')
}

/// `tokenizeInlineLatex` from markdown.ts; returns the token's raw source.
fn tokenize_inline_latex(src: &str) -> Option<String> {
    let (opening, closing) = inline_latex_delimiters(src)?;
    let close = find_closing_delimiter(src, closing, opening.len());
    if let Some(ci) = close
        && opening == "$"
        && is_rejected_dollar_latex(src, ci)
    {
        return None;
    }
    let Some(ci) = close else {
        let pending = &src[opening.len()..];
        return (opening.starts_with('\\') || contains(&rules::PENDING_MATH, pending)).then(|| src.to_string());
    };
    let text = &src[opening.len()..ci];
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    Some(src[..ci + closing.len()].to_string())
}

/// Length of the inline text cut before the next LaTeX opener (`$`, `\(` or `\[`) after the first character of
/// `src`, the unlexed suffix of `whole`.
fn inline_latex_start(whole: &str, src: &str, next: &mut [NextAt; 3]) -> Option<usize> {
    let start = whole.len() - src.len() + second_char(src);
    let [dollar, paren, bracket] = next;
    let found = [
        dollar.get(whole, start, |s| s.find('$')),
        paren.get(whole, start, |s| s.find("\\(")),
        bracket.get(whole, start, |s| s.find("\\[")),
    ];
    found
        .into_iter()
        .flatten()
        .min()
        .map(|at| at - (whole.len() - src.len()))
}

fn has_block_skip_link(s: &str) -> bool {
    for m in rules::BLOCK_SKIP.find_iter(s).flatten() {
        work(0);
        if test(&rules::LINK, m.as_str()) && (m.start() == 0 || s.as_bytes()[m.start() - 1] != b'!') {
            return true;
        }
    }
    false
}

fn is_raw_block_tag(s: &str) -> bool {
    ["pre", "code", "kbd", "script"].iter().any(|t| {
        s.strip_prefix(t)
            .is_some_and(|r| r.starts_with(|c: char| c == '>' || c.is_whitespace()))
    })
}

fn backpedal_url(mut cur: String) -> String {
    loop {
        let prev = cur.clone();
        cur = search(&rules::BACKPEDAL, &cur).map_or(String::new(), |m| g(&m, 0));
        if prev == cur {
            return cur;
        }
    }
}

impl Lexer {
    fn mask(&self, src: &str) -> String {
        let mut masked = src.to_string();
        if !self.links.is_empty() && masked.contains('[') {
            masked = self.mask_reflinks(&masked);
        }
        work(masked.len());
        masked = rules::ANY_PUNCT
            .replace_all(&masked, |c: &Caps| "+".repeat(c[0].len()))
            .into_owned();
        mask_block_skip(&masked)
    }

    fn mask_reflinks(&self, s: &str) -> String {
        work(s.len());
        rules::REFLINK_SEARCH
            .replace_all(s, |c: &Caps| {
                let a = &c[0];
                let p = a.rfind('[').unwrap_or(0);
                if !self.links.contains_key(&a[p + 1..a.len() - 1]) {
                    return a.to_string();
                }
                if p > 1 && !a.starts_with('!') {
                    let inner = &a[1..p - 1];
                    if self.link_in_text(inner) {
                        return format!("[{}][{}]", self.mask_reflinks(inner), "a".repeat(a.len() - p - 2));
                    }
                }
                format!("[{}]", "a".repeat(a.len() - 2))
            })
            .into_owned()
    }

    fn has_ref_link(&self, s: &str) -> bool {
        for m in rules::REFLINK_SEARCH.find_iter(s).flatten() {
            let t = m.as_str();
            let r = t.rfind('[').unwrap_or(0);
            if !t.starts_with('!')
                && self.links.contains_key(&t[r + 1..t.len() - 1])
                && !(r > 1 && self.link_in_text(&t[1..r - 1]))
            {
                return true;
            }
        }
        false
    }

    fn link_in_text(&self, s: &str) -> bool {
        if !s.contains('[') {
            return false;
        }
        work(2 * s.len());
        has_block_skip_link(s) || self.has_ref_link(s)
    }

    fn update_link_tag_state(&mut self, lower: &str) {
        if !self.in_link && lower.starts_with("<a ") {
            self.in_link = true;
        } else if self.in_link && lower.starts_with("</a>") {
            self.in_link = false;
        }
    }

    fn update_raw_block_state(&mut self, lower: &str) {
        if !self.in_raw_block && lower.strip_prefix('<').is_some_and(is_raw_block_tag) {
            self.in_raw_block = true;
        } else if self.in_raw_block && lower.strip_prefix("</").is_some_and(is_raw_block_tag) {
            self.in_raw_block = false;
        }
    }

    fn try_inline_tag(&mut self, src: &str, tokens: &mut Vec<Token>) -> Option<usize> {
        let c = exec(&rules::TAG, src)?;
        let raw = g(&c, 0);
        let lower = raw.to_ascii_lowercase();
        self.update_link_tag_state(&lower);
        self.update_raw_block_state(&lower);
        let len = raw.len();
        tokens.push(Token::Html { raw });
        Some(len)
    }

    fn try_inline_url(&self, src: &str) -> Option<(usize, Token)> {
        if self.in_link {
            return None;
        }
        let c = exec(&rules::URL, src)?;
        let (text, href) = if group(&c, 2) == Some("@") {
            let text = g(&c, 0);
            let href = format!("mailto:{text}");
            (text, href)
        } else {
            let cur = backpedal_url(g(&c, 0));
            let href = if group(&c, 1) == Some("www.") {
                format!("http://{cur}")
            } else {
                cur.clone()
            };
            (cur, href)
        };
        Some((text.len(), link_token(href, text)))
    }

    fn try_inline_codespan(&self, src: &str, tokens: &mut Vec<Token>) -> Option<usize> {
        let c = exec(&rules::CODESPAN, src)?;
        let mut text = c[2].replace('\n', " ");
        if text.contains(|ch| ch != ' ') && text.starts_with(' ') && text.ends_with(' ') {
            text = text[1..text.len() - 1].to_string();
        }
        tokens.push(Token::Codespan { text });
        Some(c[0].len())
    }

    fn try_inline_markup(
        &mut self,
        src: &str,
        masked: &str,
        prev_char: &str,
        runs: &mut DelimiterRuns,
        tokens: &mut Vec<Token>,
    ) -> Option<usize> {
        if let Some(raw) = tokenize_inline_latex(src) {
            let len = raw.len();
            tokens.push(Token::Latex { raw });
            return Some(len);
        }
        if let Some(c) = exec(&rules::ESCAPE, src) {
            tokens.push(Token::Escape {
                raw: g(&c, 0),
                text: g(&c, 1),
            });
            return Some(c[0].len());
        }
        if let Some(len) = self.try_inline_tag(src, tokens) {
            return Some(len);
        }
        if let Some((len, t)) = self.link(src) {
            tokens.push(t);
            return Some(len);
        }
        if let Some((len, t)) = self.reflink(src) {
            push_text(tokens, t);
            return Some(len);
        }
        if let Some((len, t)) = self.em_strong(src, masked, prev_char, runs) {
            tokens.push(t);
            return Some(len);
        }
        None
    }

    fn try_inline_span(&mut self, src: &str, tokens: &mut Vec<Token>) -> Option<usize> {
        if let Some(len) = self.try_inline_codespan(src, tokens) {
            return Some(len);
        }
        if let Some(c) = exec(&rules::BR, src) {
            tokens.push(Token::Br);
            return Some(c[0].len());
        }
        if let Some(c) = exec(&rules::STRICT_DEL, src) {
            let inner = self.inline_tokens(&c[2]);
            tokens.push(Token::Del { tokens: inner });
            return Some(c[0].len());
        }
        if let Some(c) = exec(&rules::AUTOLINK, src) {
            let text = g(&c, 1);
            let href = if group(&c, 2) == Some("@") {
                format!("mailto:{text}")
            } else {
                text.clone()
            };
            tokens.push(link_token(href, text));
            return Some(c[0].len());
        }
        if let Some((len, t)) = self.try_inline_url(src) {
            tokens.push(t);
            return Some(len);
        }
        None
    }

    pub(super) fn inline_tokens(&mut self, src: &str) -> Vec<Token> {
        let masked = self.mask(src);
        let mut tokens: Vec<Token> = Vec::new();
        let whole = src;
        let mut src = src;
        let mut latex_start = [NextAt::default(), NextAt::default(), NextAt::default()];
        let mut runs = DelimiterRuns::default();
        let mut keep_prev = false;
        let mut prev_char = String::new();
        let mut shortest = usize::MAX;
        while !src.is_empty() {
            if src.len() < shortest {
                shortest = src.len();
            } else {
                break;
            }
            if !keep_prev {
                prev_char.clear();
            }
            keep_prev = false;
            if let Some(len) = self
                .try_inline_markup(src, &masked, &prev_char, &mut runs, &mut tokens)
                .or_else(|| self.try_inline_span(src, &mut tokens))
            {
                src = &src[len..];
                continue;
            }
            let cut = inline_latex_start(whole, src, &mut latex_start).map_or(src, |at| &src[..at]);
            if let Some(n) = inline_text_len(cut).inspect(|&n| work(n)) {
                let raw = cut[..n].to_string();
                src = &src[n..];
                if !raw.ends_with('_') {
                    prev_char = raw.chars().last().map_or_default(String::from);
                }
                keep_prev = true;
                push_text(
                    &mut tokens,
                    Token::Text {
                        text: raw,
                        tokens: None,
                    },
                );
                continue;
            }
            break;
        }
        tokens
    }

    fn link(&mut self, src: &str) -> Option<(usize, Token)> {
        let c = exec(&rules::LINK, src)?;
        let mut raw = g(&c, 0);
        let mut href = g(&c, 2);
        let trimmed = href.trim().to_string();
        if trimmed.starts_with('<') {
            if !trimmed.ends_with('>') {
                return None;
            }
            let body = &trimmed[..trimmed.len() - 1];
            if (trimmed.len() - rtrim(body, '\\').len()).is_multiple_of(2) {
                return None;
            }
        } else {
            let i = find_closing_paren(&href);
            if i == -2 {
                return None;
            }
            if i > -1 {
                let end = if raw.starts_with('!') { 5 } else { 4 } + c[1].len() + i as usize;
                href.truncate(i as usize);
                raw = raw[..end].trim().to_string();
            }
        }
        let mut href = href.trim().to_string();
        if href.starts_with('<') {
            href = href[1..href.len() - 1].to_string();
        }
        let href = replace(&rules::ANY_PUNCT, &href, "$1");
        let label = c[1].to_string();
        self.output_link(raw, label, href)
    }

    fn reflink(&mut self, src: &str) -> Option<(usize, Token)> {
        let c = exec(&rules::REFLINK, src).or_else(|| exec(&rules::NOLINK, src))?;
        let key = group(&c, 2).filter(|s| !s.is_empty()).unwrap_or(&c[1]);
        let key = key.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let Some((href, _)) = self.links.get(&key).cloned() else {
            let first = c[0][..1].to_string();
            return Some((
                1,
                Token::Text {
                    text: first,
                    tokens: None,
                },
            ));
        };
        self.output_link(g(&c, 0), c[1].to_string(), href)
    }

    fn output_link(&mut self, raw: String, label: String, href: String) -> Option<(usize, Token)> {
        let text = label.replace("\\[", "[").replace("\\]", "]");
        let image = raw.starts_with('!');
        self.in_link = true;
        let emitted = self.link_emitted;
        let raw_block = self.in_raw_block;
        self.link_emitted = false;
        let tokens = self.inline_tokens(&text);
        let inner_emitted = self.link_emitted;
        self.link_emitted = emitted;
        self.in_link = false;
        if image {
            return Some((
                raw.len(),
                Token::Other {
                    kind: "image".into(),
                    text: Some(text),
                },
            ));
        }
        if inner_emitted {
            self.in_raw_block = raw_block;
            return None;
        }
        self.link_emitted = true;
        Some((raw.len(), Token::Link { href, text, tokens }))
    }
}

/// `s.replace(blockSkip, m => `[${"a".repeat(m.length - 2)}]`)`. Every blockSkip match starts at `[`, `` ` `` or `<`,
/// so the rule is tried anchored at those bytes only (with the whole string still visible to its lookbehind) instead
/// of at every position, which the fancy-regex VM does for an unanchored search.
pub(super) fn mask_block_skip(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let (mut copied, mut i) = (0, 0);
    while let Some(off) = b[i..].iter().position(|c| matches!(c, b'[' | b'`' | b'<')) {
        let at = i + off;
        let m = rules::BLOCK_SKIP
            .find_input(RegexInput::new(s).from_pos(at).anchored(true))
            .ok()
            .flatten();
        work(off + m.as_ref().map_or(0, |m| m.end() - at));
        match m {
            Some(m) => {
                out.push_str(&s[copied..at]);
                out.push('[');
                out.push_str(&"a".repeat(m.end() - at - 2));
                out.push(']');
                copied = m.end();
                i = m.end();
            }
            None => i = at + 1,
        }
    }
    if copied == 0 {
        return s.to_string();
    }
    out.push_str(&s[copied..]);
    out
}

/// `[a-zA-Z0-9.!#$%&'*+/=?_`{|}~-]`: a character of an email local part as marked's inline text rule spells it.
fn is_email_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b".!#$%&'*+/=?_`{|}~-".contains(&c)
}

/// End of the maximal run of bytes satisfying `f` that starts at `i`, remembered per run so repeated lookups inside
/// one run cost one scan of it.
#[derive(Default)]
struct RunEnd {
    start: usize,
    end: usize,
}

impl RunEnd {
    fn get(&mut self, b: &[u8], i: usize, f: fn(u8) -> bool) -> usize {
        if !(self.start..self.end).contains(&i) {
            self.start = i;
            self.end = i + b[i..].iter().take_while(|&&c| f(c)).count();
            work(self.end - i);
        }
        self.end
    }
}

fn is_ascii_space_byte(c: u8) -> bool {
    c == b' '
}

fn break_ahead(b: &[u8], i: usize, spaces: &mut RunEnd) -> bool {
    if b.get(i) != Some(&b' ') {
        return false;
    }
    let e = spaces.get(b, i, is_ascii_space_byte);
    e - i >= 2 && b.get(e) == Some(&b'\n')
}

fn email_ahead(b: &[u8], i: usize, email: &mut RunEnd) -> bool {
    b.get(i).copied().is_some_and(is_email_char) && b.get(email.get(b, i, is_email_char)) == Some(&b'@')
}

fn starts_ascii_ignore_case(rest: &[u8], pat: &[u8]) -> bool {
    rest.len() >= pat.len() && rest[..pat.len()].eq_ignore_ascii_case(pat)
}

fn is_inline_stop(rest: &[u8]) -> bool {
    rest.is_empty()
        || b"\\<![`*~_".contains(&rest[0])
        || starts_ascii_ignore_case(rest, b"http")
        || starts_ascii_ignore_case(rest, b"ftp://")
        || rest.starts_with(b"www.")
}

fn inline_text_prefix_len(s: &str, b: &[u8], first: u8) -> usize {
    if first == b'`' || first == b'~' {
        b.iter().take_while(|&&c| c == first).count()
    } else {
        second_char(s)
    }
}

fn inline_prefix_stops(b: &[u8], p: usize, spaces: &mut RunEnd, email: &mut RunEnd) -> bool {
    b.get(p).is_some_and(|c| *c == b'`' || *c == b'~') || break_ahead(b, p, spaces) || email_ahead(b, p, email)
}

fn inline_next_stops(b: &[u8], first_byte: u8, next: usize, spaces: &mut RunEnd, email: &mut RunEnd) -> bool {
    (first_byte != b' ' && break_ahead(b, next, spaces)) || (!is_email_char(first_byte) && email_ahead(b, next, email))
}

/// Length of marked's inline text token at the start of `s` (the `inlineText` rule, `None` for empty `s`):
///
/// ```text
/// ^(`+|~+|[^`~])(?:(?=[`~])|(?= {2,}\n)|(?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)|[\s\S]*?(?:(?=[\\<!\[`*~_]
///   |(?<![A-Za-z0-9_])_|[hH][tT][tT][pP][sS]?|[fF][tT][pP]://|www\.|$)|[^ ](?= {2,}\n)
///   |[^a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-](?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)))
/// ```
///
/// The rule is a lazy scan with lookaheads at every character; run by fancy-regex's backtracking VM it was the
/// largest single cost of lexing. This scanner tries the same alternatives in the same order at each position.
pub(super) fn inline_text_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let first = *b.first()?;
    let p = inline_text_prefix_len(s, b, first);
    let mut spaces = RunEnd::default();
    let mut email = RunEnd::default();
    if inline_prefix_stops(b, p, &mut spaces, &mut email) {
        return Some(p);
    }
    let mut q = p;
    loop {
        let rest = &b[q..];
        if is_inline_stop(rest) {
            return Some(q);
        }
        let next = q + second_char(&s[q..]);
        if inline_next_stops(b, rest[0], next, &mut spaces, &mut email) {
            return Some(next);
        }
        q = next;
    }
}

pub(super) fn checkbox() -> Token {
    Token::Other {
        kind: "checkbox".into(),
        text: None,
    }
}

fn link_token(href: String, text: String) -> Token {
    Token::Link {
        href,
        tokens: vec![Token::Text {
            text: text.clone(),
            tokens: None,
        }],
        text,
    }
}

fn push_text(tokens: &mut Vec<Token>, t: Token) {
    if let (
        Some(Token::Text {
            text: last,
            tokens: None,
        }),
        Token::Text { text, tokens: None },
    ) = (tokens.last_mut(), &t)
    {
        last.push_str(text);
        return;
    }
    tokens.push(t);
}
