//! `minimatch(text, pattern, { nocase })` (minimatch 10): `#` comments, `!` negation, brace expansion (`{a,b}`,
//! `{1..3}`, `{a..c}`), `/`-separated segments with `**` globstar, `*`, `?`, `[...]` classes (ranges, `!`/`^`
//! negation, POSIX classes), `\` escapes, extglobs (`+(a|b)`, `*(..)`, `?(..)`, `@(..)`, `!(..)`), and the dot rule (a
//! wildcard never matches a segment's leading `.`). The model scope matches case-insensitively; the package manager's
//! resource filters case-sensitively.
//!
//! Evidence: `crates/ai/fixtures/generate-model-resolver.mjs` records minimatch's answers (`globs`) and
//! `crates/ai/tests/model_resolver.rs` replays them; `crates/cli/fixtures/packages.mjs` records the case-sensitive
//! ones, replayed by `crates/cli/tests/packages_units.rs`.

pub fn minimatch(text: &str, pattern: &str, nocase: bool) -> bool {
    if pattern.starts_with('#') {
        return false;
    }
    let bangs = pattern.chars().take_while(|c| *c == '!').count();
    let body = &pattern[bangs..];
    let matched = brace_expand(body).iter().any(|p| match_path(text, p, nocase));
    if bangs % 2 == 1 { !matched } else { matched }
}

/// `brace-expansion`: every alternative of the first `{...}` group with a top-level comma or a `..` range,
/// recursively; anything else stays literal.
pub fn brace_expand(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '{' => {
                if let Some(close) = matching_brace(&chars, i) {
                    let inner: String = chars[i + 1..close].iter().collect();
                    let head: String = chars[..i].iter().collect();
                    let tail: String = chars[close + 1..].iter().collect();
                    let options = split_top_level(&inner)
                        .filter(|parts| parts.len() > 1)
                        .or_else(|| range(&inner));
                    if let Some(options) = options {
                        let mut out = Vec::new();
                        for option in options {
                            for expanded in brace_expand(&format!("{head}{option}{tail}")) {
                                out.push(expanded);
                            }
                        }
                        return out;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    vec![pattern.to_string()]
}

fn matching_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The comma-separated parts of a brace body, nested braces kept whole.
fn split_top_level(inner: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    Some(parts)
}

fn step_range(a: i64, b: i64, step: i64) -> Vec<i64> {
    let delta = if a < b { step } else { -step };
    let passed = |n: i64| if a < b { n > b } else { n < b };
    let mut out = Vec::new();
    let mut n = a;
    loop {
        out.push(n);
        if n == b {
            break;
        }
        n += delta;
        if passed(n) {
            break;
        }
    }
    out
}

fn int_range(start: &str, end: &str, step: i64) -> Option<Vec<String>> {
    let (a, b) = (start.parse::<i64>().ok()?, end.parse::<i64>().ok()?);
    let padded = |s: &str| {
        let stripped = s.trim_start_matches('-');
        stripped.len() > 1 && stripped.starts_with('0')
    };
    let width = if padded(start) || padded(end) {
        start.len().max(end.len())
    } else {
        0
    };
    let values = step_range(a, b, step)
        .into_iter()
        .map(|n| {
            if width > 0 {
                format!("{n:0width$}")
            } else {
                n.to_string()
            }
        })
        .collect();
    Some(values)
}

fn char_range(start: &str, end: &str, step: i64) -> Option<Vec<String>> {
    let (mut a, mut b) = (start.chars(), end.chars());
    let (Some(a), None, Some(b), None) = (a.next(), a.next(), b.next(), b.next()) else {
        return None;
    };
    step_range(a as i64, b as i64, step)
        .into_iter()
        .map(|n| char::from_u32(n as u32).map(|c| c.to_string()))
        .collect()
}

/// `{x..y}` / `{x..y..step}` over integers (zero-padded when an end is) or single characters.
fn range(inner: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let step = match parts.get(2) {
        Some(step) => step.parse::<i64>().ok()?.unsigned_abs().max(1) as i64,
        None => 1,
    };
    let (start, end) = (parts[0], parts[1]);
    int_range(start, end, step).or_else(|| char_range(start, end, step))
}

fn match_path(text: &str, pattern: &str, nocase: bool) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    let parts: Vec<&str> = pattern.split('/').collect();
    let files: Vec<&str> = text.split('/').collect();
    match_parts(&files, &parts, nocase)
}

/// `matchOne`: segment by segment; `**` takes zero or more segments, never one starting with `.`.
fn match_parts(files: &[&str], parts: &[&str], nocase: bool) -> bool {
    match parts.split_first() {
        None => files.is_empty(),
        Some((&"**", rest)) => {
            for skip in 0..=files.len() {
                if match_parts(&files[skip..], rest, nocase) {
                    return true;
                }
                match files.get(skip) {
                    Some(f) if !f.starts_with('.') => {}
                    _ => return false,
                }
            }
            false
        }
        Some((part, rest)) => match files.split_first() {
            Some((file, files)) => match_segment(file, part, nocase) && match_parts(files, rest, nocase),
            None => false,
        },
    }
}

#[derive(Debug)]
enum Node {
    Lit(char),
    Any,
    Star,
    Class { negated: bool, items: Vec<ClassItem> },
    Ext { kind: char, alternatives: Vec<Vec<Node>> },
}

#[derive(Debug)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Posix(String),
}

fn match_segment(text: &str, pattern: &str, nocase: bool) -> bool {
    let nodes = parse(&pattern.chars().collect::<Vec<_>>());
    let text: Vec<char> = text.chars().collect();
    if text.is_empty() {
        return nodes.is_empty();
    }
    if text[0] == '.' && !matches!(nodes.first(), Some(Node::Lit('.'))) {
        return false;
    }
    match_nodes(&text, &nodes, nocase)
}

fn parse_ext(chars: &[char], i: usize) -> Option<(Node, usize)> {
    let c = chars[i];
    if !matches!(c, '+' | '*' | '?' | '@' | '!') || chars.get(i + 1) != Some(&'(') {
        return None;
    }
    let close = closing_paren(chars, i + 1)?;
    let alternatives = split_alternatives(&chars[i + 2..close])
        .iter()
        .map(|alt| parse(alt))
        .collect();
    Some((Node::Ext { kind: c, alternatives }, close + 1))
}

fn parse_node(chars: &[char], i: usize, nodes: &mut Vec<Node>) -> usize {
    let c = chars[i];
    match c {
        '\\' if i + 1 < chars.len() => {
            nodes.push(Node::Lit(chars[i + 1]));
            i + 2
        }
        '*' => {
            if !matches!(nodes.last(), Some(Node::Star)) {
                nodes.push(Node::Star);
            }
            i + 1
        }
        '?' => {
            nodes.push(Node::Any);
            i + 1
        }
        '[' => match parse_class(chars, i) {
            Some((node, next)) => {
                nodes.push(node);
                next
            }
            None => {
                nodes.push(Node::Lit('['));
                i + 1
            }
        },
        _ => {
            nodes.push(Node::Lit(c));
            i + 1
        }
    }
}

fn parse(chars: &[char]) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some((node, next)) = parse_ext(chars, i) {
            nodes.push(node);
            i = next;
        } else {
            i = parse_node(chars, i, &mut nodes);
        }
    }
    nodes
}

