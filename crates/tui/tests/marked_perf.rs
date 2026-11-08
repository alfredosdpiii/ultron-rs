//! Lexer cost of the Markdown component (tui::marked).
//!
//! `tui_marked_lex_work_grows_linearly` guards against quadratic regressions without a clock: it lexes generated
//! documents at 1x and 4x size and compares the lexer's own work counter (`tui::lex_work`: rule calls plus the
//! bytes each can scan), which is the same on every run and every machine. Linear lexing gives a ratio of ~4, a
//! lexer that rescans the rest of the input per token ~16.

use tui::{lex_work, markdown_tokens};

/// One assistant-style section: headings, prose with emphasis, links and code spans, lists, a table, a quote, a code
/// fence, a reference link and a bit of math. `i` varies the text so no two sections are identical.
fn section(i: usize) -> String {
    format!(
        "## Step {i}: update the `parser_{i}` module\n\n\
         The **lexer** reads _each_ line once and the *renderer* wraps it; see [the docs](https://example.com/d/{i}) \
         and <https://example.org/{i}> or www.example.net/p{i}. A ~~stale~~ value of `x_{i}` costs $O(n)$ here, \
         and snake_case_names_{i} must not turn into emphasis.\n\n\
         - first item with **bold {i}** and a [ref link][r{i}]\n\
         - second item\n  continued on a lazy line with `code`\n  - nested item {i}\n    - deeper *item*\n\
         - [x] done task\n\
         - [ ] open task\n\n\
         1. ordered one\n2. ordered two with \\*escaped\\* stars\n3. ordered three\n\n\
         | Name | Value | Note |\n|:-----|------:|:----:|\n| a{i} | {i} | **ok** |\n| b | 2 | `c|d` |\n\n\
         > A quote with *emphasis* and a [link](https://example.com/q{i}).\n> Second quoted line.\n\n\
         ```rust\nfn f_{i}(x: usize) -> usize {{\n    // **not bold** in code\n    x * {i}\n}}\n```\n\n\
         [r{i}]: https://example.com/ref/{i} \"Ref {i}\"\n\n"
    )
}

fn assistant_doc(bytes: usize) -> String {
    let mut s = String::from("# Plan\n\n");
    let mut i = 0;
    while s.len() < bytes {
        s.push_str(&section(i));
        i += 1;
    }
    s
}

/// The lexer's work on `doc` (deterministic: the counter is per thread and the lexer has no randomness).
fn work_of(doc: &str) -> u64 {
    let before = lex_work();
    markdown_tokens(doc);
    lex_work() - before
}

/// Lexing work must grow linearly: 4x the input may cost at most 5x the work (quadratic would be ~16x). The ratio is
/// the same on every run; the bound leaves room only for per-document constants.
fn assert_linear_work(name: &str, make: impl Fn(usize) -> String, unit: usize) {
    let (small, large) = (make(unit), make(unit * 4));
    let (s, l) = (work_of(&small), work_of(&large));
    assert_eq!(work_of(&small), s, "{name}: the work count is not deterministic");
    let ratio = l as f64 / s as f64;
    println!("{name:<34} work {s:>9} -> {l:>9} ({ratio:.2}x for 4x the input)");
    assert!(
        ratio < 5.0,
        "{name}: lexing 4x the input did {ratio:.2}x the work ({s} -> {l})"
    );
}

#[test]
fn tui_marked_lex_work_grows_linearly() {
    assert_linear_work("assistant document", assistant_doc, 40_000);
    assert_linear_work("one long paragraph", |n| "word *em* _u_ `c` ".repeat(n / 18), 40_000);
    assert_linear_work("many short paragraphs", |n| "para line\n\n".repeat(n / 11), 40_000);
    assert_linear_work("unclosed delimiters", |n| "*a _b **c ".repeat(n / 10), 10_000);
    assert_linear_work("nested delimiters", |n| "*_a_* b_*c ".repeat(n / 11), 10_000);
    assert_linear_work("unclosed brackets and backticks", |n| "[a `b <c ".repeat(n / 9), 10_000);
    assert_linear_work("long line", |n| "word ".repeat(n / 5), 40_000);
    assert_linear_work("** pairs", |n| "**a** ".repeat(n / 6), 10_000);
    assert_linear_work(
        "nested quotes",
        |n| (1..=n / 400).map(|d| format!("> line {d}\n")).collect(),
        4_000,
    );
    assert_linear_work("short lines", |n| "a\n".repeat(n / 2), 20_000);
    assert_linear_work("inline math", |n| "x $a+b$ and \\(c\\) y ".repeat(n / 22), 10_000);
    assert_linear_work("unclosed math", |n| "$a ".repeat(n / 3), 10_000);
}
