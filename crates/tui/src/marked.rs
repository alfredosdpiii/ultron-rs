//! Lexer for the Markdown component: a port of the marked v18.0.11 GFM lexer (block and inline rules, MIT,
//! Copyright (c) 2018-2026 MarkedJS, (c) 2011-2018 Christopher Jeffrey) configured the way
//! packages/tui/src/components/markdown.ts configures it: strict `~~` strikethrough and the `latexBlock` / `latex`
//! extensions. It produces the token tree `crate::markdown` renders. Offsets are UTF-8 bytes where marked uses
//! UTF-16 units; masks keep byte lengths so the two agree on every slice.

mod block;
mod delim;
mod inline;
mod rules;

use std::collections::HashMap;

use fancy_regex::{Captures, Regex, RegexInput};

use crate::markdown::{Token, trim_partial_closing_fences};

type Caps<'t> = Captures<'t, str>;

thread_local! {
    static WORK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Charge `units` of scanning work to this thread's counter (see [`lex_work`]).
fn work(units: usize) {
    WORK.with(|w| w.set(w.get() + units as u64 + 1));
}

/// A deterministic measure of the lexer's work on this thread so far, for complexity tests (no clocks): one unit
/// per rule call plus the bytes it can scan. An anchored rule is charged its match length (the bytes it consumed;
/// a failed one only its call), an unanchored search, replace or iteration the whole haystack it was given (the
/// most it can read), a substring search the bytes up to where it stopped. A lexer that rescans the rest of the
/// document per token shows up here as quadratic growth, whatever the machine load.
pub fn lex_work() -> u64 {
    WORK.with(std::cell::Cell::get)
}

fn re(s: &str) -> Regex {
    Regex::new(s).expect("marked rule")
}

/// The item rule for a list opened by `bull` (`-`, `+`, `*`, or digits and `.`/`)`).
fn item_rule(bull: &str) -> &'static Regex {
    let i = match bull {
        "*" => 0,
        "+" => 1,
        "-" => 2,
        b if b.ends_with('.') => 3,
        _ => 4,
    };
    &rules::LIST_ITEM[i]
}

/// `rule.exec(s)` for a rule whose every branch starts with `^`. The search is anchored at the start: fancy-regex only
/// anchors a pattern on its own when it is a single `^…` sequence without a trailing lookahead, and otherwise retries
/// at every later position of `s`, which made each call cost the length of the unlexed rest of the document.
fn exec<'t>(r: &Regex, s: &'t str) -> Option<Caps<'t>> {
    let caps = r.captures_input(RegexInput::new(s).anchored(true)).ok().flatten();
    work(caps.as_ref().and_then(|c| c.get(0)).map_or(0, |m| m.end()));
    caps
}

/// `rule.test(s)` for a rule whose every branch starts with `^` (see [`exec`]).
fn test(r: &Regex, s: &str) -> bool {
    work(0);
    r.is_match_input(RegexInput::new(s).anchored(true)).unwrap_or(false)
}

/// Unanchored `rule.exec(s)`.
fn search<'t>(r: &Regex, s: &'t str) -> Option<Caps<'t>> {
    work(s.len());
    r.captures(s).ok().flatten()
}

/// Unanchored `rule.test(s)`.
fn contains(r: &Regex, s: &str) -> bool {
    work(s.len());
    r.is_match(s).unwrap_or(false)
}

fn group<'t>(c: &Caps<'t>, i: usize) -> Option<&'t str> {
    c.get(i).map(|m| m.as_str())
}

fn g(c: &Caps, i: usize) -> String {
    group(c, i).unwrap_or("").to_string()
}

fn replace(r: &Regex, s: &str, with: &str) -> String {
    work(s.len());
    r.replace_all(s, with).into_owned()
}

