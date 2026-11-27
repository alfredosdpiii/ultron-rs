//! LaTeX math to terminal Unicode: port of packages/tui/src/latex.ts (`renderLatex`). Inline math renders on one
//! line; display math stacks fractions, operator limits, scripts, cases and matrices over several lines.
//! Positions index Unicode scalar values where TypeScript indexes UTF-16 units; both see the same characters.

mod layout;
mod parser;
mod tables;

use std::sync::LazyLock;

use fancy_regex::Regex;

use jsrt::string;
use layout::render_layout;
use parser::Parser;

const PROTECTED_SPACE: char = '\u{f0002}';

/// `renderLatex(source, { display })`: Unicode text for a LaTeX math expression, or `None` when the expression
/// uses unsupported or malformed syntax.
pub fn render_latex(source: &str, display: bool) -> Option<String> {
    let mut nodes = Vec::new();
    let rendered = Parser::new(source, &mut nodes, display).render()?;
    let protect = |s: &str| s.replace(PROTECTED_SPACE, " ");
    if nodes.is_empty() {
        return Some(protect(&rendered));
    }
    let lines = render_layout(&rendered, &nodes).lines;
    let indentation = lines
        .iter()
        .filter(|l| !string::trim(l).is_empty())
        .map(|l| l.len() - l.trim_start_matches(string::is_space).len())
        .min()
        .unwrap_or(0);
    let out: Vec<&str> = lines
        .iter()
        .map(|l| l.get(indentation..).unwrap_or("").trim_end_matches(string::is_space))
        .collect();
    Some(protect(out.join("\n").trim_end_matches(string::is_space)))
}

static BLOCK_DOLLAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*?)\$\$[ \t]*(?:\n|$)").expect("regex"));
static BLOCK_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*?)\\\][ \t]*(?:\n|$)").expect("regex"));
static BLOCK_PENDING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}(?:\\\[|\$\$)[ \t]*(?:\n)?([\s\S]*)$").expect("regex"));

fn is_escaped(s: &str, index: usize) -> bool {
    s.as_bytes()[..index].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

/// The math source of a `latexBlock` token (markdown.ts `tokenizeBlockLatex`), recovered from its raw text, and
/// whether the block is still unclosed (`pending`).
fn block_text(raw: &str) -> (String, bool) {
    for r in [&*BLOCK_DOLLAR, &*BLOCK_BRACKET] {
        if let Ok(Some(c)) = r.captures(raw)
            && let Some(text) = c.get(1).filter(|m| !m.as_str().is_empty())
        {
            return (string::trim(text.as_str()).to_string(), false);
        }
    }
    let text = BLOCK_PENDING
        .captures(raw)
        .ok()
        .flatten()
        .and_then(|c| c.get(1))
        .map_or("", |m| m.as_str());
    (text.to_string(), true)
}

/// The math source of an inline `latex` token (markdown.ts `tokenizeInlineLatex`) and whether it is unclosed.
fn inline_text(raw: &str) -> (String, bool) {
    let (open, close) = [("$$", "$$"), ("\\(", "\\)"), ("\\[", "\\]"), ("$", "$")]
        .into_iter()
        .find(|(o, _)| raw.starts_with(o))
        .unwrap_or(("", ""));
    let closed = raw.len() >= open.len() + close.len() && raw.ends_with(close) && {
        let end = raw.len() - close.len();
        let mut i = raw[open.len()..].find(close).map(|i| i + open.len());
        while let Some(at) = i
            && is_escaped(raw, at)
        {
            i = raw[at + close.len()..].find(close).map(|j| j + at + close.len());
        }
        i == Some(end)
    };
    if closed {
        (raw[open.len()..raw.len() - close.len()].to_string(), false)
    } else {
        (raw[open.len()..].to_string(), true)
    }
}

/// What the Markdown component shows for a `latexBlock` (`block`) or inline `latex` token: rendered Unicode math,
/// or the token's source when it is unclosed, unsupported, or `render` is off (markdown.ts `renderLatex: false`).
pub fn render_latex_token(raw: &str, block: bool, render: bool) -> String {
    let (text, pending) = if block { block_text(raw) } else { inline_text(raw) };
    let fallback = if block { string::trim(raw) } else { raw };
    if pending || !render {
        return fallback.to_string();
    }
    render_latex(&text, block).unwrap_or_else(|| fallback.to_string())
}
