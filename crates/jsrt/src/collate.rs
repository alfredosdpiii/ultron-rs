//! `a.localeCompare(b)` with no locale (ICU root collation, the TypeScript product's default). The order of
//! `Array.prototype.sort()` with no comparator (UTF-16 code units) is [`crate::utf16::cmp`].
//!
//! `locale_compare` is ICU's three-level comparison: primary (whitespace, then punctuation and symbols in CLDR order,
//! then currency, then digits, then letters ignoring case and accents), then secondary (accents), then tertiary (case,
//! lowercase first; ligatures such as `æ` and `ß` after every case variant of their expansion). Variable characters
//! are not ignored (ICU's default `alternate=non-ignorable`), and control and format characters are ignorable
//! (`"a\u{200b}b"` equals `"ab"`). The tables (`collate/tables.rs`, generated from Node's ICU by
//! `scripts/gen-collate-tables.mjs`) cover JavaScript whitespace, ASCII, Latin-1, General Punctuation, currency signs,
//! combining marks and Latin Extended-A; random strings over those agree with Node 26 (ICU 78) on every pair.
// ponytail: outside the tables, letters sort after the Latin ones by lowercase code point, other numerals after 9,
// private use after every letter, and other symbols just before currency, where ICU orders scripts (Greek before
// Cyrillic...), equates non-ASCII digits with ASCII ones and expands compatibility forms (`ǅ`, `ﬁ`); stacked
// accents are not canonically reordered. No `numeric` option: review/prepare_env keeps its own numeric comparison.
// ponytail: each comparison collects both strings' elements into a Vec; callers sort short lists.

use std::cmp::Ordering;

mod tables;

/// Primary weight bases, in ICU's group order. Symbols the tables do not list sort after those they do and before
/// currency, as ICU's symbol group does.
const SYMBOL: u32 = 1;
const OTHER_SYMBOL: u32 = 1_000;
const CURRENCY: u32 = 2_000_000;
const DIGIT: u32 = 3_000_000; // + the digit; other numerals after 9 by code point
const LETTER: u32 = 4_000_000; // + a LETTERS primary, below 0x10000
const OTHER_LETTER: u32 = 5_000_000;

/// One collation element: primary, secondary, tertiary weight. `p == 0` is an accent (secondary only).
#[derive(Clone, Copy, Default)]
struct Ce {
    p: u32,
    s: u16,
    t: u8,
}

/// A character's collation elements (at most four: `⁗` is four primes, `₧` three letters).
#[derive(Clone, Copy, Default)]
struct Ces {
    ces: [Ce; 4],
    len: u8,
    at: u8,
}

const fn ce(p: u32, s: u16, t: u8) -> Ce {
    Ce { p, s, t }
}

impl Ces {
    fn push(&mut self, ce: Ce) {
        self.ces[usize::from(self.len)] = ce;
        self.len += 1;
    }
}

impl Iterator for Ces {
    type Item = Ce;
    fn next(&mut self) -> Option<Ce> {
        (self.at < self.len).then(|| {
            self.at += 1;
            self.ces[usize::from(self.at - 1)]
        })
    }
}

/// Default-ignorable format characters ICU skips entirely (control characters are handled by `is_control`).
fn ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{34f}'
            | '\u{61c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
    ) || (c.is_control() && !matches!(c, '\t'..='\r' | '\u{85}'))
}

fn symbol(c: char) -> Option<(&'static [u16], u8, bool)> {
    let i = tables::SYMBOLS.binary_search_by_key(&c, |row| row.0).ok()?;
    let (_, ranks, t, currency) = tables::SYMBOLS[i];
    Some((ranks, t, currency))
}

