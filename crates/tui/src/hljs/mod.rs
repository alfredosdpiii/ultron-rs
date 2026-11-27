//! A port of the highlight.js 10.7.3 engine (`lib/core.js`: `compileLanguage`, `_highlight`, the token tree emitter)
//! and of the TypeScript client's `renderHighlightedHtml` (packages/coding-agent/src/utils/syntax-highlight.ts), so
//! code is highlighted exactly as the TypeScript `highlightCode` does.
//!
//! Grammars are written as highlight.js mode objects ([`Def`]) with their JavaScript regex sources; [`js_regex`]
//! translates those to fancy-regex with JavaScript semantics (ASCII `\w`/`\b`/`\d`, JavaScript `\s`, `.` and
//! multi-line `^`/`$`). Mode compilation follows core.js, including its shared-object effects: a keywords object
//! gives its `$pattern` only to the first mode compiled with it, variants are expanded once and shared, and the
//! resumable matcher's rule positions are overwritten by the last sub-matcher built (state that, as in the TS
//! process, persists across calls on a thread).
//!
//! Only what the ported grammars use is implemented: no sub-languages, `on:begin`/`on:end` callbacks or
//! `endSameAsBegin`; relevance is not computed (it never reaches the output).

pub mod python;
mod regex;

pub use regex::js_regex;

use std::cell::RefCell;
use std::collections::HashMap;

use fancy_regex::Regex;

/// A `contains` entry: another mode or `'self'`.
#[derive(Clone, Copy, Debug)]
pub enum Contain {
    Mode(usize),
    SelfRef,
}

/// A highlight.js mode object. `None` means the key is absent (so a variant does not override it).
#[derive(Clone, Debug, Default)]
pub struct Def {
    pub class_name: Option<&'static str>,
    pub begin: Option<&'static str>,
    pub end: Option<&'static str>,
    pub begin_keywords: Option<&'static str>,
    /// A shared keywords object (index into [`Grammar::keywords`]).
    pub keywords: Option<usize>,
    pub illegal: Option<&'static str>,
    pub ends_with_parent: Option<bool>,
    pub ends_parent: Option<bool>,
    pub exclude_begin: Option<bool>,
    pub exclude_end: Option<bool>,
    pub return_begin: Option<bool>,
    pub return_end: Option<bool>,
    pub skip: Option<bool>,
    pub contains: Option<Vec<Contain>>,
    /// Indices of the variant objects (partial [`Def`]s merged over this one).
    pub variants: Option<Vec<usize>>,
    pub starts: Option<usize>,
    /// One of hljs's frozen built-in modes: every use gets its own copy.
    pub frozen: bool,
}

impl Def {
    /// `inherit(base, { variants: null }, variant)`.
    fn merged(base: &Def, variant: &Def) -> Def {
        Def {
            class_name: variant.class_name.or(base.class_name),
            begin: variant.begin.or(base.begin),
            end: variant.end.or(base.end),
            begin_keywords: variant.begin_keywords.or(base.begin_keywords),
            keywords: variant.keywords.or(base.keywords),
            illegal: variant.illegal.or(base.illegal),
            ends_with_parent: variant.ends_with_parent.or(base.ends_with_parent),
            ends_parent: variant.ends_parent.or(base.ends_parent),
            exclude_begin: variant.exclude_begin.or(base.exclude_begin),
            exclude_end: variant.exclude_end.or(base.exclude_end),
            return_begin: variant.return_begin.or(base.return_begin),
            return_end: variant.return_end.or(base.return_end),
            skip: variant.skip.or(base.skip),
            contains: variant.contains.clone().or_else(|| base.contains.clone()),
            variants: None,
            starts: variant.starts.or(base.starts),
            frozen: false,
        }
    }
}

/// A keywords object: `$pattern` and the word lists per class, in key order.
#[derive(Clone, Debug)]
pub struct KeywordTable {
    pub pattern: Option<&'static str>,
    pub classes: Vec<(&'static str, Vec<&'static str>)>,
}

/// A language definition: its mode objects, keywords objects and the top-level mode.
pub struct Grammar {
    pub defs: Vec<Def>,
    pub keywords: Vec<KeywordTable>,
    pub root: usize,
    pub case_insensitive: bool,
}

