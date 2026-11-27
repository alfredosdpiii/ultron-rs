use icu_collections::char16trie::{Char16Trie, TrieResult};
use icu_normalizer::ComposingNormalizerBorrowed;
use icu_properties::CodePointMapData;
use icu_properties::props::{CanonicalCombiningClass, GeneralCategory, GeneralCategoryGroup, LineBreak, Script};
use icu_provider::prelude::*;
use icu_segmenter::provider::{Baked, SegmenterDictionaryAutoV1, SegmenterDictionaryExtendedV1};

use super::rules::class;

pub(super) fn script(c: char) -> Script {
    CodePointMapData::<Script>::new().get(c)
}

pub(super) fn is_sa(c: char) -> bool {
    CodePointMapData::<LineBreak>::new().get(c) == LineBreak::ComplexContext
}

fn is_mark(c: char) -> bool {
    GeneralCategoryGroup::Mark.contains(CodePointMapData::<GeneralCategory>::new().get(c))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Engine {
    Cj,
    Thai,
    Lao,
    Burmese,
    Khmer,
}

impl Engine {
    fn for_char(c: char) -> Option<Engine> {
        [Engine::Cj, Engine::Thai, Engine::Lao, Engine::Burmese, Engine::Khmer]
            .into_iter()
            .find(|e| e.contains(c))
    }

    /// The engine's character set (`setCharacters`).
    fn contains(self, c: char) -> bool {
        match self {
            Engine::Cj => {
                matches!(script(c), Script::Han | Script::Hiragana | Script::Katakana)
                    || matches!(c, '\u{30FC}' | '\u{FF70}' | '\u{FF9E}' | '\u{FF9F}')
            }
            Engine::Thai => script(c) == Script::Thai && is_sa(c),
            Engine::Lao => script(c) == Script::Lao && is_sa(c),
            Engine::Burmese => script(c) == Script::Myanmar && is_sa(c),
            Engine::Khmer => script(c) == Script::Khmer && is_sa(c),
        }
    }

    fn dictionary(self) -> &'static str {
        match self {
            Engine::Cj => "cjdict",
            Engine::Thai => "thaidict",
            Engine::Lao => "laodict",
            Engine::Burmese => "burmesedict",
            Engine::Khmer => "khmerdict",
        }
    }
}

/// A rule `$dictionary` character: complex context (line break SA) or Han/Hiragana/Katakana.
fn is_dictionary_char(c: char) -> bool {
    let k = class(c);
    is_sa(c) || k.kana_kanji || k.hangul
}