/// Tertiary weights: 0 lowercase or caseless, 1 a small variant (`ͣ`), 2 uppercase, 3 and 4 a compatibility form
/// (`ª`, `²`, `½`, `æ`; `Æ`).
fn ces(c: char) -> Ces {
    let mut out = Ces::default();
    let digit = |d: u32, t: u8| Ce { p: DIGIT + d, s: 0, t };
    if ignorable(c) {
        return out;
    }
    if c.is_ascii_digit() {
        out.push(digit(c as u32 - '0' as u32, 0));
    } else if c.is_ascii_alphabetic() {
        let p = LETTER + (c.to_ascii_lowercase() as u32 - 'a' as u32 + 1) * 16;
        out.push(ce(p, 0, 2 * u8::from(c.is_ascii_uppercase())));
    } else if let Some(d) = "¹²³".chars().position(|x| x == c) {
        out.push(digit(d as u32 + 1, 3));
    } else if let Some(n) = "¼½¾".chars().position(|x| x == c) {
        // `1⁄4`, `1⁄2`, `3⁄4`.
        let slash = symbol('\u{2044}').map_or(0, |(ranks, _, _)| u32::from(ranks[0]));
        out.push(digit(if n == 2 { 3 } else { 1 }, 3));
        out.push(ce(SYMBOL + slash, 0, 3));
        out.push(digit(if n == 1 { 2 } else { 4 }, 3));
    } else if let Some((ranks, t, currency)) = symbol(c) {
        for rank in ranks {
            out.push(Ce {
                p: if currency { CURRENCY } else { SYMBOL } + u32::from(*rank),
                s: 0,
                t,
            });
        }
    } else if let Ok(i) = tables::MARKS.binary_search_by_key(&c, |row| row.0) {
        out.push(ce(0, tables::MARKS[i].1, 0));
    } else if let Ok(i) = tables::LETTERS.binary_search_by_key(&c, |row| row.0) {
        let (_, primaries, s, t) = tables::LETTERS[i];
        for (i, p) in primaries.into_iter().filter(|p| *p != 0).enumerate() {
            out.push(Ce {
                p: LETTER + u32::from(p),
                s: 0,
                t,
            });
            // The accent (or a ligature's own secondary) follows the first letter, as ICU's elements do.
            if i == 0 && s != 0 {
                out.push(Ce { p: 0, s, t: 0 });
            }
        }
    } else if c.is_numeric() {
        out.push(ce(DIGIT + 10 + c as u32, 0, 0));
    } else if matches!(c, '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..) {
        // Private use: ICU's implicit weights, after every script.
        out.push(ce(OTHER_LETTER + 0x11_0000 + c as u32, 0, 0));
    } else if c.is_alphanumeric() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        out.push(ce(OTHER_LETTER + lower as u32, 0, 2 * u8::from(c.is_uppercase())));
    } else {
        out.push(ce(OTHER_SYMBOL + c as u32, 0, 0));
    }
    out
}

