//! Word segments like ICU4C's word break iterator: Node's `Intl.Segmenter(undefined, { granularity: "word" })`,
//! which tui-alt-screen.ts `getWordSelection` uses for double-click selection.
//!
//! Two layers, as in ICU4C:
//! - The word rules (ICU's word.txt: UAX #29 tailored so that Han, Hiragana and Katakana chain into one run, Hangul
//!   into another, and complex-context letters such as Thai count as ALetter), with each rule's status deciding
//!   `isWordLike` (status >= 100).
//! - Rule segments holding dictionary characters are subdivided by ICU4C's break engines over the ICU dictionaries
//!   (the same cjdict/thaidict/laodict/khmerdict/burmesedict tries, shipped by `icu_segmenter_data`): CJ text by
//!   `CjkBreakEngine`'s best path over word costs (with the Katakana run cost), Thai, Lao, Khmer and Burmese by the
//!   `ThaiBreakEngine` family's three-word lookahead. ICU4X's own segmenter is not used: its dictionary segmenter is
//!   a greedy longest match and its word rules differ (Katakana and Thai runs, single-character statuses).

mod dict;
mod rules;

use dict::populate;
use rules::rule_segments;

/// The word segments of `line` and whether each is word-like (`isWordLike`), like Node's `Intl.Segmenter`.
pub fn word_segments(line: &str) -> Vec<(&str, bool)> {
    let chars: Vec<char> = line.chars().collect();
    let mut byte_at = Vec::with_capacity(chars.len() + 1);
    let mut b = 0;
    for c in &chars {
        byte_at.push(b);
        b += c.len_utf8();
    }
    byte_at.push(b);
    let mut out = Vec::new();
    for (s, e, status) in rule_segments(&chars) {
        let word_like = status >= 100;
        match populate(&chars, s, e) {
            Some(bounds) => {
                for w in bounds.windows(2) {
                    out.push((&line[byte_at[w[0]]..byte_at[w[1]]], word_like));
                }
            }
            None => out.push((&line[byte_at[s]..byte_at[e]], word_like)),
        }
    }
    out
}
