//! The string formats TypeBox registers (`typebox/build/format`), as `Check` and `prepareToolArguments` test them: the same
//! regular expressions (ASCII, case-insensitive where TypeBox's are) and the same procedural checks (dates and times,
//! IRIs through WHATWG URL parsing, hostnames and IDN hostnames label by label, punycode). An unknown format passes.
//! Also the ECMAScript (`u` flag) regular expressions of `pattern`, approximated on fancy-regex.

use regex::bytes::{Regex, RegexBuilder};
use std::sync::OnceLock;
use url::Url;

fn ascii(pattern: &str, case_insensitive: bool) -> Regex {
    RegexBuilder::new(pattern)
        .unicode(false)
        .case_insensitive(case_insensitive)
        .build()
        .expect("TypeBox format pattern")
}

macro_rules! format_regex {
    ($name:ident, $pattern:expr, $insensitive:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| ascii($pattern, $insensitive))
        }
    };
}

const IPV4: &str = r"(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)";
const IPV6_BODY: &str = r"(?:(?:(?:[0-9a-f]{1,4}:){6}|::(?:[0-9a-f]{1,4}:){5}|(?:[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){4}|(?:(?:[0-9a-f]{1,4}:)?[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){3}|(?:(?:[0-9a-f]{1,4}:){0,2}[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){2}|(?:(?:[0-9a-f]{1,4}:){0,3}[0-9a-f]{1,4})?::[0-9a-f]{1,4}:|(?:(?:[0-9a-f]{1,4}:){0,4}[0-9a-f]{1,4})?::)(?:[0-9a-f]{1,4}:[0-9a-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|(?:(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4})?::[0-9a-f]{1,4}|(?:(?:[0-9a-f]{1,4}:){0,6}[0-9a-f]{1,4})?::)";
/// The IPv6 literal inside `uri` and `uri-reference` (TypeBox's `[\da-f]` spelling of the same grammar).
const URI_IPV6: &str = r"(?:(?:[\da-f]{1,4}:){6}(?:[\da-f]{1,4}:[\da-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|::(?:[\da-f]{1,4}:){5}(?:[\da-f]{1,4}:[\da-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|(?:[\da-f]{1,4})?::(?:[\da-f]{1,4}:){4}(?:[\da-f]{1,4}:[\da-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|(?:(?:[\da-f]{1,4}:){0,1}[\da-f]{1,4})?::(?:[\da-f]{1,4}:){3}(?:[\da-f]{1,4}:[\da-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|(?:(?:[\da-f]{1,4}:){0,2}[\da-f]{1,4})?::(?:[\da-f]{1,4}:){2}(?:[\da-f]{1,4}:[\da-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|(?:(?:[\da-f]{1,4}:){0,3}[\da-f]{1,4})?::[\da-f]{1,4}:(?:[\da-f]{1,4}:[\da-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|(?:(?:[\da-f]{1,4}:){0,4}[\da-f]{1,4})?::(?:[\da-f]{1,4}:[\da-f]{1,4}|(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d))|(?:(?:[\da-f]{1,4}:){0,5}[\da-f]{1,4})?::[\da-f]{1,4}|(?:(?:[\da-f]{1,4}:){0,6}[\da-f]{1,4})?::)|v[0-9a-f]+\.[-a-z0-9._~!$&'()*+,;=:]+";
const PCHAR: &str = r"(?:[-a-z0-9._~!$&'()*+,;=:@]|%[0-9a-f]{2})";

/// The part after `scheme:` (or the whole relative reference): authority and path, or a path. `colon`: the first
/// segment of a rootless path may hold `:` (not in a relative reference).
fn authority_path(colon: bool) -> String {
    let ipv4 = r"(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)";
    let first = if colon {
        PCHAR.to_string()
    } else {
        r"(?:[-a-z0-9._~!$&'()*+,;=@]|%[0-9a-f]{2})".to_string()
    };
    format!(
        r"(?://(?:(?:[-a-z0-9._~!$&'()*+,;=:]|%[0-9a-f]{{2}})*@)?(?:\[(?:{URI_IPV6})\]|{ipv4}|(?:[-a-z0-9._~!$&'()*+,;=]|%[0-9a-f]{{2}})*)(?::\d*)?(?:/{PCHAR}*)*|/(?:{PCHAR}+(?:/{PCHAR}*)*)?|{first}+(?:/{PCHAR}*)*)"
    )
}