fn with_trie<R>(engine: Engine, f: impl FnOnce(&Char16Trie<'_>) -> R) -> R {
    let attrs = DataMarkerAttributes::from_str_or_panic(engine.dictionary());
    let req = DataRequest {
        id: DataIdentifierBorrowed::for_marker_attributes(attrs),
        ..Default::default()
    };
    if engine == Engine::Cj {
        let resp: DataResponse<SegmenterDictionaryAutoV1> = Baked.load(req).expect("cjdict");
        f(&Char16Trie::new(resp.payload.get().trie_data.clone()))
    } else {
        let resp: DataResponse<SegmenterDictionaryExtendedV1> = Baked.load(req).expect("dict");
        f(&Char16Trie::new(resp.payload.get().trie_data.clone()))
    }
}

/// `DictionaryMatcher::matches` from `text[start..]`: (code-point length, value) of each dictionary word that
/// starts there (at most `limit`), stopping once `max_length` code units are matched; plus the code points looked at.
fn matches(
    trie: &Char16Trie<'_>,
    text: &[char],
    start: usize,
    max_length: usize,
    limit: usize,
) -> (Vec<(usize, usize, i32)>, usize) {
    let mut iter = trie.iter();
    let mut words = Vec::new();
    let (mut units, mut points) = (0, 0);
    for &c in &text[start..] {
        let result = iter.next32(u32::from(c));
        units += c.len_utf16();
        points += 1;
        match result {
            TrieResult::Intermediate(v) | TrieResult::FinalValue(v) => {
                if words.len() < limit {
                    words.push((points, units, v));
                }
                if matches!(result, TrieResult::FinalValue(_)) {
                    break;
                }
            }
            TrieResult::NoMatch => break,
            TrieResult::NoValue => {}
        }
        if units >= max_length {
            break;
        }
    }
    (words, points)
}

fn is_katakana(c: char) -> bool {
    let c = u32::from(c);
    (0x30A1..=0x30FE).contains(&c) && c != 0x30FB || (0xFF66..=0xFF9F).contains(&c)
}

fn katakana_cost(length: usize) -> u32 {
    const COST: [u32; 9] = [8192, 984, 408, 240, 204, 252, 300, 372, 480];
    COST.get(length).copied().unwrap_or(8192)
}

/// NFKC `range` in fragments that start at a normalization boundary: the normalized code points, each mapped to the
/// index (in `chars`) of its fragment's start, plus the range end.
fn normalize(chars: &[char], start: usize, end: usize) -> (Vec<char>, Vec<usize>) {
    let nfkc = ComposingNormalizerBorrowed::new_nfkc();
    let range: String = chars[start..end].iter().collect();
    if nfkc.is_normalized(&range) {
        return (chars[start..end].to_vec(), (start..=end).collect());
    }
    let (mut text, mut map) = (Vec::new(), Vec::new());
    let mut i = start;
    while i < end {
        let fragment_start = i;
        let mut fragment = String::new();
        fragment.push(chars[i]);
        i += 1;
        while i < end && !has_boundary_before(chars[i]) {
            fragment.push(chars[i]);
            i += 1;
        }
        for c in nfkc.normalize(&fragment).chars() {
            text.push(c);
            map.push(fragment_start);
        }
    }
    map.push(end);
    (text, map)
}

/// `Normalizer2::hasBoundaryBefore` for NFKC, for the characters of a CJ range: `c` normalizes to a starter.
/// (The backward-combining starters, Hangul vowels and a few Indic vowel signs, never occur in a CJ range.)
fn has_boundary_before(c: char) -> bool {
    let nfkc = ComposingNormalizerBorrowed::new_nfkc();
    let mut buf = [0u8; 4];
    nfkc.normalize(c.encode_utf8(&mut buf))
        .chars()
        .next()
        .is_none_or(|first| {
            CodePointMapData::<CanonicalCombiningClass>::new().get(first) == CanonicalCombiningClass::NotReordered
        })
}

fn relax_cj_word_edges(trie: &Char16Trie<'_>, text: &[char], i: usize, best: &mut [u32], prev: &mut [usize]) {
    let (words, _) = matches(trie, text, i, 20, text.len());
    let mut words: Vec<(usize, u32)> = words.into_iter().map(|(cp, _, v)| (cp, v as u32)).collect();
    if (words.is_empty() || words[0].0 != 1) && !('\u{AC00}'..='\u{D7A3}').contains(&text[i]) {
        words.push((1, 255));
    }
    for (length, value) in words {
        let snlp = best[i] + value;
        if snlp < best[i + length] {
            best[i + length] = snlp;
            prev[i + length] = i;
        }
    }
}

fn relax_cj_katakana(text: &[char], i: usize, prev_katakana: bool, best: &mut [u32], prev: &mut [usize]) -> bool {
    let katakana = is_katakana(text[i]);
    if !prev_katakana && katakana {
        let n = text.len();
        let mut run = 1;
        while i + run < n && run < 20 && is_katakana(text[i + run]) {
            run += 1;
        }
        if run < 20 {
            let snlp = best[i] + katakana_cost(run);
            if snlp < best[i + run] {
                best[i + run] = snlp;
                prev[i + run] = i;
            }
        }
    }
    katakana
}

fn collect_cj_breaks(best: &[u32], prev: &[usize], map: &[usize], start: usize, end: usize, breaks: &mut Vec<usize>) {
    let n = prev.len() - 1;
    let mut tentative = Vec::new();
    if best[n] == u32::MAX {
        tentative.push(n);
    } else {
        let mut i = n;
        while i > 0 {
            tentative.push(i);
            i = prev[i];
        }
    }
    if breaks.last().is_none_or(|&b| b < start) {
        tentative.push(0);
    }
    let mut previous: Option<usize> = None;
    for &cp in tentative.iter().rev() {
        let pos = map[cp];
        if previous.is_none_or(|p| pos > p) && pos != start {
            breaks.push(pos);
        }
        previous = Some(pos);
    }
    if breaks.last() == Some(&end) {
        breaks.pop();
    }
}

/// `CjkBreakEngine::divideUpDictionaryRange`: the best path over dictionary word costs.
fn divide_cj(trie: &Char16Trie<'_>, chars: &[char], start: usize, end: usize, breaks: &mut Vec<usize>) {
    if start >= end {
        return;
    }
    let (text, map) = normalize(chars, start, end);
    let n = text.len();
    let mut best = vec![u32::MAX; n + 1];
    best[0] = 0;
    let mut prev = vec![usize::MAX; n + 1];
    let mut prev_katakana = false;
    for i in 0..n {
        if best[i] == u32::MAX {
            continue;
        }
        relax_cj_word_edges(trie, &text, i, &mut best, &mut prev);
        prev_katakana = relax_cj_katakana(&text, i, prev_katakana, &mut best, &mut prev);
    }
    collect_cj_breaks(&best, &prev, &map, start, end, breaks);
}

#[derive(Default, Clone)]
struct PossibleWord {
    count: usize,
    prefix: usize,
    offset: Option<usize>,
    mark: usize,
    current: usize,
    lengths: Vec<usize>,
}

impl PossibleWord {
    fn candidates(&mut self, trie: &Char16Trie<'_>, text: &[char], pos: &mut usize, end: usize) -> usize {
        let start = *pos;
        if self.offset != Some(start) {
            self.offset = Some(start);
            let (words, prefix) = matches(trie, text, start, end - start, 20);
            self.lengths = words.into_iter().map(|(_, cu, _)| cu).collect();
            self.count = self.lengths.len();
            self.prefix = prefix;
        }
        if self.count > 0 {
            *pos = start + self.lengths[self.count - 1];
        }
        self.current = self.count.wrapping_sub(1);
        self.mark = self.current;
        self.count
    }

    fn accept_marked(&self, pos: &mut usize) -> usize {
        *pos = self.offset.unwrap_or(0) + self.lengths[self.mark];
        self.lengths[self.mark]
    }

    fn back_up(&mut self, pos: &mut usize) -> bool {
        if self.current > 0 && self.current != usize::MAX {
            self.current -= 1;
            *pos = self.offset.unwrap_or(0) + self.lengths[self.current];
            return true;
        }
        false
    }

    fn mark_current(&mut self) {
        self.mark = self.current;
    }
}

struct SaRules {
    root_combine: usize,
    prefix_combine: usize,
    suffixes: bool,
    begin: fn(char) -> bool,
    end: fn(char) -> bool,
}

fn thai_begin(c: char) -> bool {
    ('\u{0E01}'..='\u{0E2E}').contains(&c) || ('\u{0E40}'..='\u{0E44}').contains(&c)
}

fn thai_end(c: char) -> bool {
    Engine::Thai.contains(c) && c != '\u{0E31}' && !('\u{0E40}'..='\u{0E44}').contains(&c)
}

fn lao_begin(c: char) -> bool {
    ('\u{0E81}'..='\u{0EAE}').contains(&c)
        || ('\u{0EDC}'..='\u{0EDD}').contains(&c)
        || ('\u{0EC0}'..='\u{0EC4}').contains(&c)
}

fn lao_end(c: char) -> bool {
    Engine::Lao.contains(c) && !('\u{0EC0}'..='\u{0EC4}').contains(&c)
}

fn burmese_begin(c: char) -> bool {
    ('\u{1000}'..='\u{102A}').contains(&c)
}

fn burmese_end(c: char) -> bool {
    Engine::Burmese.contains(c)
}

fn khmer_begin(c: char) -> bool {
    ('\u{1780}'..='\u{17B3}').contains(&c)
}

fn khmer_end(c: char) -> bool {
    Engine::Khmer.contains(c) && c != '\u{17D2}'
}

fn sa_rules(engine: Engine) -> SaRules {
    match engine {
        Engine::Thai => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: true,
            begin: thai_begin,
            end: thai_end,
        },
        Engine::Lao => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: false,
            begin: lao_begin,
            end: lao_end,
        },
        Engine::Burmese => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: false,
            begin: burmese_begin,
            end: burmese_end,
        },
        _ => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: false,
            begin: khmer_begin,
            end: khmer_end,
        },
    }
}

