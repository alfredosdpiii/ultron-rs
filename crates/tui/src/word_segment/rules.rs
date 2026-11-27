use icu_properties::props::{ExtendedPictographic, GraphemeClusterBreak, Ideographic, Script, WordBreak};
use icu_properties::{CodePointMapData, CodePointSetData};

use super::dict::{is_sa, script};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Class {
    pub(super) wb: WordBreak,
    /// `$ALetterPlus | $Hebrew_Letter`.
    pub(super) ah: bool,
    pub(super) hebrew: bool,
    pub(super) kana_kanji: bool,
    pub(super) hangul: bool,
    pub(super) ex_fm: bool,
    pub(super) ext_pict: bool,
    /// The status of the single-character rules (`$IdeographicEx`, `$HiraganaEx`, ...): (with trailing ExFm, alone).
    pub(super) single: (u16, u16),
}

fn is_ah_class(c: char, wb: WordBreak, kana_kanji: bool, hangul: bool) -> bool {
    let control = CodePointMapData::<GraphemeClusterBreak>::new().get(c) == GraphemeClusterBreak::Control;
    wb == WordBreak::HebrewLetter
        || (wb == WordBreak::ALetter && !kana_kanji && !hangul)
        || (is_sa(c) && wb != WordBreak::Extend && !control)
}

fn single_status(c: char, wb: WordBreak, sc: Script, ah: bool, hangul: bool) -> (u16, u16) {
    let ideographic = CodePointSetData::new::<Ideographic>().contains(c);
    let single_ex = if ideographic || sc == Script::Hiragana || wb == WordBreak::Katakana {
        400
    } else if ah {
        200
    } else if wb == WordBreak::Numeric {
        100
    } else {
        0
    };
    (single_ex, if hangul { 200 } else { single_ex })
}

pub(super) fn class(c: char) -> Class {
    let wb = CodePointMapData::<WordBreak>::new().get(c);
    let sc = script(c);
    let hangul = ('\u{AC00}'..='\u{D7A3}').contains(&c);
    let kana_kanji = sc == Script::Han || sc == Script::Hiragana || wb == WordBreak::Katakana;
    let ex_fm = matches!(wb, WordBreak::Extend | WordBreak::Format | WordBreak::ZWJ);
    let ah = is_ah_class(c, wb, kana_kanji, hangul);
    Class {
        wb,
        ah,
        hebrew: wb == WordBreak::HebrewLetter,
        kana_kanji,
        hangul,
        ex_fm,
        ext_pict: CodePointSetData::new::<ExtendedPictographic>().contains(c),
        single: single_status(c, wb, sc, ah, hangul),
    }
}

fn is_mid_letter(k: &Class) -> bool {
    k.wb == WordBreak::MidLetter || matches!(k.wb, WordBreak::MidNumLet | WordBreak::SingleQuote)
}

fn is_mid_num(k: &Class) -> bool {
    matches!(k.wb, WordBreak::MidNum | WordBreak::MidNumLet | WordBreak::SingleQuote)
}

fn is_num(x: &Class) -> bool {
    x.wb == WordBreak::Numeric
}

fn join_direct(ka: &Class, kb: &Class, direct: bool) -> Option<u16> {
    if !direct {
        return None;
    }
    if (kb.ext_pict && ka.wb == WordBreak::ZWJ) || (ka.wb == WordBreak::WSegSpace && kb.wb == WordBreak::WSegSpace) {
        return Some(0);
    }
    if ka.hangul && kb.hangul {
        return Some(200);
    }
    (ka.kana_kanji && kb.kana_kanji).then_some(400)
}

fn join_aletter(k: &[Class], a: usize, b: usize, before: Option<usize>, after: Option<usize>) -> Option<u16> {
    let (ka, kb) = (&k[a], &k[b]);
    if ka.ah && kb.ah {
        return Some(200);
    }
    if ka.ah && is_mid_letter(kb) && after.is_some_and(|c| k[c].ah) {
        return Some(200);
    }
    if is_mid_letter(ka) && kb.ah && before.is_some_and(|c| k[c].ah) {
        return Some(200);
    }
    None
}

fn join_hebrew(k: &[Class], a: usize, b: usize, before: Option<usize>, after: Option<usize>) -> Option<u16> {
    let (ka, kb) = (&k[a], &k[b]);
    if ka.hebrew && kb.wb == WordBreak::SingleQuote {
        return Some(200);
    }
    if ka.hebrew && kb.wb == WordBreak::DoubleQuote && after.is_some_and(|c| k[c].hebrew) {
        return Some(200);
    }
    if ka.wb == WordBreak::DoubleQuote && kb.hebrew && before.is_some_and(|c| k[c].hebrew) {
        return Some(200);
    }
    None
}

fn join_mid_numeric(k: &[Class], a: usize, b: usize, before: Option<usize>, after: Option<usize>) -> Option<u16> {
    let (ka, kb) = (&k[a], &k[b]);
    if is_num(ka) && is_mid_num(kb) && after.is_some_and(|c| is_num(&k[c])) {
        return Some(100);
    }
    if is_mid_num(ka) && is_num(kb) && before.is_some_and(|c| is_num(&k[c])) {
        return Some(100);
    }
    None
}

fn join_numeric(k: &[Class], a: usize, b: usize, before: Option<usize>, after: Option<usize>) -> Option<u16> {
    let (ka, kb) = (&k[a], &k[b]);
    if is_num(ka) && is_num(kb) {
        return Some(100);
    }
    if (ka.ah && is_num(kb)) || (is_num(ka) && kb.ah) {
        return Some(200);
    }
    join_mid_numeric(k, a, b, before, after)
}