fn lang_re(src: &str, ci: bool) -> Regex {
    Regex::new(&js_regex(src, ci)).unwrap_or_else(|_| Regex::new("(?!)").expect("never-matching regex"))
}

/// A JavaScript object identity during compilation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Obj {
    Def(usize),
    Variant(usize, usize),
    Copy(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleKind {
    Begin(usize),
    End,
    Illegal,
}

struct Rule {
    re: Regex,
    kind: RuleKind,
}

struct CMode {
    class_name: Option<&'static str>,
    begin: String,
    end_re: Option<Regex>,
    terminator_end: String,
    keywords: Option<HashMap<String, &'static str>>,
    keyword_re: Option<Regex>,
    before_dot: bool,
    ends_with_parent: bool,
    ends_parent: bool,
    exclude_begin: bool,
    exclude_end: bool,
    return_begin: bool,
    return_end: bool,
    skip: bool,
    starts: Option<usize>,
    rules: Vec<Rule>,
    begin_count: usize,
}

/// The `ResumableMultiRegex` state of one mode: which sub-matchers exist, the shared rule positions and the
/// resume index.
#[derive(Default)]
struct MatcherState {
    built: Vec<bool>,
    positions: Vec<usize>,
    regex_index: usize,
}

/// A compiled language (`compileLanguage`), with the matcher state that outlives a call.
pub struct Compiled {
    modes: Vec<CMode>,
    root: usize,
    states: Vec<MatcherState>,
}

struct Compiler<'g> {
    g: &'g Grammar,
    patterns: Vec<Option<&'static str>>,
    memo: HashMap<Obj, usize>,
    copies: Vec<Obj>,
    modes: Vec<Option<CMode>>,
    variant_cache: HashMap<usize, Vec<Obj>>,
}