const QUERY_FRAGMENT: &str =
    r"(?:\?(?:[-a-z0-9._~!$&'()*+,;=:@/?]|%[0-9a-f]{2})*)?(?:#(?:[-a-z0-9._~!$&'()*+,;=:@/?]|%[0-9a-f]{2})*)?";

fn uri() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        ascii(
            &format!(r"^[a-z][a-z0-9+\-.]*:{}{QUERY_FRAGMENT}$", authority_path(true)),
            true,
        )
    })
}

fn uri_reference() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        ascii(
            &format!(
                r"^(?:[a-z][a-z0-9+\-.]*:{}|{}?){QUERY_FRAGMENT}$",
                authority_path(true),
                authority_path(false)
            ),
            true,
        )
    })
}

format_regex!(
    email,
    r#"^(?:[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*|"(?:[^"\\]|\\[\x20-\x7e])*")@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*|\[(?:IPv6:[a-f0-9:]+|(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])){3})\])$"#,
    true
);
format_regex!(
    duration,
    r"^P((\d+Y(\d+M(\d+D)?)?|\d+M(\d+D)?|\d+D)(T(\d+H(\d+M(\d+S)?)?|\d+M(\d+S)?|\d+S))?|T(\d+H(\d+M(\d+S)?)?|\d+M(\d+S)?|\d+S)|\d+W)$",
    false
);
format_regex!(json_pointer, r"^(?:/(?:[^~/]|~0|~1)*)*$", false);
format_regex!(
    json_pointer_fragment,
    r"^#(?:/(?:[a-z0-9_\-.!$&'()*+,;:=@]|%[0-9a-f]{2}|~0|~1)*)*$",
    true
);
format_regex!(
    relative_json_pointer,
    r"^(?:0|[1-9][0-9]*)(?:#|(?:/(?:[^~/]|~0|~1)*)*)$",
    false
);
format_regex!(
    uri_template,
    r#"^(?:(?:[^\x00-\x20"<>%\\^`{|}\x7f]|%[0-9a-f]{2})|\{[+#./;?&=,!@|]?(?:[a-z0-9_]|%[0-9a-f]{2})+(?:\.(?:[a-z0-9_]|%[0-9a-f]{2})+)*(?::[1-9]\d{0,3}|\*)?(?:,(?:[a-z0-9_]|%[0-9a-f]{2})+(?:\.(?:[a-z0-9_]|%[0-9a-f]{2})+)*(?::[1-9]\d{0,3}|\*)?)*\})*$"#,
    true
);
format_regex!(uuid, r"^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$", true);

fn ipv4() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| ascii(&format!("^{IPV4}$"), false))
}

fn ipv6() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| ascii(&format!("^{IPV6_BODY}$"), true))
}

fn digits(text: &str) -> Option<u32> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().unwrap_or(u32::MAX))
}

/// `IsDate`: `YYYY-MM-DD` with a real day of the month.
pub fn is_date(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let (Some(year), Some(month), Some(day)) = (digits(&value[..4]), digits(&value[5..7]), digits(&value[8..])) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    const DAYS: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    (1..=12).contains(&month) && day >= 1 && day <= if month == 2 && leap { 29 } else { DAYS[month as usize] }
}

fn strip_fraction(rest: &str) -> Option<&str> {
    let Some(fraction) = rest.strip_prefix('.') else {
        return Some(rest);
    };
    let n = fraction.bytes().take_while(u8::is_ascii_digit).count();
    (n > 0).then(|| &fraction[n..])
}

fn parse_hms_digits(value: &str) -> Option<(u32, u32, u32)> {
    let b = value.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' || !value.is_char_boundary(8) {
        return None;
    }
    Some((digits(&value[..2])?, digits(&value[3..5])?, digits(&value[6..8])?))
}

fn parse_hms(value: &str) -> Option<(u32, u32, u32, &str)> {
    let (hr, min, sec) = parse_hms_digits(value)?;
    if hr > 23 || min > 59 || sec > 60 {
        return None;
    }
    Some((hr, min, sec, strip_fraction(&value[8..])?))
}

