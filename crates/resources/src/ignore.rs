//! The `ignore` package (7.x) as the package manager uses it: `.gitignore`-style rules, matched case-insensitively
//! (its default), the last matching rule winning, and a path ignored when any of its parent directories is.
//! Each rule becomes a regular expression the way `ignore` builds one: a leading `/` or an inner slash anchors it,
//! `**/`, `/**/` and a trailing `/**` match directory runs, `*` and `?` stay inside one segment, a trailing `/`
//! matches directories only, and a pattern without one also matches a directory of that name.
//!
//! Evidence: `crates/cli/fixtures/packages.mjs` records `ignore().add(rules).ignores(path)` over a matrix;
//! `tests/packages_units.rs` replays it. The skill loader (crate::skills) and the package manager both use it.

use regex::Regex;

struct Rule {
    negative: bool,
    regex: Option<Regex>,
}

/// `ignore()`: rules added in order.
#[derive(Default)]
pub struct Ignore {
    rules: Vec<Rule>,
}

/// `checkPattern`: blank lines, comments and a lone trailing backslash are not rules.
fn is_rule(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.bytes().all(|b| b == b' ')
        && !pattern.starts_with('#')
        && !(pattern.ends_with('\\') && !pattern.ends_with("\\\\"))
}

/// Trailing spaces are dropped unless escaped (`a\ ` keeps one space).
fn trim_trailing_spaces(body: &str) -> String {
    let trimmed = body.trim_end_matches(' ');
    if trimmed.len() == body.len() {
        return body.to_string();
    }
    let backslashes = trimmed.chars().rev().take_while(|c| *c == '\\').count();
    if backslashes % 2 == 1 {
        format!("{} ", &trimmed[..trimmed.len() - 1])
    } else {
        trimmed.to_string()
    }
}

const POSIX: [(&str, &str); 12] = [
    ("alnum", "0-9A-Za-z"),
    ("alpha", "A-Za-z"),
    ("blank", r" \t"),
    ("cntrl", r"\x00-\x1f\x7f"),
    ("digit", "0-9"),
    ("graph", "!-.0-~"),
    ("lower", "a-z"),
    ("print", " -.0-~"),
    ("punct", r"!-.:-@\[-`{-~"),
    ("space", r" \t\n\r"),
    ("upper", "A-Z"),
    ("xdigit", "0-9A-Fa-f"),
];

fn class_member(c: char) -> String {
    if "\\]^-[&~".contains(c) {
        format!("\\{c}")
    } else {
        c.to_string()
    }
}

fn range_start(chars: &[char], index: usize, c: char, prev: Option<char>) -> Option<char> {
    prev.filter(|_| c == '-' && chars.get(index + 1).is_some_and(|&next| next != ']'))
}

fn scan_bracket_range(chars: &[char], index: &mut usize, prev: char, body: &mut String) -> Option<()> {
    *index += 1;
    let mut to = chars[*index];
    if to == '\\' {
        *index += 1;
        to = *chars.get(*index)?;
    }
    if prev <= to {
        body.push('-');
        body.push_str(&class_member(to));
    }
    Some(())
}

fn scan_posix_class(chars: &[char], start: usize) -> Option<Option<(&'static str, usize)>> {
    let name_start = start + 2;
    let end = (name_start..chars.len()).find(|&i| chars[i] == ']')?;
    if end > name_start && chars[end - 1] == ':' {
        let name: String = chars[name_start..end - 1].iter().collect();
        let expanded = POSIX.iter().find(|(n, _)| *n == name)?.1;
        return Some(Some((expanded, end)));
    }
    Some(None)
}

fn scan_bracket_escape(chars: &[char], index: &mut usize, prev: &mut Option<char>, body: &mut String) -> Option<()> {
    let escaped = *chars.get(*index + 1)?;
    body.push_str(&class_member(escaped));
    *prev = Some(escaped);
    *index += 1;
    Some(())
}