/// `s.slice(i)` that tolerates out-of-range and mid-character indices.
fn from(s: &str, mut i: usize) -> &str {
    if i >= s.len() {
        return "";
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

fn rtrim(s: &str, c: char) -> &str {
    s.trim_end_matches(c)
}

/// Byte offset of `s.slice(1)` (`from(s, 1)`): the length of the first character.
fn second_char(s: &str) -> usize {
    s.chars().next().map_or(0, char::len_utf8)
}

/// The next match of a search at or after a moving start offset into `whole`. The lexers only ever drop a prefix of
/// `whole`, so a match found once stays the answer until the start passes it: repeated "find the next X in the rest"
/// scans cost one pass over `whole` instead of one pass per token.
#[derive(Default)]
struct NextAt {
    searched: bool,
    at: Option<usize>,
}

impl NextAt {
    fn get(&mut self, whole: &str, start: usize, find: impl Fn(&str) -> Option<usize>) -> Option<usize> {
        if !self.searched || self.at.is_some_and(|a| a < start) {
            self.searched = true;
            self.at = find(&whole[start..]).map(|i| i + start);
            work(self.at.unwrap_or(whole.len()) - start);
        }
        self.at
    }
}

struct Lexer {
    links: HashMap<String, (String, String)>,
    in_link: bool,
    in_raw_block: bool,
    link_emitted: bool,
    top: bool,
}

/// Tokens the Markdown component renders for `text`: marked's GFM lexer with markdown.ts's strict strikethrough and
/// LaTeX extensions, after tabs become three spaces and streamed partial closing fences are trimmed. Blank input
/// renders nothing.
pub fn markdown_tokens(text: &str) -> Vec<Token> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut tokens = lex(&text.replace('\t', "   "));
    trim_partial_closing_fences(&mut tokens);
    tokens
}

/// `markdownParser.lexer(src)`.
pub fn lex(src: &str) -> Vec<Token> {
    let src = src.replace("\r\n", "\n").replace('\r', "\n");
    let mut lexer = Lexer {
        links: HashMap::new(),
        in_link: false,
        in_raw_block: false,
        link_emitted: false,
        top: true,
    };
    let mut blocks = Vec::new();
    lexer.block_tokens(&src, &mut blocks, false);
    lexer.convert_all(blocks)
}

#[cfg(test)]
mod tests {
    use super::delim::{RunTable, delimiter_runs};
    use super::inline::{inline_text_len, mask_block_skip};
    use super::*;

    /// The closing-delimiter matches marked's emStrongRDelim regex finds scanning `masked` from `hay_start`, as
    /// (group, run start, run length): the reference the delimiter-run classifier must reproduce.
    fn regex_closers(masked: &str, hay_start: usize, delim: u8) -> Vec<(u8, usize, usize)> {
        let r = if delim == b'*' {
            &*rules::EM_R_AST
        } else {
            &*rules::EM_R_UND
        };
        r.captures_iter(&masked[hay_start..])
            .flatten()
            .filter_map(|c| {
                (1..=6).find_map(|i| {
                    let m = c.get(i).filter(|m| !m.as_str().is_empty())?;
                    Some((i as u8, hay_start + m.start(), m.as_str().len()))
                })
            })
            .collect()
    }

    fn run_closers(masked: &str, hay_start: usize, delim: u8) -> Vec<(u8, usize, usize)> {
        let skip = if delim == b'*' {
            &*rules::EM_R_AST_SKIP
        } else {
            &*rules::EM_R_UND_SKIP
        };
        let skip_to = exec(skip, &masked[hay_start..]).map_or(0, |c| c[0].len());
        delimiter_runs(masked, delim)
            .into_iter()
            .filter(|r| r.start >= hay_start + skip_to.max(1))
            .filter_map(|r| r.class.map(|c| (c, r.start, r.len)))
            .collect()
    }

    /// The closing search by prefix sums and segment tree ends where marked's one-run-at-a-time walk does, from
    /// every start run, for every opener length and previous character, on generated delimiter-heavy sources.
    #[test]
    fn tui_marked_closing_search_matches_the_walk() {
        let mut next = xorshift(0x5eed_1234_abcd_0001);
        let alphabet = [
            "*", "_", "**", "__", "***", " ", "a", "b", ".", ",", "!", "~", "\n", "é",
        ];
        for _ in 0..3000 {
            let masked = generated(&mut next, &alphabet, 40);
            for delim in *b"*_" {
                let mut table = RunTable::new(delimiter_runs(&masked, delim));
                for first in 0..=table.run_count() {
                    for l_len in 1..=3 {
                        for same_prev in [false, true] {
                            assert_eq!(
                                table.close(first, l_len, same_prev),
                                table.close_by_walk(first, l_len, same_prev),
                                "{masked:?} delim {} first {first} l_len {l_len} same_prev {same_prev}",
                                delim as char
                            );
                        }
                    }
                }
            }
        }
    }

    fn xorshift(mut seed: u64) -> impl FnMut() -> usize {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        }
    }

    fn generated(next: &mut impl FnMut() -> usize, alphabet: &[&str], max: usize) -> String {
        let len = 1 + next() % max;
        (0..len).map(|_| alphabet[next() % alphabet.len()]).collect()
    }

    /// The inline text scanner ends every token where marked's `inlineText` regex does, on generated strings over the
    /// characters its alternatives test (stop characters, `http`/`ftp://`/`www.` prefixes, double-space line breaks,
    /// email local parts before `@`, backtick and tilde runs, non-ASCII).
    #[test]
    fn tui_marked_inline_text_scanner_matches_the_regex() {
        let alphabet = [
            "a", "Z", "0", " ", " ", "  \n", "\n", "`", "~", "_", "*", "\\", "<", "!", "[", "@", ".", "-", "é", "€",
            "http", "HTTPs", "ftp://", "FTP:/", "www.", "ww", "x@y", "(",
        ];
        let mut next = xorshift(0x9e37_79b9_7f4a_7c15);
        for _ in 0..50_000 {
            let s = generated(&mut next, &alphabet, 16);
            let want = exec(&rules::INLINE_TEXT, &s).map(|c| c[0].len());
            assert_eq!(inline_text_len(&s), want, "{s:?}");
        }
        assert_eq!(inline_text_len(""), None);
    }

    /// Masking code spans, links and tags by candidate positions equals blockSkip's `replace_all`.
    #[test]
    fn tui_marked_block_skip_mask_matches_the_regex() {
        let alphabet = ["[", "]", "(", ")", "`", "``", "<", ">", " ", "a", "\\", "!", "é", "\n"];
        let mut next = xorshift(0x2545_f491_4f6c_dd1d);
        for _ in 0..50_000 {
            let s = generated(&mut next, &alphabet, 24);
            let want = rules::BLOCK_SKIP
                .replace_all(&s, |c: &Caps| format!("[{}]", "a".repeat(c[0].len() - 2)))
                .into_owned();
            assert_eq!(mask_block_skip(&s), want, "{s:?}");
        }
    }

    /// Generated strings over delimiters, word characters, spaces, ASCII and Unicode punctuation and symbols, `~`
    /// and non-ASCII spaces, scanned from every start a left delimiter can leave (just after a run, before a
    /// non-delimiter character): the per-run classes equal the regex's matches.
    #[test]
    fn tui_marked_delimiter_runs_match_the_em_strong_regex() {
        let alphabet = [
            "*", "*", "_", "_", "a", "é", " ", "\u{a0}", "\n", ".", "~", "\"", "!", "«", "–", "€", "+", "[", "]", "0",
        ];
        let mut next = xorshift(0x2545_f491_4f6c_dd1d);
        let mut checked = 0;
        for _ in 0..20_000 {
            let s = generated(&mut next, &alphabet, 40);
            for delim in *b"*_" {
                for (start, ch) in s.char_indices() {
                    let prev_is_delim = s[..start].ends_with(delim as char);
                    if !prev_is_delim || ch as u32 == u32::from(delim) {
                        continue;
                    }
                    assert_eq!(
                        run_closers(&s, start, delim),
                        regex_closers(&s, start, delim),
                        "{s:?} from {start} ({})",
                        delim as char
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 50_000, "only {checked} scans");
    }
}