impl Compiler<'_> {
    fn def_of(&self, obj: Obj) -> Def {
        match obj {
            Obj::Def(i) => self.g.defs[i].clone(),
            Obj::Variant(base, k) => {
                let base_def = &self.g.defs[base];
                let v = base_def.variants.as_ref().expect("variants")[k];
                Def::merged(base_def, &self.g.defs[v])
            }
            Obj::Copy(n) => {
                let mut d = self.def_of(self.copies[n]);
                d.frozen = false;
                d
            }
        }
    }

    fn copy_of(&mut self, obj: Obj) -> Obj {
        self.copies.push(obj);
        Obj::Copy(self.copies.len() - 1)
    }

    fn depends_on_parent(&self, def: &Def) -> bool {
        def.ends_with_parent == Some(true) || def.starts.is_some_and(|s| self.depends_on_parent(&self.g.defs[s]))
    }

    /// `expandOrCloneMode`.
    fn expand(&mut self, obj: Obj) -> Vec<Obj> {
        let def = self.def_of(obj);
        if let (Obj::Def(i), Some(vs)) = (obj, &def.variants) {
            let n = vs.len();
            return self
                .variant_cache
                .entry(i)
                .or_insert_with(|| (0..n).map(|k| Obj::Variant(i, k)).collect())
                .clone();
        }
        if self.depends_on_parent(&def) || def.frozen {
            return vec![self.copy_of(obj)];
        }
        vec![obj]
    }

    fn build_keyword_map(classes: Vec<(&'static str, Vec<&'static str>)>, ci: bool) -> HashMap<String, &'static str> {
        let mut map = HashMap::new();
        for (class, words) in classes {
            for w in words {
                let word = w.split('|').next().unwrap_or("");
                let word = if ci { word.to_lowercase() } else { word.to_string() };
                map.insert(word, class);
            }
        }
        map
    }

    fn build_keywords(
        &mut self,
        def: &Def,
        parent: Option<usize>,
        begin: &mut Option<String>,
        before_dot: &mut bool,
    ) -> (Option<HashMap<String, &'static str>>, Option<Regex>) {
        let ci = self.g.case_insensitive;
        let mut keyword_words: Option<Vec<(&'static str, Vec<&'static str>)>> = None;
        let mut keyword_pattern: Option<&'static str> = None;
        if let Some(t) = def.keywords {
            keyword_pattern = self.patterns[t].take();
            keyword_words = Some(self.g.keywords[t].classes.clone());
        }
        if parent.is_some()
            && let Some(bk) = def.begin_keywords
        {
            *begin = Some(format!(
                r"\b({})(?!\.)(?=\b|\s)",
                bk.split(' ').collect::<Vec<_>>().join("|")
            ));
            *before_dot = true;
            if keyword_words.is_none() {
                keyword_words = Some(vec![("keyword", bk.split(' ').collect())]);
            }
        }
        let keywords = keyword_words.map(|classes| Self::build_keyword_map(classes, ci));
        let keyword_re = keywords
            .as_ref()
            .map(|_| lang_re(keyword_pattern.unwrap_or(r"\w+"), ci));
        (keywords, keyword_re)
    }

    fn build_end_rules(
        &self,
        def: &Def,
        parent: Option<usize>,
        begin: &mut Option<String>,
        ends_with_parent: bool,
    ) -> (Option<Regex>, String) {
        let ci = self.g.case_insensitive;
        let Some(p) = parent else {
            return (None, String::new());
        };
        if begin.is_none() {
            *begin = Some(r"\B|\b".into());
        }
        let end = def
            .end
            .map(str::to_string)
            .or_else(|| (!ends_with_parent).then(|| r"\B|\b".to_string()));
        let end_re = end.as_deref().map(|e| lang_re(e, ci));
        let mut terminator_end = end.clone().unwrap_or_default();
        let parent_end = &self.modes[p].as_ref().expect("parent compiled first").terminator_end;
        if ends_with_parent && !parent_end.is_empty() {
            if end.is_some() {
                terminator_end.push('|');
            }
            terminator_end.push_str(parent_end);
        }
        (end_re, terminator_end)
    }

    fn compile_children_and_starts(
        &mut self,
        obj: Obj,
        def: &Def,
        parent: Option<usize>,
        cid: usize,
    ) -> (Vec<usize>, Option<usize>) {
        let mut children = Vec::new();
        for c in def.contains.clone().unwrap_or_default() {
            let o = match c {
                Contain::SelfRef => obj,
                Contain::Mode(i) => Obj::Def(i),
            };
            children.extend(self.expand(o));
        }
        let child_ids: Vec<usize> = children.into_iter().map(|o| self.compile(o, Some(cid))).collect();
        let starts = def.starts.map(|s| {
            let o = if self.depends_on_parent(&self.g.defs[s]) {
                self.copy_of(Obj::Def(s))
            } else {
                Obj::Def(s)
            };
            self.compile(o, parent)
        });
        (child_ids, starts)
    }

    fn build_mode_rules(&self, child_ids: &[usize], terminator_end: &str, illegal: Option<&str>) -> (Vec<Rule>, usize) {
        let ci = self.g.case_insensitive;
        let mut rules = Vec::new();
        for &child in child_ids {
            let src = self.modes[child].as_ref().expect("compiled").begin.clone();
            rules.push(Rule {
                re: lang_re(&src, ci),
                kind: RuleKind::Begin(child),
            });
        }
        let begin_count = rules.len();
        if !terminator_end.is_empty() {
            rules.push(Rule {
                re: lang_re(terminator_end, ci),
                kind: RuleKind::End,
            });
        }
        if let Some(illegal) = illegal {
            rules.push(Rule {
                re: lang_re(illegal, ci),
                kind: RuleKind::Illegal,
            });
        }
        (rules, begin_count)
    }

    /// `compileMode`.
    fn compile(&mut self, obj: Obj, parent: Option<usize>) -> usize {
        if let Some(&c) = self.memo.get(&obj) {
            return c;
        }
        let def = self.def_of(obj);
        let cid = self.modes.len();
        self.modes.push(None);
        self.memo.insert(obj, cid);

        let mut begin = def.begin.map(str::to_string);
        let mut before_dot = false;
        let (keywords, keyword_re) = self.build_keywords(&def, parent, &mut begin, &mut before_dot);
        let ends_with_parent = def.ends_with_parent.unwrap_or(false);
        let (end_re, terminator_end) = self.build_end_rules(&def, parent, &mut begin, ends_with_parent);
        self.modes[cid] = Some(CMode {
            class_name: def.class_name.filter(|c| !c.is_empty()),
            begin: begin.unwrap_or_default(),
            end_re,
            terminator_end: terminator_end.clone(),
            keywords,
            keyword_re,
            before_dot,
            ends_with_parent,
            ends_parent: def.ends_parent.unwrap_or(false),
            exclude_begin: def.exclude_begin.unwrap_or(false),
            exclude_end: def.exclude_end.unwrap_or(false),
            return_begin: def.return_begin.unwrap_or(false),
            return_end: def.return_end.unwrap_or(false),
            skip: def.skip.unwrap_or(false),
            starts: None,
            rules: Vec::new(),
            begin_count: 0,
        });

        let (child_ids, starts) = self.compile_children_and_starts(obj, &def, parent, cid);
        let (rules, begin_count) = self.build_mode_rules(&child_ids, &terminator_end, def.illegal);
        let m = self.modes[cid].as_mut().expect("this mode");
        m.rules = rules;
        m.begin_count = begin_count;
        m.starts = starts;
        cid
    }
}