fn scan_bracket_posix(chars: &[char], index: &mut usize, prev: &mut Option<char>, body: &mut String) -> Option<bool> {
    if chars.get(*index + 1) != Some(&':') {
        return Some(false);
    }
    let Some((expanded, end)) = scan_posix_class(chars, *index)? else {
        return Some(false);
    };
    body.push_str(expanded);
    *prev = None;
    *index = end;
    Some(true)
}

fn scan_bracket_step(chars: &[char], index: &mut usize, prev: &mut Option<char>, body: &mut String) -> Option<()> {
    let c = *chars.get(*index)?;
    if c == '\\' {
        return scan_bracket_escape(chars, index, prev, body);
    }
    if let Some(from) = range_start(chars, *index, c, *prev) {
        scan_bracket_range(chars, index, from, body)?;
        *prev = None;
        return Some(());
    }
    if c == '[' && scan_bracket_posix(chars, index, prev, body)? {
        return Some(());
    }
    body.push_str(&class_member(c));
    *prev = Some(c);
    Some(())
}

fn format_bracket(body: &str, negated: bool) -> String {
    if negated {
        format!("[^/{body}]")
    } else {
        format!("[{body}&&[^/]]")
    }
}

/// `scanBracket`: a `[...]` class at `start`, as a regex class that never matches `/`, and the index of its `]`.
/// None: it never closes (the rule then matches nothing).
fn scan_bracket(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut index = start + 1;
    let negated = matches!(chars.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let mut body = String::new();
    let mut prev: Option<char> = None;
    loop {
        scan_bracket_step(chars, &mut index, &mut prev, &mut body)?;
        index += 1;
        if chars.get(index) == Some(&']') {
            return Some((format_bracket(&body, negated), index));
        }
    }
}

fn trailing_star(glob: &[char], i: usize, last_of_pattern: bool, after_slash: bool) -> Option<&'static str> {
    if !last_of_pattern || !glob[i..].iter().all(|c| *c == '*') {
        return None;
    }
    Some(if after_slash && i == 0 {
        "[^/]+(?:$|/$)"
    } else {
        "[^/]*(?:$|/$)"
    })
}

fn segment_token(glob: &[char], i: usize) -> Option<(String, usize)> {
    match glob[i] {
        '\\' if i + 1 < glob.len() => Some((regex::escape(&glob[i + 1].to_string()), i + 1)),
        '*' => Some(("[^/]*".to_string(), i)),
        '?' => Some(("[^/]".to_string(), i)),
        '[' => scan_bracket(glob, i),
        other => Some((regex::escape(&other.to_string()), i)),
    }
}

/// The regex of one segment's glob (no `/` inside). None: an unclosed class (never matches).
fn segment(glob: &[char], last_of_pattern: bool, after_slash: bool) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < glob.len() {
        if glob[i] == '*'
            && let Some(tail) = trailing_star(glob, i, last_of_pattern, after_slash)
        {
            out.push_str(tail);
            return Some(out);
        }
        let (piece, end) = segment_token(glob, i)?;
        out.push_str(&piece);
        i = end + 1;
    }
    Some(out)
}

/// A rule that matches no path.
fn never() -> Option<Regex> {
    Regex::new("[^\\s\\S]").ok()
}

fn strip_anchor(body: &str) -> (&str, String) {
    let mut rest = body;
    let inner_slash = rest.trim_end_matches('/').contains('/');
    if let Some(stripped) = rest.strip_prefix('/') {
        return (stripped, "^".to_string());
    }
    if rest.starts_with("**/") {
        while let Some(stripped) = rest.strip_prefix("**/") {
            rest = stripped;
        }
        return (rest, "^(?:.*/)?".to_string());
    }
    let prefix = if inner_slash { "^" } else { "(?:^|/)" };
    (rest, prefix.to_string())
}

fn push_globstar(source: &mut String, last: bool, dir_only: bool) -> bool {
    if !last {
        source.push_str("(?:/[^/]+)*");
        return false;
    }
    source.push_str(if dir_only { "(?:/[^/]+)+" } else { "/.+" });
    !dir_only
}