fn parse_numeric_tz(negative: bool, hours: &str, mins: &str) -> Option<i64> {
    let h = digits(hours)?;
    let m = digits(mins)?;
    if h > 23 || m > 59 {
        return None;
    }
    let sign = if negative { -1 } else { 1 };
    Some(sign * i64::from(h * 60 + m))
}

fn parse_tz_offset_minutes(rest: &str) -> Option<i64> {
    if rest.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    let r = rest.as_bytes();
    if r.len() != 6 || (r[0] != b'+' && r[0] != b'-') || r[3] != b':' {
        return None;
    }
    parse_numeric_tz(r[0] == b'-', &rest[1..3], &rest[4..6])
}

/// `IsTime` (time zone required): leap seconds only at 23:59:60 UTC.
pub fn is_time(value: &str) -> bool {
    let Some((hr, min, sec, rest)) = parse_hms(value) else {
        return false;
    };
    let Some(offset) = parse_tz_offset_minutes(rest) else {
        return false;
    };
    if sec < 60 {
        return true;
    }
    let total = i64::from(hr * 60 + min) - offset;
    total.rem_euclid(1440) == 1439
}

/// `IsDateTime`: a date and a time split at one `T` (either case).
pub fn is_date_time(value: &str) -> bool {
    let parts: Vec<&str> = value.split(['T', 't']).collect();
    parts.len() == 2 && is_date(parts[0]) && is_time(parts[1])
}

/// `URL.canParse(value)` (and with a base).
fn can_parse(value: &str, base: Option<&str>) -> bool {
    match base {
        None => Url::parse(value).is_ok(),
        Some(base) => Url::parse(base).and_then(|b| b.join(value)).is_ok(),
    }
}

/// `%` not followed by two hex digits.
fn bad_percent(value: &str) -> bool {
    let b = value.as_bytes();
    b.iter().enumerate().any(|(i, c)| {
        *c == b'%'
            && !(b.get(i + 1).is_some_and(u8::is_ascii_hexdigit) && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit))
    })
}

fn is_iri(value: &str) -> bool {
    if value.chars().any(|c| (c as u32) <= 0x20 || "<>^`{|}\\".contains(c)) || bad_percent(value) {
        return false;
    }
    let narrowed = if value.encode_utf16().count() < 2048 {
        narrow_ipv_future(value)
    } else {
        value.to_string()
    };
    can_parse(&narrowed, None)
}

fn narrow_ipv_future(value: &str) -> String {
    let bytes = value.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b'[' || !matches!(bytes.get(start + 1), Some(b'v' | b'V')) {
            continue;
        }
        let hex = bytes[start + 2..].iter().take_while(|b| b.is_ascii_hexdigit()).count();
        let dot = start + 2 + hex;
        if hex == 0 || bytes.get(dot) != Some(&b'.') {
            continue;
        }
        let body = bytes[dot + 1..].iter().take_while(|b| **b != b']').count();
        if body == 0 || bytes.get(dot + 1 + body) != Some(&b']') {
            continue;
        }
        return format!("{}[::1]{}", &value[..start], &value[dot + 2 + body..]);
    }
    value.to_string()
}

fn is_iri_reference(value: &str) -> bool {
    let invalid = value.chars().any(|c| (c as u32) <= 0x20 || c == '\u{7f}' || c == '\\') || bad_percent(value);
    let scheme = value.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && {
        let rest: String = value
            .chars()
            .skip(1)
            .skip_while(|c| c.is_ascii_alphanumeric() || "+-.".contains(*c))
            .collect();
        rest.starts_with("//")
    };
    !invalid && !scheme && can_parse(value, Some("http://example.com"))
}

/// `IsRegex`: an ECMAScript regular expression with the `u` flag (approximated on fancy-regex).
fn is_regex(value: &str) -> bool {
    js_regex(value).is_some()
}

macro_rules! js_space_str {
    () => {
        r"\t\n\x0B\x0C\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}"
    };
}

const JS_SPACE: &str = js_space_str!();
const JS_SPACE_CLASS: &str = concat!("[", js_space_str!(), "]");
const JS_NON_SPACE_CLASS: &str = concat!("[^", js_space_str!(), "]");

