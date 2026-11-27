use crate::markdown::Token;

use super::{Caps, Lexer, contains, exec, from, group, rules, test, work};

/// One maximal run of `*` or `_` in a masked inline source, with the emStrongRDelim branch (capture group 1-6) that
/// matches it. Each branch consumes the character before the run, which no earlier match can have consumed (a match
/// ends at a run's end or just before a run's preceding character), so a run's branch depends only on its two
/// neighbours and not on where the search started. That lets one pass classify every run of a paragraph instead of
/// re-running the regex over the rest of the paragraph at each delimiter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Run {
    pub start: usize,
    pub len: usize,
    pub class: Option<u8>,
}

/// The `*` and `_` runs of one inline source, computed on first use.
#[derive(Default)]
pub(super) struct DelimiterRuns {
    ast: Option<RunTable>,
    und: Option<RunTable>,
}

impl DelimiterRuns {
    fn get(&mut self, masked: &str, delim: u8) -> &mut RunTable {
        let slot = if delim == b'*' { &mut self.ast } else { &mut self.und };
        slot.get_or_insert_with(|| RunTable::new(delimiter_runs(masked, delim)))
    }
}

/// A source's runs with what lets the emStrong scan find an opener's closing run without walking the runs between:
/// `opener_len[i]` sums the lengths of opener-only runs (branches 3 and 4) before run `i`, and `searches` hold the
/// prefix sums and trees of [`CloserSearch`].
pub(super) struct RunTable {
    runs: Vec<Run>,
    opener_len: Vec<usize>,
    /// The closing searches per (opener length mod 3, previous character is the delimiter), built on first use.
    searches: [Option<CloserSearch>; 6],
}

/// Where a closing search from run `first` ends, for one (opener length mod 3, same previous character) variant,
/// without walking the runs in between.
struct CloserSearch {
    /// Sum of the lengths of subtracting runs before run `i`.
    sub: Vec<isize>,
    /// Sum of the lengths of `skip` runs before run `i`.
    skip: Vec<isize>,
    /// The first `stop` run at or after `i` (`runs.len()` when none).
    next_stop: Vec<usize>,
    /// Minimum segment tree over `q` (`isize::MAX` for runs that do not subtract), leaves from `size`.
    tree: Vec<isize>,
    size: usize,
}

fn closer_run_kind(class: Option<u8>, l_mod: usize, r_len: isize, same_prev: bool) -> u8 {
    match class {
        Some(5 | 6) if l_mod != 0 && (l_mod as isize + r_len) % 3 == 0 => 2,
        Some(5 | 6) if same_prev => 3,
        Some(1 | 2 | 5 | 6) => 1,
        _ => 0,
    }
}

fn build_closer_next_stop(kinds: &[u8]) -> Vec<usize> {
    let n = kinds.len();
    let mut next_stop = vec![n; n + 1];
    for i in (0..n).rev() {
        next_stop[i] = if kinds[i] == 3 { i } else { next_stop[i + 1] };
    }
    next_stop
}

fn build_closer_min_tree(tree: &mut [isize], size: usize) {
    for i in (1..size).rev() {
        tree[i] = tree[2 * i].min(tree[2 * i + 1]);
    }
}

impl CloserSearch {
    fn new(table: &RunTable, l_mod: usize, same_prev: bool) -> Self {
        let runs = &table.runs;
        let n = runs.len();
        let (mut sub, mut skip) = (vec![0isize; n + 1], vec![0isize; n + 1]);
        let size = n.max(1).next_power_of_two();
        let mut tree = vec![isize::MAX; 2 * size];
        let mut kinds = vec![0u8; n];
        for (i, run) in runs.iter().enumerate() {
            let r_len = run.len as isize;
            kinds[i] = closer_run_kind(run.class, l_mod, r_len, same_prev);
            sub[i + 1] = sub[i] + if kinds[i] == 1 { r_len } else { 0 };
            skip[i + 1] = skip[i] + if kinds[i] == 2 { r_len } else { 0 };
            if kinds[i] == 1 {
                tree[size + i] = table.opener_len[i] as isize - sub[i + 1];
            }
        }
        let next_stop = build_closer_next_stop(&kinds);
        build_closer_min_tree(&mut tree, size);
        work(n);
        CloserSearch {
            sub,
            skip,
            next_stop,
            tree,
            size,
        }
    }