fn finish_source(source: &mut String, dir_only: bool) {
    if dir_only {
        source.push_str("/$");
    } else if !source.ends_with("(?:$|/$)") {
        source.push_str("(?:$|/$)");
    }
}

/// `makeRegex` for one rule body (without its `!`).
fn compile(body: &str) -> Option<Regex> {
    let body = trim_trailing_spaces(body);
    let (rest, mut source) = strip_anchor(&body);
    let dir_only = rest.ends_with('/');
    let core = rest.strip_suffix('/').unwrap_or(rest);
    let segments: Vec<&str> = core.split('/').collect();
    let count = segments.len();
    for (index, seg) in segments.iter().enumerate() {
        let last = index + 1 == count;
        if *seg == "**" && index > 0 {
            if push_globstar(&mut source, last, dir_only) {
                return Regex::new(&format!("(?i){source}")).ok();
            }
            continue;
        }
        if index > 0 {
            source.push('/');
        }
        let glob: Vec<char> = seg.chars().collect();
        let Some(compiled) = segment(&glob, last && !dir_only, index > 0) else {
            return never();
        };
        source.push_str(&compiled);
    }
    finish_source(&mut source, dir_only);
    Regex::new(&format!("(?i){source}")).ok()
}

impl Ignore {
    pub fn new() -> Self {
        Self::default()
    }

    /// `add(patterns)`.
    pub fn add<S: AsRef<str>>(&mut self, patterns: &[S]) {
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if !is_rule(pattern) {
                continue;
            }
            let (negative, body) = match pattern.strip_prefix('!') {
                Some(body) => (true, body),
                None => (false, pattern),
            };
            let body = if let Some(rest) = body.strip_prefix("\\!") {
                format!("!{rest}")
            } else if let Some(rest) = body.strip_prefix("\\#") {
                format!("#{rest}")
            } else {
                body.to_string()
            };
            self.rules.push(Rule {
                negative,
                regex: compile(&body),
            });
        }
    }

    fn test(&self, path: &str) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.regex.as_ref().is_some_and(|r| r.is_match(path)) {
                ignored = !rule.negative;
            }
        }
        ignored
    }

    /// `ignores(path)` for a relative POSIX path (`dir/` for a directory).
    pub fn ignores(&self, path: &str) -> bool {
        let trimmed = path.strip_suffix('/').unwrap_or(path);
        let mut at = 0;
        while let Some(slash) = trimmed[at..].find('/') {
            let parent = &trimmed[..at + slash + 1];
            if self.test(parent) {
                return true;
            }
            at += slash + 1;
        }
        self.test(path)
    }
}

/// `prefixIgnorePattern`: a rule of an ignore file in a subdirectory, made relative to the walk's root.
pub fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("\\#")) {
        return None;
    }
    let (negated, mut pattern) = if let Some(rest) = line.strip_prefix('!') {
        (true, rest)
    } else if line.starts_with("\\!") {
        (false, &line[1..])
    } else {
        (false, line)
    };
    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest;
    }
    let prefixed = format!("{prefix}{pattern}");
    Some(if negated { format!("!{prefixed}") } else { prefixed })
}

/// `addIgnoreRules`: `.gitignore`, `.ignore` and `.fdignore` of `dir`, prefixed with its path under `root`.
pub fn add_ignore_rules(ig: &mut Ignore, dir: &str, root: &str) {
    let rel = jsrt::path::relative(root, dir);
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for name in [".gitignore", ".ignore", ".fdignore"] {
        let Ok(content) = std::fs::read_to_string(jsrt::path::join(&[dir, name])) else {
            continue;
        };
        let patterns: Vec<String> = content
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .filter_map(|line| prefix_ignore_pattern(line, &prefix))
            .collect();
        if !patterns.is_empty() {
            ig.add(&patterns);
        }
    }
}