/// `compileLanguage`.
pub fn compile(g: &Grammar) -> Compiled {
    let mut c = Compiler {
        g,
        patterns: g.keywords.iter().map(|k| k.pattern).collect(),
        memo: HashMap::new(),
        copies: Vec::new(),
        modes: Vec::new(),
        variant_cache: HashMap::new(),
    };
    let root = c.compile(Obj::Def(g.root), None);
    let modes: Vec<CMode> = c.modes.into_iter().map(|m| m.expect("compiled")).collect();
    let states = modes
        .iter()
        .map(|m| MatcherState {
            built: vec![false; m.rules.len() + 1],
            positions: vec![0; m.rules.len()],
            regex_index: 0,
        })
        .collect();
    Compiled { modes, root, states }
}

/// The token tree, flattened: text, and opened/closed scopes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    Text(String),
    Open(&'static str),
    Close,
}

struct Hit {
    rule: usize,
    start: usize,
    end: usize,
}

/// Per-call cache of each rule's leftmost match from a position: a match found from `p` at `q` is also the
/// leftmost from any position in `p..=q` (and "none" stays none from later positions).
type FindCache = HashMap<(usize, usize), (usize, Option<(usize, usize)>)>;

fn next_char_len(text: &str, at: usize) -> usize {
    text.get(at..).and_then(|s| s.chars().next()).map_or(1, char::len_utf8)
}

impl Compiled {
    fn find(&self, cache: &mut FindCache, mode: usize, rule: usize, text: &str, from: usize) -> Option<(usize, usize)> {
        if from > text.len() {
            return None;
        }
        if let Some(&(f, r)) = cache.get(&(mode, rule))
            && f <= from
            && r.is_none_or(|(s, _)| s >= from)
        {
            return r;
        }
        let r = self.modes[mode].rules[rule]
            .re
            .find_from_pos(text, from)
            .ok()
            .flatten()
            .map(|m| (m.start(), m.end()));
        cache.insert((mode, rule), (from, r));
        r
    }

    /// `getMatcher(index)` then `exec` on it: the leftmost match among rules `index..` (first rule on a tie).
    fn sub_exec(&mut self, cache: &mut FindCache, mode: usize, index: usize, text: &str, from: usize) -> Option<Hit> {
        let n = self.modes[mode].rules.len();
        let st = &mut self.states[mode];
        if index <= n && !st.built[index] {
            st.built[index] = true;
            for (j, r) in (index..n).enumerate() {
                st.positions[r] = j;
            }
        }
        let mut best: Option<Hit> = None;
        for rule in index..n {
            if let Some((start, end)) = self.find(cache, mode, rule, text, from)
                && best.as_ref().is_none_or(|b| start < b.start)
            {
                best = Some(Hit { rule, start, end });
            }
        }
        best
    }

    /// `ResumableMultiRegex.exec`.
    fn exec(&mut self, cache: &mut FindCache, mode: usize, text: &str, last_index: usize) -> Option<Hit> {
        let index = self.states[mode].regex_index;
        let mut result = self.sub_exec(cache, mode, index, text, last_index);
        if index != 0 && result.as_ref().is_none_or(|r| r.start != last_index) {
            let from = last_index + next_char_len(text, last_index);
            result = self.sub_exec(cache, mode, 0, text, from);
        }
        if let Some(r) = &result {
            let st = &mut self.states[mode];
            st.regex_index += st.positions[r.rule] + 1;
            if st.regex_index == self.modes[mode].begin_count {
                st.regex_index = 0;
            }
        }
        result
    }