/// `a.localeCompare(b)` (no locale, no options). Strings ICU finds equal (`"a\u{200b}b"`, `"ab"`) are `Equal`, so a
/// stable sort keeps their input order as `Array.prototype.sort` does.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let elements = |s: &str| s.chars().flat_map(ces).collect::<Vec<Ce>>();
    let (a, b) = (elements(a), elements(b));
    let primary = |v: &[Ce]| v.iter().filter(|ce| ce.p != 0).map(|ce| ce.p).collect::<Vec<_>>();
    let secondary = |v: &[Ce]| v.iter().map(|ce| ce.s).collect::<Vec<_>>();
    let tertiary = |v: &[Ce]| v.iter().filter(|ce| ce.p != 0).map(|ce| ce.t).collect::<Vec<_>>();
    primary(&a)
        .cmp(&primary(&b))
        .then_with(|| secondary(&a).cmp(&secondary(&b)))
        .then_with(|| tertiary(&a).cmp(&tertiary(&b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(items: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = items.iter().rev().map(|s| s.to_string()).collect();
        out.sort_by(|a, b| locale_compare(a, b));
        out
    }

    /// The order the TypeScript product's `localeCompare` sort gave (tui/fixtures/editor-autocomplete-parity.json,
    /// `collate`): whitespace, punctuation in CLDR order, symbols, currency, digits, then letters, lowercase first.
    #[test]
    fn jsrt_locale_compare_matches_the_recorded_typescript_sort() {
        let expected = [
            "",
            "_x",
            ".git",
            ".hidden",
            "@at",
            "#tag",
            "~home",
            "$var",
            "1x",
            "a",
            "A",
            "a b",
            "a_b",
            "a-b",
            "a,b",
            "a;b",
            "a.b",
            "a(1)",
            "a[1]",
            "a{1}",
            "a+b",
            "a=b",
            "a1",
            "a10",
            "a2",
            "aa",
            "aA",
            "Aa",
            "ab",
            "aB",
            "Ab",
            "AB",
            "dir/",
            "Dir/",
            "f",
            "foo_bar.ts",
            "foo-bar.ts",
            "foo.ts",
            "Foo.ts",
            "main.test.ts",
            "main.ts",
            "Main.ts",
            "my dir/",
            "readme-old.md",
            "readme.md",
            "README.md",
            "x.ts/",
            "x/",
            "zeta-2.txt",
            "Zeta.txt",
        ];
        assert_eq!(sorted(&expected), expected);
    }

    /// `["PI_AB", "PI_A_B", "PI_A1", "PI_Ab", "PI_ab"].sort((a, b) => a.localeCompare(b))` in Node
    /// (daemon/tests/worker_settings.rs).
    #[test]
    fn jsrt_locale_compare_sorts_identifiers_underscore_first() {
        assert_eq!(
            sorted(&["PI_AB", "PI_A_B", "PI_A1", "PI_Ab", "PI_ab"]),
            ["PI_A_B", "PI_A1", "PI_ab", "PI_Ab", "PI_AB"]
        );
    }

    /// Orders checked against Node 26 (ICU 78) `localeCompare`.
    #[test]
    fn jsrt_locale_compare_folds_accents_to_a_secondary_level() {
        assert_eq!(sorted(&["a", "A", "á", "Á", "b"]), ["a", "A", "á", "Á", "b"]);
        assert_eq!(sorted(&["éa", "eb", "éb", "ea"]), ["ea", "éa", "eb", "éb"]);
        assert_eq!(
            sorted(&["o", "ø", "ó", "p", "Ø", "oz"]),
            ["o", "ó", "ø", "Ø", "oz", "p"]
        );
        assert_eq!(
            sorted(&["Zürich", "Zurich", "zz", "Zuerich"]),
            ["Zuerich", "Zurich", "Zürich", "zz"]
        );
        // A decomposed accent is the precomposed one.
        assert_eq!(locale_compare("e\u{301}", "é"), Ordering::Equal);
    }

    #[test]
    fn jsrt_locale_compare_puts_ligatures_after_their_expansion() {
        assert_eq!(
            sorted(&["ss", "ß", "SS", "Ss", "sS", "st"]),
            ["ss", "sS", "Ss", "SS", "ß", "st"]
        );
        assert_eq!(
            sorted(&["ae", "æ", "Ae", "AE", "Æ", "af", "ad"]),
            ["ad", "ae", "Ae", "AE", "æ", "Æ", "af"]
        );
    }

    #[test]
    fn jsrt_locale_compare_orders_whitespace_symbols_and_currency_like_icu() {
        // Line terminators and tab before the space; other spaces equal it but for a tertiary difference.
        assert_eq!(
            sorted(&["a b", "a\tb", "a\nb", "a\u{a0}b", "a_b"]),
            ["a\tb", "a\nb", "a b", "a\u{a0}b", "a_b"]
        );
        assert_eq!(
            sorted(&["1", "$", "€", "😀", "~", "–", "-"]),
            ["-", "–", "~", "😀", "$", "€", "1"]
        );
        assert_eq!(locale_compare("²", "2"), Ordering::Greater);
        assert_eq!(locale_compare("²", "3"), Ordering::Less);
    }

    #[test]
    fn jsrt_locale_compare_ignores_format_and_control_characters() {
        assert_eq!(locale_compare("a\u{200b}b", "ab"), Ordering::Equal);
        assert_eq!(locale_compare("\u{feff}", ""), Ordering::Equal);
        assert_eq!(locale_compare("a\u{1}b", "ab"), Ordering::Equal);
        assert_eq!(locale_compare("a\u{85}b", "ab"), Ordering::Less);
    }
}
