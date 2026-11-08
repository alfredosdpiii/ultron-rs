//! `highlightCode` (packages/coding-agent/src/modes/interactive/theme/theme.ts) for the `rlm` cell renderer: the
//! highlight.js port in [`tui::hljs`], themed with the TS `buildCliHighlightTheme` mapping and split into lines.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use theme::styles::TextStyle;
use theme::{Fg, Theme};
use tui::hljs::Token;

use crate::rlm_cell::Highlighter;

/// How a highlight.js scope is drawn.
#[derive(Clone, Copy)]
enum Style {
    Color(Fg),
    Text(TextStyle),
}

/// `buildCliHighlightTheme`: the style for each highlight.js scope that has one.
fn scope_style(scope: &str) -> Option<Style> {
    Some(match scope {
        "keyword" | "name" => Style::Color(Fg::SyntaxKeyword),
        "built_in" | "class" | "type" => Style::Color(Fg::SyntaxType),
        "literal" | "number" => Style::Color(Fg::SyntaxNumber),
        "regexp" | "string" => Style::Color(Fg::SyntaxString),
        "comment" | "doctag" => Style::Color(Fg::SyntaxComment),
        "meta" => Style::Color(Fg::Muted),
        "function" | "title" => Style::Color(Fg::SyntaxFunction),
        "tag" | "punctuation" => Style::Color(Fg::SyntaxPunctuation),
        "attr" | "variable" | "params" => Style::Color(Fg::SyntaxVariable),
        "operator" => Style::Color(Fg::SyntaxOperator),
        "emphasis" => Style::Text(TextStyle::Italic),
        "strong" => Style::Text(TextStyle::Bold),
        "link" => Style::Text(TextStyle::Underline),
        "addition" => Style::Color(Fg::ToolDiffAdded),
        "deletion" => Style::Color(Fg::ToolDiffRemoved),
        _ => return None,
    })
}

/// `highlightCode(code, lang)`: one styled string per line. A language highlight.js does not know is shown in
/// `mdCodeBlock`; code highlight.js gives up on comes back plain.
pub fn highlight_code(theme: &Theme, code: &str, lang: Option<&str>) -> Vec<String> {
    let Some(lang) = lang.filter(|l| tui::hljs::supports_language(l)) else {
        return code.split('\n').map(|line| theme.fg(Fg::MdCodeBlock, line)).collect();
    };
    themed(theme, code, tui::hljs::highlight(code, lang).as_deref())
}

/// The token list in the theme's colors, split into lines (`None`: the plain code).
fn themed(theme: &Theme, code: &str, tokens: Option<&[Token]>) -> Vec<String> {
    let Some(tokens) = tokens else {
        return code.split('\n').map(str::to_string).collect();
    };
    let apply = |style: &Style, text: &str| match *style {
        Style::Text(style) => style.apply(text, 1),
        Style::Color(color) => theme.fg(color, text),
    };
    tui::hljs::render(tokens, &scope_style, &apply)
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// Cells whose tokens are kept: the TS renderer highlights a cell once per change of its code (`state.highlighted`),
/// and a frame repaints the same cells again.
const CACHED_CELLS: usize = 64;

/// The `rlm` cell highlighter: `highlightCode(code, "python")` with the theme's current colors. A cell's tokens are
/// computed once per code and themed on every call, so a theme change shows at once.
pub fn rlm_highlighter(theme: &Rc<Theme>) -> Highlighter {
    let theme = Rc::clone(theme);
    let cache: RefCell<VecDeque<(String, Option<Vec<Token>>)>> = RefCell::default();
    Rc::new(move |code: &str| {
        let mut cache = cache.borrow_mut();
        let at = match cache.iter().position(|(cached, _)| cached == code) {
            Some(at) => at,
            None => {
                if cache.len() == CACHED_CELLS {
                    cache.pop_front();
                }
                cache.push_back((code.to_string(), tui::hljs::highlight(code, "python")));
                cache.len() - 1
            }
        };
        themed(&theme, code, cache[at].1.as_deref())
    })
}