    /// `hljs.highlight(code, { language, ignoreIllegals: true })` as a token list; `None` where highlight.js
    /// returns the escaped code instead (an error raised in safe mode).
    pub fn highlight(&mut self, code: &str) -> Option<Vec<Token>> {
        Run::new(self, code).run()
    }
}

enum Kind {
    Begin,
    End,
    Illegal,
}

struct Run<'a> {
    c: &'a mut Compiled,
    text: &'a str,
    cache: FindCache,
    stack: Vec<usize>,
    tokens: Vec<Token>,
    open: usize,
    buffer: String,
    resume: bool,
    last: Option<(bool, usize)>,
}

impl<'a> Run<'a> {
    fn new(c: &'a mut Compiled, text: &'a str) -> Self {
        let root = c.root;
        Self {
            c,
            text,
            cache: HashMap::new(),
            stack: vec![root],
            tokens: Vec::new(),
            open: 0,
            buffer: String::new(),
            resume: false,
            last: None,
        }
    }

    fn top(&self) -> usize {
        *self.stack.last().expect("root stays")
    }

    fn add_text(&mut self, s: &str) {
        if !s.is_empty() {
            self.tokens.push(Token::Text(s.to_string()));
        }
    }

    fn open_node(&mut self, kind: &'static str) {
        self.tokens.push(Token::Open(kind));
        self.open += 1;
    }

    fn close_node(&mut self) {
        if self.open > 0 {
            self.open -= 1;
            self.tokens.push(Token::Close);
        }
    }