    /// The first leaf in `[lo, hi)` whose value is at most `t`.
    fn first_at_most(&self, lo: usize, hi: usize, t: isize) -> Option<usize> {
        self.descend(1, 0, self.size, lo, hi, t)
    }

    fn descend(&self, node: usize, a: usize, b: usize, lo: usize, hi: usize, t: isize) -> Option<usize> {
        work(0);
        if b <= lo || hi <= a || self.tree[node] > t {
            return None;
        }
        if b - a == 1 {
            return Some(a);
        }
        let m = (a + b) / 2;
        self.descend(2 * node, a, m, lo, hi, t)
            .or_else(|| self.descend(2 * node + 1, m, b, lo, hi, t))
    }
}

impl RunTable {
    pub(super) fn new(runs: Vec<Run>) -> Self {
        let mut opener_len = vec![0; runs.len() + 1];
        for (i, r) in runs.iter().enumerate() {
            opener_len[i + 1] = opener_len[i] + if matches!(r.class, Some(3 | 4)) { r.len } else { 0 };
        }
        RunTable {
            runs,
            opener_len,
            searches: Default::default(),
        }
    }

    #[cfg(test)]
    pub(super) fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// The run that closes an opener of length `l_len` whose search starts at run `first`, with `left` (0 or below)
    /// and `mid` at that run; None when no run closes it. Same result as [`RunTable::close_by_walk`], in O(log n).
    pub(super) fn close(&mut self, first: usize, l_len: usize, same_prev: bool) -> Option<(usize, isize, isize)> {
        let variant = l_len % 3 * 2 + usize::from(same_prev);
        if self.searches[variant].is_none() {
            self.searches[variant] = Some(CloserSearch::new(self, l_len % 3, same_prev));
        }
        let search = self.searches[variant].as_ref()?;
        let first = first.min(self.runs.len());
        let target = self.opener_len[first] as isize - search.sub[first] - l_len as isize;
        let j = search.first_at_most(first, search.next_stop[first], target)?;
        let left = l_len as isize + (self.opener_len[j] - self.opener_len[first]) as isize
            - (search.sub[j + 1] - search.sub[first]);
        Some((j, left, search.skip[j] - search.skip[first]))
    }