fn js_class_escape(next: char) -> Option<(&'static str, &'static str)> {
    match next {
        'd' => Some(("0-9", "[0-9]")),
        'D' => Some(("", "[^0-9]")),
        'w' => Some(("A-Za-z0-9_", "[A-Za-z0-9_]")),
        'W' => Some(("", "[^A-Za-z0-9_]")),
        's' => Some((JS_SPACE, JS_SPACE_CLASS)),
        'S' => Some(("", JS_NON_SPACE_CLASS)),
        _ => None,
    }
}

fn js_boundary_escape(next: char) -> Option<&'static str> {
    match next {
        'b' => Some(r"(?:(?<=[A-Za-z0-9_])(?![A-Za-z0-9_])|(?<![A-Za-z0-9_])(?=[A-Za-z0-9_]))"),
        'B' => Some(r"(?:(?<=[A-Za-z0-9_])(?=[A-Za-z0-9_])|(?<![A-Za-z0-9_])(?![A-Za-z0-9_]))"),
        _ => None,
    }
}

fn push_js_escape(out: &mut String, next: char, in_class: bool) {
    if !in_class && let Some(boundary) = js_boundary_escape(next) {
        out.push_str(boundary);
        return;
    }
    if let Some((inside, outside)) = js_class_escape(next) {
        if !in_class {
            out.push_str(outside);
        } else if !inside.is_empty() {
            out.push_str(inside);
        } else {
            out.push('\\');
            out.push(next);
        }
        return;
    }
    out.push('\\');
    out.push(next);
}

fn push_backslash_escape(out: &mut String, next: Option<char>, in_class: bool) {
    match next {
        Some(c) => push_js_escape(out, c, in_class),
        None => out.push('\\'),
    }
}

fn open_js_class(out: &mut String, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let negated = chars.next_if_eq(&'^').is_some();
    if chars.next_if_eq(&']').is_some() {
        out.push_str(if negated { r"[\s\S]" } else { "(?!)" });
        return false;
    }
    out.push('[');
    if negated {
        out.push('^');
    }
    true
}

fn step_js_regex(c: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String, in_class: &mut bool) {
    if c == '\\' {
        push_backslash_escape(out, chars.next(), *in_class);
    } else if c == '[' && !*in_class {
        *in_class = open_js_class(out, chars);
    } else if c == ']' && *in_class {
        *in_class = false;
        out.push(c);
    } else if c == '.' && !*in_class {
        out.push_str(r"[^\n\r\u{2028}\u{2029}]");
    } else {
        out.push(c);
    }
}

/// `pattern`'s regular expression: `new RegExp(pattern, "u")`, translated to fancy-regex: `\d`, `\w`, `\s` and `\b` keep
/// their ECMAScript meaning (ASCII digits and word characters, ECMAScript white space).
pub fn js_regex(pattern: &str) -> Option<fancy_regex::Regex> {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        step_js_regex(c, &mut chars, &mut out, &mut in_class);
    }
    if quantifier_out_of_order(pattern) {
        return None;
    }
    fancy_regex::Regex::new(&out).ok()
}

