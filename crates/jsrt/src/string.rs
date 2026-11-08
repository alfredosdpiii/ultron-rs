//! JavaScript strings: the `\s` / `trim` whitespace set and `replace(/\s+/g, " ").trim()`.

/// A character JavaScript's `\s` and `String.prototype.trim` treat as whitespace: WhiteSpace (TAB, VT, FF, U+FEFF
/// and the Zs category) and LineTerminator (LF, CR, U+2028, U+2029). Unlike Rust's `char::is_whitespace` (Unicode
/// White_Space), U+0085 is not one and U+FEFF is.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `text.trim()`.
pub fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `text.replace(/\s+/g, " ").trim()`: whitespace runs become one space, none at the ends.
pub fn flatten(text: &str) -> String {
    text.split(is_space)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsrt_string_whitespace_is_javascripts_not_unicodes() {
        assert_eq!(trim(" \u{a0}\u{feff}\u{2028}x\u{85} "), "x\u{85}");
        assert_eq!(trim("\u{85}x\u{85}"), "\u{85}x\u{85}");
        assert_eq!(trim("\u{1680}\u{2000}\u{200a}\u{202f}\u{205f}\u{3000}\u{b}\u{c}x"), "x");
        assert_eq!(trim("\u{180e}x"), "\u{180e}x");
        assert_eq!("  x \u{feff}".trim_end_matches(is_space), "  x");
        assert_eq!(flatten(" a\u{85}b\u{feff} \u{2028}c\t"), "a\u{85}b c");
        assert_eq!(flatten(" \t\n "), "");
        // Exactly Unicode White_Space without U+0085, plus U+FEFF.
        for c in (0..=0x3000u32).filter_map(char::from_u32) {
            assert_eq!(
                is_space(c),
                (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}',
                "{:x}",
                c as u32
            );
        }
    }
}
