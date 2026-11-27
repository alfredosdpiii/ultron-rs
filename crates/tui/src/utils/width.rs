use crate::tables;
use unicode_segmentation::UnicodeSegmentation;

use super::ansi::strip_terminal_sequences;

fn in_table(table: &[(u32, u32)], cp: char) -> bool {
    let cp = cp as u32;
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

pub fn is_cjk_break(c: char) -> bool {
    in_table(tables::CJK_BREAK, c)
}

pub fn is_cjk_punctuation(c: char) -> bool {
    in_table(tables::CJK_PUNCT, c)
}

/// East Asian Width as reported by get-east-asian-width (2 for wide/fullwidth, else 1).
fn east_asian_width(c: char) -> usize {
    if in_table(tables::WIDE, c) { 2 } else { 1 }
}

pub(super) fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

fn could_be_emoji(segment: &str, first: char) -> bool {
    let cp = first as u32;
    (0x1f000..=0x1fbff).contains(&cp)
        || (0x2300..=0x23ff).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
        || (0x2b50..=0x2b55).contains(&cp)
        || segment.contains('\u{FE0F}')
        || segment.encode_utf16().count() > 2
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_skin_tone(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

fn is_rgi_tag_sequence(chars: &[char]) -> bool {
    let Some(rest) = chars.strip_prefix(&['\u{1F3F4}']) else {
        return false;
    };
    rest.last() == Some(&'\u{E007F}')
        && rest[..rest.len() - 1]
            .iter()
            .all(|c| ('\u{E0020}'..='\u{E007E}').contains(c))
}

fn is_rgi_zwj_part(part: &str) -> bool {
    let p: Vec<char> = part.chars().collect();
    match p.as_slice() {
        [c] => in_table(tables::EXTENDED_PICTOGRAPHIC, *c) || in_table(tables::RGI_SINGLE, *c),
        [c, '\u{FE0F}'] => in_table(tables::EXTENDED_PICTOGRAPHIC, *c),
        [c, m] if is_skin_tone(*m) => in_table(tables::RGI_MODIFIER_BASE, *c),
        _ => false,
    }
}

fn is_rgi_zwj_sequence(segment: &str) -> bool {
    segment.split('\u{200D}').all(is_rgi_zwj_part)
}

fn is_rgi_simple_emoji(chars: &[char]) -> bool {
    match chars {
        [c] => in_table(tables::RGI_SINGLE, *c),
        [c, '\u{FE0F}'] => in_table(tables::RGI_WITH_VS16, *c),
        [c, m] if is_skin_tone(*m) => in_table(tables::RGI_MODIFIER_BASE, *c),
        [c, '\u{FE0F}', '\u{20E3}'] => matches!(c, '#' | '*' | '0'..='9'),
        [a, b] if is_regional(*a) && is_regional(*b) => true,
        _ => false,
    }
}

/// `^\p{RGI_Emoji}$`: exact for single code points, VS16 and skin-tone forms (tables generated from Node);
/// keycaps, flags, tag and ZWJ sequences are recognised structurally.
fn is_rgi_emoji(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    if chars.is_empty() {
        return false;
    }
    if is_rgi_simple_emoji(&chars) || is_rgi_tag_sequence(&chars) {
        return true;
    }
    chars.contains(&'\u{200D}') && is_rgi_zwj_sequence(segment)
}

fn trailing_grapheme_width(chars: std::str::Chars<'_>) -> usize {
    let mut width = 0;
    let mut follows_mark = false;
    for c in chars {
        if in_table(tables::TERMINAL_SPACING_MARK, c) {
            width += 1;
            follows_mark = false;
        } else if in_table(tables::MARK, c) {
            follows_mark = true;
        } else if !in_table(tables::NON_PRINTING, c) {
            if follows_mark || ('\u{FF00}'..='\u{FFEF}').contains(&c) {
                width += east_asian_width(c);
            } else if c == '\u{0E33}' || c == '\u{0EB3}' {
                width += 1;
            }
            follows_mark = false;
        }
    }
    width
}

/// Terminal width of one grapheme cluster.
pub(crate) fn grapheme_width(segment: &str) -> usize {
    if segment == "\t" {
        return 3;
    }
    let Some(first) = segment.chars().next() else {
        return 0;
    };
    if segment.chars().all(|c| in_table(tables::TERMINAL_SPACING_MARK, c)) {
        return segment.chars().count();
    }
    if segment.chars().all(|c| in_table(tables::ZERO_WIDTH, c)) {
        return 0;
    }
    if could_be_emoji(segment, first) && is_rgi_emoji(segment) {
        return 2;
    }
    let base = segment.trim_start_matches(|c| in_table(tables::NON_PRINTING, c));
    let mut chars = base.chars();
    let Some(cp) = chars.next() else {
        return 0;
    };
    if is_regional(cp) {
        return 2;
    }
    east_asian_width(cp) + trailing_grapheme_width(chars)
}

/// Visible width of a string in terminal columns (tabs count 3, escape sequences 0).
pub fn visible_width(s: &str) -> usize {
    if is_printable_ascii(s) {
        return s.len();
    }
    let clean = if s.contains('\t') {
        strip_terminal_sequences(&s.replace('\t', "   "))
    } else {
        strip_terminal_sequences(s)
    };
    if is_printable_ascii(&clean) {
        return clean.len();
    }
    clean.graphemes(true).map(grapheme_width).sum()
}