    /// `processKeywords` (`processBuffer` without sub-languages).
    fn process_buffer(&mut self) {
        let buffer = std::mem::take(&mut self.buffer);
        let mode = &self.c.modes[self.top()];
        let (Some(keywords), Some(re)) = (&mode.keywords, &mode.keyword_re) else {
            self.add_text(&buffer);
            return;
        };
        let mut pieces: Vec<(Option<&'static str>, String)> = Vec::new();
        let mut buf = String::new();
        let mut last = 0;
        let mut at = 0;
        while at <= buffer.len() {
            let Some(m) = re.find_from_pos(&buffer, at).ok().flatten() else {
                break;
            };
            buf.push_str(&buffer[last..m.start()]);
            match keywords.get(m.as_str()) {
                Some(class) if !class.starts_with('_') => {
                    pieces.push((None, std::mem::take(&mut buf)));
                    pieces.push((Some(class), m.as_str().to_string()));
                }
                _ => buf.push_str(m.as_str()),
            }
            last = m.end();
            at = if m.end() == m.start() {
                m.end() + next_char_len(&buffer, m.end())
            } else {
                m.end()
            };
        }
        buf.push_str(buffer.get(last..).unwrap_or(""));
        pieces.push((None, buf));
        for (class, text) in pieces {
            match class {
                Some(class) if !text.is_empty() => {
                    self.open_node(class);
                    self.add_text(&text);
                    self.close_node();
                }
                Some(_) => {}
                None => self.add_text(&text),
            }
        }
    }

    fn start_new_mode(&mut self, mode: usize) {
        if let Some(class) = self.c.modes[mode].class_name {
            self.open_node(class);
        }
        self.stack.push(mode);
    }

    /// `endOfMode`: the depth of the mode the end closes.
    fn end_of_mode(&self, depth: usize, remainder: &str) -> Option<usize> {
        let mode = &self.c.modes[self.stack[depth]];
        let matched = mode
            .end_re
            .as_ref()
            .is_some_and(|re| re.find(remainder).ok().flatten().is_some_and(|m| m.start() == 0));
        if matched {
            let mut d = depth;
            while self.c.modes[self.stack[d]].ends_parent && d > 0 {
                d -= 1;
            }
            return Some(d);
        }
        if mode.ends_with_parent && depth > 0 {
            return self.end_of_mode(depth - 1, remainder);
        }
        None
    }

    fn do_ignore(&mut self, lexeme: &str, start: usize) -> usize {
        if self.c.states[self.top()].regex_index == 0 {
            match lexeme.chars().next() {
                Some(ch) => self.buffer.push(ch),
                None => self.buffer.push_str("undefined"),
            }
            next_char_len(self.text, start)
        } else {
            self.resume = true;
            0
        }
    }

    fn do_begin(&mut self, mode: usize, lexeme: &str, start: usize) -> usize {
        let m = &self.c.modes[mode];
        if m.before_dot && self.text[..start].ends_with('.') {
            return self.do_ignore(lexeme, start);
        }
        let (skip, exclude_begin, return_begin) = (m.skip, m.exclude_begin, m.return_begin);
        if skip {
            self.buffer.push_str(lexeme);
        } else {
            if exclude_begin {
                self.buffer.push_str(lexeme);
            }
            self.process_buffer();
            if !return_begin && !exclude_begin {
                self.buffer = lexeme.to_string();
            }
        }
        self.start_new_mode(mode);
        if return_begin { 0 } else { lexeme.len() }
    }

    fn do_end(&mut self, lexeme: &str, start: usize) -> Option<usize> {
        let depth = self.end_of_mode(self.stack.len() - 1, &self.text[start..])?;
        let end_mode = self.stack[depth];
        let origin = &self.c.modes[self.top()];
        let (skip, return_end, exclude_end) = (origin.skip, origin.return_end, origin.exclude_end);
        if skip {
            self.buffer.push_str(lexeme);
        } else {
            if !(return_end || exclude_end) {
                self.buffer.push_str(lexeme);
            }
            self.process_buffer();
            if exclude_end {
                self.buffer = lexeme.to_string();
            }
        }
        while self.stack.len() > depth {
            if self.c.modes[self.top()].class_name.is_some() {
                self.close_node();
            }
            self.stack.pop();
        }
        if let Some(starts) = self.c.modes[end_mode].starts {
            self.start_new_mode(starts);
        }
        Some(if return_end { 0 } else { lexeme.len() })
    }

    /// `processLexeme`; `Err` is the "potential infinite loop" error.
    fn process_lexeme(
        &mut self,
        before: &str,
        hit: Option<(Kind, usize, usize, usize)>,
        iterations: usize,
    ) -> Result<usize, ()> {
        self.buffer.push_str(before);
        let Some((kind, rule_mode, start, end)) = hit else {
            self.process_buffer();
            return Ok(0);
        };
        let lexeme = &self.text[start..end];
        let is_begin = matches!(kind, Kind::Begin);
        if self.last == Some((true, start)) && matches!(kind, Kind::End) && lexeme.is_empty() {
            let n = next_char_len(self.text, start);
            self.buffer.push_str(self.text.get(start..start + n).unwrap_or(""));
            return Ok(n);
        }
        self.last = Some((is_begin, start));
        match kind {
            Kind::Begin => return Ok(self.do_begin(rule_mode, lexeme, start)),
            Kind::End => {
                if let Some(n) = self.do_end(lexeme, start) {
                    return Ok(n);
                }
            }
            Kind::Illegal => {
                if lexeme.is_empty() {
                    return Ok(next_char_len(self.text, start));
                }
            }
        }
        if iterations > 100_000 && iterations > self.text[..start].encode_utf16().count() * 3 {
            return Err(());
        }
        self.buffer.push_str(lexeme);
        Ok(lexeme.len())
    }

    fn run(mut self) -> Option<Vec<Token>> {
        let text = self.text;
        let mut index = 0;
        let mut iterations = 0;
        let root = self.c.root;
        self.c.states[root].regex_index = 0;
        loop {
            iterations += 1;
            let top = self.top();
            if self.resume {
                self.resume = false;
            } else {
                self.c.states[top].regex_index = 0;
            }
            let Some(hit) = self.c.exec(&mut self.cache, top, text, index) else {
                break;
            };
            let (kind, child) = match self.c.modes[top].rules[hit.rule].kind {
                RuleKind::Begin(child) => (Kind::Begin, child),
                RuleKind::End => (Kind::End, 0),
                RuleKind::Illegal => (Kind::Illegal, 0),
            };
            let before = &text[index.min(hit.start)..hit.start];
            let processed = self
                .process_lexeme(before, Some((kind, child, hit.start, hit.end)), iterations)
                .ok()?;
            index = (hit.start + processed).min(text.len() + 1);
        }
        let rest = text.get(index..).unwrap_or("");
        self.process_lexeme(rest, None, iterations).ok()?;
        while self.open > 0 {
            self.close_node();
        }
        Some(self.tokens)
    }
}

/// `renderHighlightedHtml` over the token list: each run of text between scope changes, with the innermost scope
/// that `has_formatter` accepts (`getScopeFormatter`: the scope, then its part before `.`, then before `-`).
pub fn render<K>(
    tokens: &[Token],
    formatter: &dyn Fn(&str) -> Option<K>,
    apply: &dyn Fn(&K, &str) -> String,
) -> String {
    let mut out = String::new();
    let mut buf = String::new();
    let mut scopes: Vec<&str> = Vec::new();
    let flush = |buf: &mut String, scopes: &[&str], out: &mut String| {
        if buf.is_empty() {
            return;
        }
        let key = scopes.iter().rev().find_map(|s| scope_formatter(s, formatter));
        match key {
            Some(k) => out.push_str(&apply(&k, buf)),
            None => out.push_str(buf),
        }
        buf.clear();
    };
    for t in tokens {
        match t {
            Token::Text(s) => buf.push_str(s),
            Token::Open(k) => {
                flush(&mut buf, &scopes, &mut out);
                scopes.push(k);
            }
            Token::Close => {
                flush(&mut buf, &scopes, &mut out);
                scopes.pop();
            }
        }
    }
    flush(&mut buf, &scopes, &mut out);
    out
}

fn scope_formatter<K>(scope: &str, formatter: &dyn Fn(&str) -> Option<K>) -> Option<K> {
    formatter(scope)
        .or_else(|| scope.find('.').and_then(|i| formatter(&scope[..i])))
        .or_else(|| scope.find('-').and_then(|i| formatter(&scope[..i])))
}

thread_local! {
    static LANGUAGES: RefCell<HashMap<&'static str, Compiled>> = RefCell::new(HashMap::new());
}

/// Names and aliases of the ported languages (`hljs.getLanguage(name) !== undefined`).
pub fn supports_language(name: &str) -> bool {
    canonical(name).is_some()
}

fn canonical(name: &str) -> Option<&'static str> {
    match name.to_lowercase().as_str() {
        "python" | "py" | "gyp" | "ipython" => Some("python"),
        _ => None,
    }
}