fn enl_partner_status(other: &Class) -> Option<u16> {
    if other.ah {
        Some(200)
    } else if is_num(other) {
        Some(100)
    } else if other.wb == WordBreak::Katakana {
        Some(400)
    } else {
        None
    }
}

fn join_kata_enl_ri(ka: &Class, kb: &Class, ri_odd: bool) -> Option<u16> {
    if ka.wb == WordBreak::Katakana && kb.wb == WordBreak::Katakana {
        return Some(400);
    }
    if kb.wb == WordBreak::ExtendNumLet {
        if ka.wb == WordBreak::ExtendNumLet {
            return Some(200);
        }
        if let Some(status) = enl_partner_status(ka) {
            return Some(status);
        }
    }
    if ka.wb == WordBreak::ExtendNumLet
        && let Some(status) = enl_partner_status(kb)
    {
        return Some(status);
    }
    (ri_odd && ka.wb == WordBreak::RegionalIndicator && kb.wb == WordBreak::RegionalIndicator).then_some(0)
}

/// The status of the rule joining base `a` to base `b` across ExFm (`direct`: nothing between them), with the
/// bases around them (`before` a, `after` b) for the MidLetter/MidNum rules; None when no rule joins them.
fn join(k: &[Class], a: usize, b: usize, before: Option<usize>, after: Option<usize>, ri_odd: bool) -> Option<u16> {
    let (ka, kb) = (&k[a], &k[b]);
    let direct = b == a + 1;
    join_direct(ka, kb, direct)
        .filter(|_| kb.ext_pict && ka.wb == WordBreak::ZWJ || ka.wb == WordBreak::WSegSpace)
        .or_else(|| join_aletter(k, a, b, before, after))
        .or_else(|| join_hebrew(k, a, b, before, after))
        .or_else(|| join_numeric(k, a, b, before, after))
        .or_else(|| join_kata_enl_ri(ka, kb, ri_odd))
        .or_else(|| join_direct(ka, kb, direct))
}

fn is_newline_class(c: &Class) -> bool {
    matches!(c.wb, WordBreak::CR | WordBreak::LF | WordBreak::Newline)
}

fn consume_newline_segment(k: &[Class], start: usize) -> usize {
    let mut i = start + 1;
    if k[start].wb == WordBreak::CR && i < k.len() && k[i].wb == WordBreak::LF {
        i += 1;
    }
    i
}

fn consume_leading_ex_fm(k: &[Class], start: usize) -> (usize, bool) {
    let n = k.len();
    let mut i = start;
    while i < n && k[i].ex_fm && !(i > start && k[i - 1].wb == WordBreak::ZWJ && k[i].ext_pict) {
        i += 1;
    }
    let zwj_pict = i < n && i > start && k[i - 1].wb == WordBreak::ZWJ && k[i].ext_pict;
    (i, zwj_pict)
}

fn next_join_step(
    k: &[Class],
    base: usize,
    next: usize,
    prev_base: Option<usize>,
    ri_count: usize,
    trailing: usize,
) -> Option<u16> {
    let n = k.len();
    let mut after = next + 1;
    while after < n && k[after].ex_fm {
        after += 1;
    }
    let after = (after < n).then_some(after);
    if trailing > 0 && k[next - 1].wb == WordBreak::ZWJ && k[next].ext_pict {
        join(k, next - 1, next, None, None, false)
    } else {
        join(k, base, next, prev_base, after, ri_count % 2 == 1)
    }
}

fn advance_segment_body(k: &[Class], mut base: usize, init_joined: Option<u16>) -> (usize, u16) {
    let n = k.len();
    let mut prev_base: Option<usize> = None;
    let mut ri_count = usize::from(k[base].wb == WordBreak::RegionalIndicator);
    let mut joined = init_joined;
    let mut i = base + 1;
    let mut trailing = 0;
    loop {
        while i < n && k[i].ex_fm && !(k[i - 1].wb == WordBreak::ZWJ && k[i].ext_pict) {
            i += 1;
            trailing += 1;
        }
        if i >= n || is_newline_class(&k[i]) {
            break;
        }
        let Some(status) = next_join_step(k, base, i, prev_base, ri_count, trailing) else {
            break;
        };
        prev_base = Some(base);
        base = i;
        ri_count = if k[i].wb == WordBreak::RegionalIndicator {
            ri_count + 1
        } else {
            0
        };
        joined = Some(status);
        trailing = 0;
        i += 1;
    }
    let status = if trailing > 0 {
        k[base].single.0
    } else {
        k[base].single.1.max(joined.unwrap_or(0))
    };
    (i, status)
}

/// ICU's word rules (word.txt): the rule segments of `chars` as (start, end, status).
pub(super) fn rule_segments(chars: &[char]) -> Vec<(usize, usize, u16)> {
    let k: Vec<Class> = chars.iter().map(|&c| class(c)).collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        if is_newline_class(&k[start]) {
            let end = consume_newline_segment(&k, start);
            out.push((start, end, 0));
            start = end;
            continue;
        }
        let (base, init_joined) = if k[start].ex_fm {
            let (i, zwj_pict) = consume_leading_ex_fm(&k, start);
            if !zwj_pict {
                out.push((start, i, 0));
                start = i;
                continue;
            }
            (i, Some(0))
        } else {
            (start, None)
        };
        let (end, status) = advance_segment_body(&k, base, init_joined);
        out.push((start, end, status));
        start = end;
    }
    out
}