fn closing_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_alternatives(chars: &[char]) -> Vec<Vec<char>> {
    let mut out = vec![Vec::new()];
    let mut depth = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                if let Some(last) = out.last_mut() {
                    last.extend([c, chars[i + 1]]);
                }
                i += 2;
                continue;
            }
            '(' => depth += 1,
            ')' => depth -= 1,
            '|' if depth == 0 => {
                out.push(Vec::new());
                i += 1;
                continue;
            }
            _ => {}
        }
        if let Some(last) = out.last_mut() {
            last.push(c);
        }
        i += 1;
    }
    out
}

fn parse_posix_class(chars: &[char], i: usize) -> Option<(ClassItem, usize)> {
    if chars.get(i) != Some(&'[') || chars.get(i + 1) != Some(&':') {
        return None;
    }
    let rest: String = chars[i + 2..].iter().collect();
    let end = rest.find(":]")?;
    let next = i + 2 + rest[..end].chars().count() + 2;
    Some((ClassItem::Posix(rest[..end].to_string()), next))
}

fn parse_class_char(chars: &[char], i: usize) -> Option<(char, usize)> {
    let c = *chars.get(i)?;
    if c == '\\' {
        Some((*chars.get(i + 1)?, i + 2))
    } else {
        Some((c, i + 1))
    }
}

fn parse_class_item(chars: &[char], i: usize) -> Option<(ClassItem, usize)> {
    if let Some(posix) = parse_posix_class(chars, i) {
        return Some(posix);
    }
    let (lo, next) = parse_class_char(chars, i)?;
    if chars.get(next) == Some(&'-') && chars.get(next + 1).is_some_and(|c| *c != ']') {
        let (hi, after) = parse_class_char(chars, next + 1)?;
        Some((ClassItem::Range(lo, hi), after))
    } else {
        Some((ClassItem::Char(lo), next))
    }
}

