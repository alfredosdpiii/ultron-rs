const JS_SPACE: &str =
    r"\t\n\x0B\x0C\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";
const LINE_TERMINATORS: &str = r"\n\r\u{2028}\u{2029}";

const CLASS_ESCAPES: &[(char, &str, &str)] = &[
    ('w', "[0-9A-Za-z_]", "0-9A-Za-z_"),
    ('W', "[^0-9A-Za-z_]", "[^0-9A-Za-z_]"),
    ('d', "[0-9]", "0-9"),
    ('D', "[^0-9]", "[^0-9]"),
    ('/', "/", "/"),
];

fn lookup_escape_table(next: char, in_class: bool) -> Option<String> {
    if let Some(&(_, outside, inside)) = CLASS_ESCAPES.iter().find(|&&(ch, _, _)| ch == next) {
        return Some((if in_class { inside } else { outside }).to_string());
    }
    match next {
        's' => Some(if in_class {
            JS_SPACE.to_string()
        } else {
            format!("[{JS_SPACE}]")
        }),
        'S' => Some(format!("[^{JS_SPACE}]")),
        _ => None,
    }
}

fn translate_js_escape(chars: &[char], i: &mut usize, in_class: bool, word_b: &str, not_word_b: &str) -> String {
    let next = chars[*i + 1];
    *i += 2;
    if let Some(mapped) = lookup_escape_table(next, in_class) {
        return mapped;
    }
    match (next, in_class) {
        ('b', false) => word_b.to_string(),
        ('b', true) => r"\x08".into(),
        ('B', false) => not_word_b.to_string(),
        ('u', _) if *i + 4 <= chars.len() && chars[*i..*i + 4].iter().all(char::is_ascii_hexdigit) => {
            let hex: String = chars[*i..*i + 4].iter().collect();
            *i += 4;
            format!(r"\u{{{hex}}}")
        }
        (ch, _) if ch.is_ascii_alphanumeric() => format!("\\{ch}"),
        (ch, _) => fancy_regex::escape(&ch.to_string()).into_owned(),
    }
}

fn push_in_class_char(c: char, out: &mut String, in_class: &mut bool) {
    match c {
        ']' => {
            *in_class = false;
            out.push(']');
        }
        '[' => out.push_str(r"\["),
        '&' | '~' => {
            out.push('\\');
            out.push(c);
        }
        _ => out.push(c),
    }
}

fn push_open_bracket(chars: &[char], i: &mut usize, out: &mut String, in_class: &mut bool) {
    *in_class = true;
    out.push('[');
    if chars.get(*i) == Some(&'^') {
        out.push('^');
        *i += 1;
    }
    if chars.get(*i) == Some(&']') {
        *i += 1;
        *in_class = false;
        if out.ends_with("[^") {
            out.truncate(out.len() - 2);
            out.push_str("(?s:.)");
        } else {
            out.truncate(out.len() - 1);
            out.push_str("(?!)");
        }
    }
}

fn is_open_quantifier(chars: &[char], i: usize) -> bool {
    let rest: String = chars[i..].iter().collect();
    let Some(close) = rest.find('}') else {
        return false;
    };
    let body = &rest[..close];
    let mut parts = body.splitn(2, ',');
    let first = parts.next().unwrap_or("");
    !first.is_empty()
        && first.chars().all(|c| c.is_ascii_digit())
        && parts.next().is_none_or(|s| s.chars().all(|c| c.is_ascii_digit()))
}

fn push_close_brace(out: &mut String) {
    if out.ends_with(|ch: char| ch.is_ascii_digit() || ch == ',') && quantifier_open(out) {
        out.push('}');
    } else {
        out.push_str(r"\}");
    }
}

fn push_outside_char(c: char, chars: &[char], i: &mut usize, out: &mut String, in_class: &mut bool) {
    match c {
        '[' => push_open_bracket(chars, i, out, in_class),
        '.' => out.push_str(&format!("[^{LINE_TERMINATORS}]")),
        '^' => out.push_str(r"(?:\A|(?<=\n)|(?<=\r)|(?<=\u{2028})|(?<=\u{2029}))"),
        '$' => out.push_str(&format!(r"(?=[{LINE_TERMINATORS}]|\z)")),
        '{' => out.push_str(if is_open_quantifier(chars, *i) { "{" } else { r"\{" }),
        '}' => push_close_brace(out),
        _ => out.push(c),
    }
}

/// Translate a JavaScript regex source compiled with the `m` flag (and `i` when `case_insensitive`) to an
/// equivalent fancy-regex pattern.
pub fn js_regex(src: &str, case_insensitive: bool) -> String {
    let word_b = "(?:(?<=[0-9A-Za-z_])(?![0-9A-Za-z_])|(?<![0-9A-Za-z_])(?=[0-9A-Za-z_]))";
    let not_word_b = "(?:(?<=[0-9A-Za-z_])(?=[0-9A-Za-z_])|(?<![0-9A-Za-z_])(?![0-9A-Za-z_]))";
    let mut out = String::from(if case_insensitive { "(?i)" } else { "" });
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut in_class = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let piece = translate_js_escape(&chars, &mut i, in_class, word_b, not_word_b);
            out.push_str(&piece);
            continue;
        }
        i += 1;
        if in_class {
            push_in_class_char(c, &mut out, &mut in_class);
        } else {
            push_outside_char(c, &chars, &mut i, &mut out, &mut in_class);
        }
    }
    out
}

/// Whether the translated pattern ends inside an open `{n,m` quantifier.
fn quantifier_open(out: &str) -> bool {
    let tail = out.trim_end_matches(|c: char| c.is_ascii_digit() || c == ',');
    tail.ends_with('{') && !tail.ends_with(r"\{")
}