struct SaCursor<'a> {
    trie: &'a Char16Trie<'a>,
    text: &'a [char],
    words: [PossibleWord; 3],
    found: usize,
    pos: usize,
    range_end: usize,
}

impl<'a> SaCursor<'a> {
    fn new(trie: &'a Char16Trie<'a>, text: &'a [char], range_start: usize, range_end: usize) -> Self {
        Self {
            trie,
            text,
            words: Default::default(),
            found: 0,
            pos: range_start,
            range_end,
        }
    }

    fn try_third_word(&mut self) -> bool {
        let f = self.found;
        loop {
            if self.words[(f + 2) % 3].candidates(self.trie, self.text, &mut self.pos, self.range_end) > 0 {
                self.words[f % 3].mark_current();
                return true;
            }
            if !self.words[(f + 1) % 3].back_up(&mut self.pos) {
                return false;
            }
        }
    }

    fn find_best_lookahead(&mut self) {
        if self.pos >= self.range_end {
            return;
        }
        let f = self.found;
        loop {
            if self.words[(f + 1) % 3].candidates(self.trie, self.text, &mut self.pos, self.range_end) > 0 {
                self.words[f % 3].mark_current();
                if self.pos >= self.range_end || self.try_third_word() {
                    return;
                }
            }
            if !self.words[f % 3].back_up(&mut self.pos) {
                return;
            }
        }
    }

