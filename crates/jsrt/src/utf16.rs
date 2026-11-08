//! JavaScript string indexing: `length`, `slice` and the default `sort()` order count UTF-16 code units.
//!
//! A JavaScript slice can cut a surrogate pair in half and keep the lone half. Rust strings cannot hold one, so
//! [`head`], [`skip`] and [`tail`] leave the whole pair out (the result is one unit shorter), and [`head_lossy`]
//! keeps the unit count with U+FFFD in its place (what Node writes for a lone surrogate on a UTF-8 stream).

use std::cmp::Ordering;

/// `text.length`.
pub fn len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.slice(0, end)`; a cut inside a surrogate pair leaves the pair out.
pub fn head(text: &str, end: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        units += c.len_utf16();
        if units > end {
            return &text[..index];
        }
    }
    text
}

/// `text.slice(0, end)` with a cut surrogate pair written as U+FFFD, so the result is exactly `end` units (or the
/// whole text).
pub fn head_lossy(text: &str, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().take(end).collect();
    String::from_utf16_lossy(&units)
}

/// `text.slice(start)`; a cut inside a surrogate pair leaves the pair out.
pub fn skip(text: &str, start: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        if units >= start {
            return &text[index..];
        }
        units += c.len_utf16();
    }
    ""
}

/// The last `count` units: `text.slice(-count)` for `count > 0` (the whole text when `count` is at least its
/// length), and `""` for 0 (where `slice(-0)` would be the whole text); a cut inside a surrogate pair leaves the pair
/// out.
pub fn tail(text: &str, count: usize) -> &str {
    let total = len(text);
    if count == 0 {
        return "";
    }
    if total <= count {
        return text;
    }
    skip(text, total - count)
}

/// `Array.prototype.sort()` order without a comparator: UTF-16 code units, so U+FF61 sorts after U+1F600 (unlike
/// Rust's `str` order, which compares code points).
pub fn cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `items.sort()` with no comparator.
pub fn sort<S: AsRef<str>>(items: &mut [S]) {
    items.sort_by(|a, b| cmp(a.as_ref(), b.as_ref()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsrt_utf16_lengths_and_slices_count_code_units() {
        assert_eq!(len("é\u{1F600}"), 3);
        assert_eq!(head("a\u{1F600}b", 1), "a");
        assert_eq!(head("a\u{1F600}b", 2), "a");
        assert_eq!(head("a\u{1F600}b", 3), "a\u{1F600}");
        assert_eq!(head("abc", 10), "abc");
        assert_eq!(head_lossy("a\u{1F600}b", 2), "a\u{FFFD}");
        assert_eq!(head_lossy("a\u{1F600}b", 3), "a\u{1F600}");
        assert_eq!(skip("a\u{1F600}b", 1), "\u{1F600}b");
        assert_eq!(skip("a\u{1F600}b", 2), "b");
        assert_eq!(skip("ab", 5), "");
        assert_eq!(tail("a\u{1F600}b", 1), "b");
        assert_eq!(tail("a\u{1F600}b", 2), "b");
        assert_eq!(tail("a\u{1F600}b", 3), "\u{1F600}b");
        assert_eq!(tail("ab", 5), "ab");
        assert_eq!(tail("ab", 0), "");
    }

    #[test]
    fn jsrt_utf16_sort_order_is_code_units_not_code_points() {
        assert_eq!(cmp("\u{FF61}", "\u{1F600}"), Ordering::Greater);
        assert_eq!("\u{FF61}".cmp("\u{1F600}"), Ordering::Less);
        assert_eq!(cmp("B", "a"), Ordering::Less);
        assert_eq!(cmp("a", "ab"), Ordering::Less);
        let mut items = vec!["b", "B", "a", "\u{ffff}", "\u{1F600}", "_"];
        sort(&mut items);
        assert_eq!(items, ["B", "_", "a", "b", "\u{1F600}", "\u{ffff}"]);
    }
}