    /// marked's emStrongRDelim loop over the runs from `first`, one run at a time: the reference for
    /// [`RunTable::close`].
    #[cfg(test)]
    pub(super) fn close_by_walk(&self, first: usize, l_len: usize, same_prev: bool) -> Option<(usize, isize, isize)> {
        let (mut left, mut mid) = (l_len as isize, 0isize);
        for (j, run) in self.runs.iter().enumerate().skip(first) {
            let r_len = run.len as isize;
            match run.class {
                Some(3 | 4) => left += r_len,
                Some(5 | 6) if !l_len.is_multiple_of(3) && (l_len as isize + r_len) % 3 == 0 => mid += r_len,
                Some(5 | 6) if same_prev => return None,
                Some(1 | 2 | 5 | 6) => {
                    left -= r_len;
                    if left <= 0 {
                        return Some((j, left, mid));
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// `[\p{P}\p{S}]`.
fn is_punct(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_punctuation()
    } else {
        contains(&rules::PUNCT_OR_SYMBOL, c.encode_utf8(&mut [0; 4]))
    }
}

/// `\s` (Unicode White_Space).
fn is_space(c: char) -> bool {
    c.is_whitespace()
}

fn ast_char_kind(c: char) -> usize {
    if is_space(c) {
        0
    } else if is_punct(c) && c != '~' {
        1
    } else {
        2
    }
}

fn und_char_kind(c: char) -> usize {
    if is_space(c) {
        0
    } else if is_punct(c) {
        1
    } else {
        2
    }
}

const AST_CLASS: [[Option<u8>; 3]; 3] = [
    [None, Some(4), Some(3)],
    [Some(1), Some(5), Some(3)],
    [Some(2), Some(2), Some(6)],
];

const UND_CLASS: [[Option<u8>; 3]; 3] = [
    [None, Some(4), Some(3)],
    [Some(1), Some(5), Some(3)],
    [Some(2), Some(2), None],
];

/// The emStrongRDelimAst branch (1-6) for a `*` run between `c` and `d` (`None` at the end of the source).
fn classify_ast(c: char, d: Option<char>) -> Option<u8> {
    AST_CLASS[ast_char_kind(c)][d.map_or(0, ast_char_kind)]
}

/// The emStrongRDelimUnd branch (1-5) for a `_` run between `c` and `d` (`None` at the end of the source).
fn classify_und(c: char, d: Option<char>) -> Option<u8> {
    UND_CLASS[und_char_kind(c)][d.map_or(0, und_char_kind)]
}

/// Every maximal `delim` run of `masked`, classified as [`Run`] describes.
pub(super) fn delimiter_runs(masked: &str, delim: u8) -> Vec<Run> {
    work(masked.len());
    let b = masked.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|&x| x == delim) {
        let start = i + off;
        let end = start + b[start..].iter().take_while(|&&x| x == delim).count();
        let c = masked[..start].chars().next_back();
        let d = masked[end..].chars().next();
        let class = c.and_then(|c| {
            if delim == b'*' {
                classify_ast(c, d)
            } else {
                classify_und(c, d)
            }
        });
        runs.push(Run {
            start,
            len: end - start,
            class,
        });
        i = end;
    }
    runs
}

fn has_group(m: &Caps<'_>, i: usize) -> bool {
    group(m, i).is_some_and(|s| !s.is_empty())
}

fn valid_em_left(m: &Caps<'_>, prev_char: &str) -> bool {
    let (h1, h2, h3, h4) = (has_group(m, 1), has_group(m, 2), has_group(m, 3), has_group(m, 4));
    if !(h1 || h2 || h3 || h4) || (h4 && contains(&rules::ALNUM, prev_char)) {
        return false;
    }
    !(h1 || h3) || prev_char.is_empty() || test(&rules::PUNCT, prev_char)
}

fn em_strong_skip(masked: &str, delim: u8, hay_start: usize) -> usize {
    let skip_rule = if delim == b'*' {
        &*rules::EM_R_AST_SKIP
    } else {
        &*rules::EM_R_UND_SKIP
    };
    exec(skip_rule, from(masked, hay_start)).map_or(0, |c| c[0].len())
}

impl Lexer {
    pub(super) fn em_strong(
        &mut self,
        src: &str,
        masked: &str,
        prev_char: &str,
        runs: &mut DelimiterRuns,
    ) -> Option<(usize, Token)> {
        let m = exec(&rules::EM_L, src)?;
        if !valid_em_left(&m, prev_char) {
            return None;
        }
        let l_len = m[0].chars().count() - 1;
        let delim = m[0].as_bytes()[0];
        let same_prev = prev_char.as_bytes() == [delim];
        let hay_start = masked.len() - src.len() + l_len;
        let skip_to = em_strong_skip(masked, delim, hay_start);
        let table = runs.get(masked, delim);
        let first = table.runs.partition_point(|r| r.start < hay_start + skip_to.max(1));
        let (j, left, mid) = table.close(first, l_len, same_prev)?;
        let run = table.runs[j];
        let r_len = (run.len as isize).min(run.len as isize + left + mid);
        let raw = &src[..l_len + (run.start - hay_start) + r_len as usize];
        if l_len.min(r_len as usize) % 2 == 1 {
            let text = &raw[1..raw.len() - 1];
            return Some((
                raw.len(),
                Token::Em {
                    tokens: self.inline_tokens(text),
                },
            ));
        }
        let text = &raw[2..raw.len() - 2];
        Some((
            raw.len(),
            Token::Strong {
                tokens: self.inline_tokens(text),
            },
        ))
    }
}