    fn match_word(&mut self) -> usize {
        let candidates = self.words[self.found % 3].candidates(self.trie, self.text, &mut self.pos, self.range_end);
        if candidates == 0 {
            return 0;
        }
        if candidates > 1 {
            self.find_best_lookahead();
        }
        let len = self.words[self.found % 3].accept_marked(&mut self.pos);
        self.found += 1;
        len
    }

    fn scan_unknown_span(&mut self, rules: &SaRules, base: usize) -> usize {
        let mut remaining = self.range_end as isize - base as isize;
        let mut chars = 0;
        loop {
            let pc = self.text.get(self.pos).copied();
            self.pos += 1;
            chars += 1;
            remaining -= 1;
            if remaining <= 0 {
                break;
            }
            let uc = self.text.get(self.pos).copied();
            if pc.is_some_and(rules.end) && uc.is_some_and(rules.begin) {
                let n =
                    self.words[(self.found + 1) % 3].candidates(self.trie, self.text, &mut self.pos, self.range_end);
                self.pos = base + chars;
                if n > 0 {
                    break;
                }
            }
        }
        chars
    }

    fn recover_unknown(&mut self, rules: &SaRules, current: usize, cp_length: usize, cu_length: &mut usize) {
        if self.pos >= self.range_end || cp_length >= rules.root_combine {
            return;
        }
        let f = self.found;
        if self.words[f % 3].candidates(self.trie, self.text, &mut self.pos, self.range_end) == 0
            && (*cu_length == 0 || self.words[f % 3].prefix < rules.prefix_combine)
        {
            let chars = self.scan_unknown_span(rules, current + *cu_length);
            if *cu_length == 0 {
                self.found += 1;
            }
            *cu_length += chars;
        } else {
            self.pos = current + *cu_length;
        }
    }

    fn consume_marks(&mut self, engine: Engine, cu_length: &mut usize) {
        while self.pos < self.range_end && self.text.get(self.pos).copied().is_some_and(|c| is_sa_mark(engine, c)) {
            self.pos += 1;
            *cu_length += 1;
        }
    }