/// `{m,n}` with `m > n`, a syntax error in ECMAScript ("numbers out of order") that fancy-regex accepts.
fn quantifier_out_of_order(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'{' => {
                let rest = &pattern[i + 1..];
                if let Some(end) = rest.find('}')
                    && let Some((low, high)) = rest[..end].split_once(',')
                    && let (Ok(low), Ok(high)) = (low.parse::<u64>(), high.parse::<u64>())
                    && low > high
                {
                    return true;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

/// `IsHostname`: ASCII or punycode labels of 1-63 characters, at most 253 in all, no trailing dot.
pub fn is_hostname(value: &str) -> bool {
    let len = value.encode_utf16().count();
    if len == 0 || len > 253 || value.ends_with('.') {
        return false;
    }
    value.split('.').all(|label| {
        let len = label.encode_utf16().count();
        (1..=63).contains(&len) && (is_puny_label(label) || is_ascii_label(label))
    })
}

fn is_ascii_label(label: &str) -> bool {
    !label.starts_with('-')
        && !label.ends_with('-')
        && !(label.chars().nth(2) == Some('-') && label.chars().nth(3) == Some('-'))
        && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_ace_prefixed(label: &str) -> bool {
    label.to_lowercase().starts_with("xn--")
}

fn is_puny_label(label: &str) -> bool {
    if !is_ace_prefixed(label) {
        return false;
    }
    let body: String = label.chars().skip(4).collect::<String>().to_lowercase();
    if body.rfind('-') == Some(0) {
        return false;
    }
    match puny_decode(&body) {
        Some(decoded) if !decoded.is_ascii() => is_unicode_label(&decoded),
        _ => false,
    }
}

const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;

fn adapt(delta: u32, points: u32, first: bool) -> u32 {
    let mut delta = if first { delta / 700 } else { delta >> 1 };
    delta += delta / points;
    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) >> 1 {
        delta /= BASE - TMIN;
        k += BASE;
    }
    k + ((BASE - TMIN + 1) * delta) / (delta + 38)
}

fn decode_basic_prefix(units: &[u16]) -> Option<(Vec<u32>, usize)> {
    let Some(delim) = units.iter().rposition(|u| *u == u16::from(b'-')) else {
        return Some((Vec::new(), 0));
    };
    if delim == 0 {
        return Some((Vec::new(), 1));
    }
    let mut output = Vec::with_capacity(delim);
    for &unit in &units[..delim] {
        if unit >= 128 {
            return None;
        }
        output.push(u32::from(unit));
    }
    Some((output, delim + 1))
}

fn puny_digit(ch: u16) -> Option<u32> {
    match ch {
        0x61..=0x7a => Some(u32::from(ch) - 0x61),
        0x30..=0x39 => Some(u32::from(ch) - 0x30 + 26),
        _ => None,
    }
}

fn puny_threshold(k: u32, bias: u32) -> u32 {
    k.saturating_sub(bias).clamp(TMIN, TMAX)
}

fn decode_delta(units: &[u16], index: &mut usize, i: &mut u32, bias: u32) -> Option<()> {
    let (mut w, mut k) = (1u32, BASE);
    loop {
        let ch = *units.get(*index)?;
        *index += 1;
        let digit = puny_digit(ch)?;
        *i = i.checked_add(digit.checked_mul(w)?)?;
        let t = puny_threshold(k, bias);
        if digit < t {
            return Some(());
        }
        w = w.checked_mul(BASE - t)?;
        k += BASE;
    }
}

/// `Decode` of TypeBox's punycode (lowercase digits only).
fn puny_decode(value: &str) -> Option<String> {
    let units: Vec<u16> = value.encode_utf16().collect();
    let (mut output, mut index) = decode_basic_prefix(&units)?;
    let (mut n, mut i, mut bias) = (128u32, 0u32, 72u32);
    while index < units.len() {
        let old = i;
        decode_delta(&units, &mut index, &mut i, bias)?;
        let len = output.len() as u32 + 1;
        bias = adapt(i - old, len, old == 0);
        n = n.checked_add(i / len)?;
        i %= len;
        output.insert(i as usize, n);
        i += 1;
    }
    output.into_iter().map(char::from_u32).collect()
}

/// `Encode` of TypeBox's punycode (its length bounds a Unicode label).
fn puny_encode(input: &str) -> String {
    let basic: String = input.chars().filter(char::is_ascii).collect();
    let mut result = basic.clone();
    if !basic.is_empty() {
        result.push('-');
    }
    let points: Vec<u32> = input.chars().map(u32::from).collect();
    let (mut n, mut delta, mut bias) = (128u32, 0u32, 72u32);
    let mut handled = basic.chars().count() as u32;
    let digit = |d: u32| char::from_u32(if d < 26 { d + 0x61 } else { d - 26 + 0x30 }).unwrap_or('a');
    while (handled as usize) < points.len() {
        let m = points.iter().copied().filter(|p| *p >= n).min().unwrap_or(n);
        delta = delta.saturating_add((m - n).saturating_mul(handled + 1));
        n = m;
        for p in &points {
            if *p < n {
                delta = delta.saturating_add(1);
            }
            if *p == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = puny_threshold(k, bias);
                    if q < t {
                        break;
                    }
                    result.push(digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                result.push(digit(q));
                bias = adapt(delta, handled + 1, handled == basic.chars().count() as u32);
                delta = 0;
                handled += 1;
            }
        }
        delta = delta.saturating_add(1);
        n += 1;
    }
    result
}

fn class(pattern: &'static str) -> impl Fn(char) -> bool {
    move |c: char| {
        static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<&'static str, regex::Regex>>> =
            OnceLock::new();
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let re = cache
            .entry(pattern)
            .or_insert_with(|| regex::Regex::new(&format!("^{pattern}$")).expect("Unicode class"));
        re.is_match(c.encode_utf8(&mut [0; 4]))
    }
}

/// `GetBidiClass`.
fn bidi_class(c: char) -> &'static str {
    if c.is_ascii_digit() || ('\u{06f0}'..='\u{06f9}').contains(&c) {
        "EN"
    } else if ('\u{0660}'..='\u{0669}').contains(&c) {
        "AN"
    } else if class(r"\p{Mn}")(c) {
        "NSM"
    } else if class(r"\p{Script=Hebrew}")(c) {
        "R"
    } else if class(r"[\p{Script=Arabic}\p{Script=Syriac}\p{Script=Thaana}\p{Script=Mandaic}]")(c) {
        "AL"
    } else if class(r"\p{L}")(c) {
        "L"
    } else {
        "ON"
    }
}

fn has_rtl(value: &str) -> bool {
    value.chars().any(|c| matches!(bidi_class(c), "R" | "AL" | "AN"))
}

fn has_bidi_chars(label: &str) -> bool {
    if is_ace_prefixed(label) {
        let body: String = label.chars().skip(4).collect::<String>().to_lowercase();
        return puny_decode(&body).is_some_and(|d| has_rtl(&d));
    }
    has_rtl(label)
}

fn satisfies_bidi_rule(value: &str) -> bool {
    let mut rtl = false;
    let (mut saw_en, mut saw_an) = (false, false);
    for (index, c) in value.chars().enumerate() {
        let class = bidi_class(c);
        if index == 0 {
            if !matches!(class, "L" | "R" | "AL") {
                return false;
            }
            rtl = matches!(class, "R" | "AL");
        }
        let allowed: &[&str] = if rtl {
            &["R", "AL", "AN", "EN", "ES", "CS", "ET", "ON", "BN", "NSM"]
        } else {
            &["L", "EN", "ES", "CS", "ET", "ON", "BN", "NSM"]
        };
        if !allowed.contains(&class) {
            return false;
        }
        saw_en |= class == "EN";
        saw_an |= class == "AN";
    }
    !(rtl && saw_en && saw_an)
}

const VIRAMA: &[u32] = &[
    0x094d, 0x09cd, 0x0a4d, 0x0acd, 0x0b4d, 0x0bcd, 0x0c4d, 0x0ccd, 0x0d3b, 0x0d3c, 0x0d4d, 0x0dca, 0x1b44, 0x1baa,
    0x1bab, 0xa9c0, 0x11046, 0x1107f, 0x110b9, 0x11133, 0x11134, 0x111c0, 0x11235, 0x1134d, 0x11442, 0x114c2, 0x115bf,
    0x1163f, 0x116b6, 0x11c3f, 0x11d44, 0x11d45,
];

fn has_valid_label_length_and_bidi(value: &str) -> bool {
    if !value.is_ascii() && puny_encode(value).encode_utf16().count() + 4 > 63 {
        return false;
    }
    !has_rtl(value) || satisfies_bidi_rule(value)
}

fn has_valid_label_edges(chars: &[char]) -> bool {
    if chars.first() == Some(&'-') || chars.last() == Some(&'-') || chars.get(2..4) == Some(&['-', '-']) {
        return false;
    }
    !chars.first().is_some_and(|c| class(r"[\p{Mn}\p{Mc}\p{Me}]")(*c))
}

fn is_permitted_label_char(c: char) -> bool {
    const DISALLOWED: &str = "\u{0640}\u{07fa}\u{302e}\u{302f}\u{3031}\u{3032}\u{3033}\u{3034}\u{3035}\u{303b}";
    const PERMITTED: &str = r"[\p{L}\-+.,:/\p{Nd}\p{Mn}\p{Mc}\u{00b7}\u{0375}\u{05f3}\u{05f4}\u{200c}\u{200d}\u{30fb}\u{00df}\u{03c2}\u{06fd}\u{06fe}\u{0f0b}\u{3007}]";
    !DISALLOWED.contains(c) && class(PERMITTED)(c)
}

fn is_zwnj_prev(p: char) -> bool {
    (p as u32) >= 0x80 || VIRAMA.contains(&(p as u32))
}

fn context_ok(c: char, prev: Option<char>, next: Option<char>) -> bool {
    match c {
        '\u{00b7}' => prev == Some('l') && next == Some('l'),
        '\u{0375}' => next.is_some_and(class(r"\p{Script=Greek}")),
        '\u{05f3}' | '\u{05f4}' => prev.is_some_and(class(r"\p{Script=Hebrew}")),
        '\u{200c}' => prev.is_some_and(is_zwnj_prev),
        '\u{200d}' => prev.is_some_and(|p| VIRAMA.contains(&(p as u32))),
        _ => true,
    }
}

/// `IsUnicodeLabel` (RFC 5891/5892 rules as TypeBox applies them).
fn is_unicode_label(value: &str) -> bool {
    if !has_valid_label_length_and_bidi(value) {
        return false;
    }
    let chars: Vec<char> = value.chars().collect();
    if !has_valid_label_edges(&chars) {
        return false;
    }
    let japanese = class(r"[\p{Script=Hiragana}\p{Script=Katakana}\p{Script=Han}]");
    let mut has_japanese = false;
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        if !is_permitted_label_char(c) || !context_ok(c, prev, next) {
            return false;
        }
        has_japanese |= japanese(c);
    }
    !(value.contains('\u{30fb}') && !has_japanese)
}

fn nfc(value: &str) -> String {
    icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
        .normalize(value)
        .into_owned()
}

pub fn is_idn_hostname(value: &str) -> bool {
    if value.is_empty() || value.contains(' ') {
        return false;
    }
    let widened: String = value
        .chars()
        .map(|c| match c {
            '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
            _ => c,
        })
        .collect();
    let normalized: String = nfc(&widened)
        .chars()
        .filter(|c| {
            !matches!(c, '\u{00ad}' | '\u{034f}' | '\u{180b}'..='\u{180d}' | '\u{200b}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
        })
        .map(|c| if matches!(c, '\u{3002}' | '\u{ff0e}' | '\u{ff61}') { '.' } else { c })
        .collect();
    if normalized.encode_utf16().count() > 253 {
        return false;
    }
    let labels: Vec<&str> = normalized.split('.').collect();
    let bidi = labels.iter().any(|l| has_bidi_chars(l));
    labels.iter().all(|label| {
        let len = label.encode_utf16().count();
        (1..=63).contains(&len)
            && (is_puny_label(label) || is_unicode_label(label))
            && (!bidi || satisfies_bidi_rule(label))
    })
}

fn is_idn_email(value: &str) -> bool {
    static RE: OnceLock<fancy_regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        fancy_regex::Regex::new(
            r#"(?i)^(?:[A-Za-z0-9!#$%&'*+/=?^_`{|}~\u{0080}-\u{10FFFF}-]+(?:\.[A-Za-z0-9!#$%&'*+/=?^_`{|}~\u{0080}-\u{10FFFF}-]+)*|"(?:[^"\\]|\\.)*")@[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,62})(?<!-)(?:\.[\p{L}\p{N}](?:[\p{L}\p{N}-]{0,62})(?<!-))*$"#,
        )
        .expect("idn-email pattern")
    })
    .is_match(&nfc(value))
    .unwrap_or(false)
}