/// `hljs.highlight(code, { language, ignoreIllegals: true })` with the language compiled once per thread (its
/// matcher state carries over between calls, as in the TS process). `None` for an unknown language or where
/// highlight.js returns the plain code.
pub fn highlight(code: &str, language: &str) -> Option<Vec<Token>> {
    let lang = canonical(language)?;
    LANGUAGES.with(|cell| {
        let mut langs = cell.borrow_mut();
        let compiled = langs.entry(lang).or_insert_with(|| compile(python::grammar()));
        compiled.highlight(code)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(src: &str) -> Regex {
        Regex::new(&js_regex(src, false)).expect("compiles")
    }

    #[test]
    fn hljs_js_regex_has_javascript_semantics() {
        assert!(re(r"\bé").is_match("aé").unwrap());
        assert!(!re(r"\bé").is_match(" é").unwrap());
        assert_eq!(re(r"\w+").find("é_ab9").unwrap().unwrap().as_str(), "_ab9");
        assert!(re(r"\Ba").is_match("ba").unwrap());
        assert!(re(r"\s").is_match("\u{feff}").unwrap());
        assert!(!re(r"\s").is_match("\u{85}").unwrap());
        assert!(re(r"[\s\S]").is_match("\u{85}").unwrap());
        assert!(!re(r"a.b").is_match("a\rb").unwrap());
        assert!(re(r"^b").is_match("a\rb").unwrap());
        assert!(re(r"a$").is_match("a\rb").unwrap());
        assert!(re(r"a$").is_match("a").unwrap());
        assert!(re(r"\{\{").is_match("{{").unwrap());
        assert!(re(r"{{").is_match("{{").unwrap());
        assert!(re(r"a{2}").is_match("aa").unwrap());
        assert!(!re(r"a{2}").is_match("a{2}").unwrap());
        assert!(re(r"\b\B").find("ab cd").unwrap().is_none());
        assert!(re(r"(?=# type:)").is_match("# type: int").unwrap());
        assert!(re(r"^[\t ]*@").is_match("x\n  @dec").unwrap());
    }
}