    fn consume_thai_suffixes(&mut self, current: usize, cu_length: &mut usize) {
        const PAIYANNOI: char = '\u{0E2F}';
        const MAIYAMOK: char = '\u{0E46}';
        let mut uc = self.text.get(self.pos).copied();
        if self.words[self.found % 3].candidates(self.trie, self.text, &mut self.pos, self.range_end) != 0
            || !is_thai_suffix(uc)
        {
            self.pos = current + *cu_length;
            return;
        }
        if uc == Some(PAIYANNOI) && !is_thai_suffix(self.text.get(self.pos - 1).copied()) {
            self.pos += 1;
            *cu_length += 1;
            uc = self.text.get(self.pos).copied();
        }
        if uc == Some(MAIYAMOK) && self.text.get(self.pos - 1).copied() != Some(MAIYAMOK) {
            self.pos += 1;
            *cu_length += 1;
        }
    }
}

fn is_sa_mark(engine: Engine, c: char) -> bool {
    c == ' ' || (engine.contains(c) && is_mark(c))
}

fn is_thai_suffix(c: Option<char>) -> bool {
    matches!(c, Some('\u{0E2F}' | '\u{0E46}'))
}

/// `ThaiBreakEngine` (and the Lao, Burmese and Khmer engines) `divideUpDictionaryRange`.
fn divide_sa(
    engine: Engine,
    trie: &Char16Trie<'_>,
    text: &[char],
    range_start: usize,
    range_end: usize,
    breaks: &mut Vec<usize>,
) {
    let too_short = if engine == Engine::Thai {
        range_start + 4 >= range_end
    } else {
        range_end - range_start < 4
    };
    if too_short {
        return;
    }
    let rules = sa_rules(engine);
    let mut cursor = SaCursor::new(trie, text, range_start, range_end);
    while cursor.pos < range_end {
        let current = cursor.pos;
        let mut cu_length = cursor.match_word();
        let cp_length = cu_length;
        cursor.recover_unknown(&rules, current, cp_length, &mut cu_length);
        cursor.consume_marks(engine, &mut cu_length);
        if rules.suffixes && cursor.pos < range_end && cu_length > 0 {
            cursor.consume_thai_suffixes(current, &mut cu_length);
        }
        if cu_length > 0 {
            breaks.push(current + cu_length);
        }
    }
    if breaks.last().is_some_and(|&b| b >= range_end) {
        breaks.pop();
    }
}

fn populate_next_run(chars: &[char], i: usize, end: usize, breaks: &mut Vec<usize>) -> usize {
    let Some(engine) = Engine::for_char(chars[i]) else {
        let s = script(chars[i]);
        let mut next = i;
        while next < end && script(chars[next]) == s {
            next += 1;
        }
        return next;
    };
    let mut j = i;
    while j < end && engine.contains(chars[j]) {
        j += 1;
    }
    with_trie(engine, |trie| {
        if engine == Engine::Cj {
            divide_cj(trie, chars, i, j, breaks);
        } else {
            divide_sa(engine, trie, chars, i, j, breaks);
        }
    });
    j
}

/// `DictionaryCache::populateDictionary` over the rule segment `chars[start..end]`: its subdivision boundaries
/// (including `start` and `end`), or None when the dictionaries found no break.
pub(super) fn populate(chars: &[char], start: usize, end: usize) -> Option<Vec<usize>> {
    let units: usize = chars[start..end].iter().map(|c| c.len_utf16()).sum();
    if units <= 1 {
        return None;
    }
    let mut breaks = Vec::new();
    let mut i = start;
    while i < end {
        while i < end && !is_dictionary_char(chars[i]) {
            i += 1;
        }
        if i >= end {
            break;
        }
        i = populate_next_run(chars, i, end, &mut breaks);
    }
    if breaks.is_empty() {
        return None;
    }
    if start < breaks[0] {
        breaks.insert(0, start);
    }
    if breaks.last().is_some_and(|&last| end > last) {
        breaks.push(end);
    }
    Some(breaks)
}