/// A `[...]` class starting at `open`, and the index after it; None when it never closes.
fn parse_class(chars: &[char], open: usize) -> Option<(Node, usize)> {
    let mut i = open + 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        if *chars.get(i)? == ']' && !first {
            return Some((Node::Class { negated, items }, i + 1));
        }
        first = false;
        let (item, next) = parse_class_item(chars, i)?;
        items.push(item);
        i = next;
    }
}

fn eq_char(a: char, b: char, nocase: bool) -> bool {
    a == b || nocase && a.to_lowercase().eq(b.to_lowercase())
}

type CharPredicate = fn(char) -> bool;

const POSIX_CLASSES: &[(&str, CharPredicate)] = &[
    ("alpha", char::is_alphabetic),
    ("digit", |c| c.is_ascii_digit()),
    ("alnum", char::is_alphanumeric),
    ("upper", char::is_uppercase),
    ("lower", char::is_lowercase),
    ("space", char::is_whitespace),
    ("punct", |c| c.is_ascii_punctuation()),
    ("xdigit", |c| c.is_ascii_hexdigit()),
    ("word", |c| c.is_alphanumeric() || c == '_'),
    ("blank", |c| c == ' ' || c == '\t'),
    ("cntrl", char::is_control),
    ("print", |c| !c.is_control()),
    ("graph", |c| !c.is_control() && !c.is_whitespace()),
    ("ascii", |c| c.is_ascii()),
];

fn posix_has(name: &str, c: char, nocase: bool) -> bool {
    if nocase && matches!(name, "upper" | "lower") {
        return c.is_alphabetic();
    }
    POSIX_CLASSES
        .iter()
        .find(|(n, _)| *n == name)
        .is_some_and(|(_, f)| f(c))
}

fn class_has(items: &[ClassItem], c: char, nocase: bool) -> bool {
    let variants = if nocase {
        vec![c, c.to_ascii_lowercase(), c.to_ascii_uppercase()]
    } else {
        vec![c]
    };
    items.iter().any(|item| match item {
        ClassItem::Char(x) => eq_char(*x, c, nocase),
        ClassItem::Range(lo, hi) => variants.iter().any(|v| lo <= v && v <= hi),
        ClassItem::Posix(name) => posix_has(name, c, nocase),
    })
}

fn match_ext(text: &[char], rest: &[Node], kind: char, alternatives: &[Vec<Node>], nocase: bool) -> bool {
    let one = |chunk: &[char]| alternatives.iter().any(|alt| match_nodes(chunk, alt, nocase));
    match kind {
        '@' => (0..=text.len()).any(|n| one(&text[..n]) && match_nodes(&text[n..], rest, nocase)),
        '?' => {
            match_nodes(text, rest, nocase)
                || (0..=text.len()).any(|n| one(&text[..n]) && match_nodes(&text[n..], rest, nocase))
        }
        '+' => repeat(text, rest, &one, false, nocase),
        '*' => repeat(text, rest, &one, true, nocase),
        _ => (0..=text.len()).any(|n| !one(&text[..n]) && match_nodes(&text[n..], rest, nocase)),
    }
}

fn match_nodes(text: &[char], nodes: &[Node], nocase: bool) -> bool {
    let Some((node, rest)) = nodes.split_first() else {
        return text.is_empty();
    };
    match node {
        Node::Lit(c) => text.first().is_some_and(|t| eq_char(*t, *c, nocase)) && match_nodes(&text[1..], rest, nocase),
        Node::Any => !text.is_empty() && match_nodes(&text[1..], rest, nocase),
        Node::Class { negated, items } => {
            text.first().is_some_and(|t| class_has(items, *t, nocase) != *negated)
                && match_nodes(&text[1..], rest, nocase)
        }
        Node::Star => (0..=text.len()).any(|n| match_nodes(&text[n..], rest, nocase)),
        Node::Ext { kind, alternatives } => match_ext(text, rest, *kind, alternatives, nocase),
    }
}

/// One or more (zero or more with `empty`) chunks matching an alternative, then `rest`.
fn repeat(text: &[char], rest: &[Node], one: &dyn Fn(&[char]) -> bool, empty: bool, nocase: bool) -> bool {
    if empty && match_nodes(text, rest, nocase) {
        return true;
    }
    (1..=text.len()).any(|n| one(&text[..n]) && repeat(&text[n..], rest, one, true, nocase))
}