type RegexFormat = (&'static str, fn() -> &'static Regex);
type StrFormat = (&'static str, fn(&str) -> bool);

const REGEX_FORMATS: &[RegexFormat] = &[
    ("duration", duration),
    ("email", email),
    ("ipv4", ipv4),
    ("ipv6", ipv6),
    ("json-pointer-uri-fragment", json_pointer_fragment),
    ("json-pointer", json_pointer),
    ("relative-json-pointer", relative_json_pointer),
    ("uri-reference", uri_reference),
    ("uri-template", uri_template),
    ("uri", uri),
    ("uuid", uuid),
];

fn is_url(value: &str) -> bool {
    can_parse(value, None)
}

const STR_FORMATS: &[StrFormat] = &[
    ("date-time", is_date_time),
    ("date", is_date),
    ("hostname", is_hostname),
    ("idn-email", is_idn_email),
    ("idn-hostname", is_idn_hostname),
    ("iri-reference", is_iri_reference),
    ("iri", is_iri),
    ("regex", is_regex),
    ("time", is_time),
    ("url", is_url),
];

/// Whether `value` matches TypeBox format `format` (an unknown format matches).
pub fn matches(format: &str, value: &str) -> bool {
    if let Some((_, re)) = REGEX_FORMATS.iter().find(|(name, _)| *name == format) {
        return re().is_match(value.as_bytes());
    }
    if let Some((_, check)) = STR_FORMATS.iter().find(|(name, _)| *name == format) {
        return check(value);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_time_leap_seconds() {
        assert!(is_time("23:59:60Z"));
        assert!(is_time("23:59:60z"));
        assert!(is_time("00:59:60+01:00"));
        assert!(is_time("18:59:60-05:00"));
        assert!(is_time("12:00:00-00:00"));
        assert!(!is_time("23:59:60+01:00"));
        assert!(!is_time("03:04:05.Z"));
        assert!(!is_time("00:00:00+00:\u{e9}"));
    }

    #[test]
    fn test_js_regex_classes() {
        let empty = js_regex("[]");
        assert!(empty.is_some());
        assert!(!empty.is_some_and(|re| re.is_match("a").unwrap_or(false)));

        let any_char = js_regex("[^]");
        assert!(any_char.is_some_and(|re| re.is_match("\n").unwrap_or(false)));

        let in_class = js_regex(r"[\D]");
        assert!(in_class.is_some_and(|re| re.is_match("a").unwrap_or(false)));
        assert!(!js_regex(r"[\D]").is_some_and(|re| re.is_match("5").unwrap_or(false)));

        let boundary = js_regex(r"\bfoo\b");
        assert!(boundary.is_some_and(|re| re.is_match("foo bar").unwrap_or(false)));
        let non_boundary = js_regex(r"\Bfoo\B");
        assert!(non_boundary.is_some_and(|re| re.is_match("afoob").unwrap_or(false)));

        assert!(js_regex(r"\").is_none());
        assert!(js_regex("[").is_none());
        assert!(js_regex("a{2,1}").is_none());
        assert!(js_regex(r"\{2,1\}").is_some());
    }

    #[test]
    fn test_puny_decode_edges() {
        assert_eq!(puny_decode("mnchen-3ya").as_deref(), Some("m\u{fc}nchen"));
        assert_eq!(puny_decode("99999999999999"), None);
        assert_eq!(puny_decode("\u{e9}-abc"), None);
        assert_eq!(puny_decode("MNCHEN-3YA"), None);
        assert_eq!(puny_decode("0b"), None);
    }

    #[test]
    fn test_unicode_label_context() {
        assert!(is_unicode_label("l\u{00b7}l"));
        assert!(!is_unicode_label("a\u{00b7}b"));
        assert!(is_unicode_label("\u{0375}\u{03b1}"));
        assert!(!is_unicode_label("\u{0375}a"));
        assert!(is_unicode_label("\u{05d0}\u{05f3}"));
        assert!(!is_unicode_label("a\u{05f3}"));
        assert!(is_unicode_label("\u{0627}\u{200c}\u{0628}"));
        assert!(!is_unicode_label("a\u{200c}b"));
        assert!(is_unicode_label("\u{0915}\u{094d}\u{200d}"));
        assert!(!is_unicode_label("a\u{200d}b"));
        assert!(is_unicode_label("\u{30ab}\u{30fb}\u{30bf}"));
        assert!(!is_unicode_label("abc\u{30fb}def"));
        assert!(!is_unicode_label("\u{0301}abc"));
        let too_long: String = "\u{00fc}".repeat(60);
        assert!(!is_unicode_label(&too_long));
    }

    #[test]
    fn test_matches_unknown_format() {
        assert!(matches("custom-unknown", "anything"));
        assert!(matches("url", "https://example.com/a"));
        assert!(!matches("url", "not a url"));
    }
}
