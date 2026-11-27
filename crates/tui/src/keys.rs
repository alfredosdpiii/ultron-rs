//! Key parsing for legacy, xterm modifyOtherKeys and Kitty keyboard sequences (port of packages/tui/src/keys.ts).

pub mod matching;
pub use matching::matches_key;

const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CTRL: u32 = 4;
const SUPER: u32 = 8;
const LOCK_MASK: u32 = 64 + 128;

const UP: i64 = -1;
const DOWN: i64 = -2;
const RIGHT: i64 = -3;
const LEFT: i64 = -4;
const DELETE: i64 = -10;
const INSERT: i64 = -11;
const PAGE_UP: i64 = -12;
const PAGE_DOWN: i64 = -13;
const HOME: i64 = -14;
const END: i64 = -15;

const SYMBOL_KEYS: &str = "`-=[]\\;',./!@#$%^&*()_+|~{}:<>?";

const KITTY_FUNCTIONAL_MAP: &[(i64, i64)] = &[
    (57409, 46),
    (57410, 47),
    (57411, 42),
    (57412, 45),
    (57413, 43),
    (57415, 61),
    (57416, 44),
    (57417, LEFT),
    (57418, RIGHT),
    (57419, UP),
    (57420, DOWN),
    (57421, PAGE_UP),
    (57422, PAGE_DOWN),
    (57423, HOME),
    (57424, END),
    (57425, INSERT),
    (57426, DELETE),
];

const KITTY_TILDE_KEYS: &[(i64, i64)] = &[
    (2, INSERT),
    (3, DELETE),
    (5, PAGE_UP),
    (6, PAGE_DOWN),
    (7, HOME),
    (8, END),
];

const NAMED_CODEPOINTS: &[(i64, &str)] = &[
    (27, "escape"),
    (9, "tab"),
    (13, "enter"),
    (57414, "enter"),
    (32, "space"),
    (127, "backspace"),
    (DELETE, "delete"),
    (INSERT, "insert"),
    (HOME, "home"),
    (END, "end"),
    (PAGE_UP, "pageUp"),
    (PAGE_DOWN, "pageDown"),
    (UP, "up"),
    (DOWN, "down"),
    (LEFT, "left"),
    (RIGHT, "right"),
];

const LEGACY_KEY_IDS: &[(&str, &str)] = &[
    ("\x1bOA", "up"),
    ("\x1bOB", "down"),
    ("\x1bOC", "right"),
    ("\x1bOD", "left"),
    ("\x1bOH", "home"),
    ("\x1bOF", "end"),
    ("\x1b[E", "clear"),
    ("\x1bOE", "clear"),
    ("\x1bOe", "ctrl+clear"),
    ("\x1b[e", "shift+clear"),
    ("\x1b[2~", "insert"),
    ("\x1b[2$", "shift+insert"),
    ("\x1b[2^", "ctrl+insert"),
    ("\x1b[3$", "shift+delete"),
    ("\x1b[3^", "ctrl+delete"),
    ("\x1b[[5~", "pageUp"),
    ("\x1b[[6~", "pageDown"),
    ("\x1b[a", "shift+up"),
    ("\x1b[b", "shift+down"),
    ("\x1b[c", "shift+right"),
    ("\x1b[d", "shift+left"),
    ("\x1bOa", "ctrl+up"),
    ("\x1bOb", "ctrl+down"),
    ("\x1bOc", "ctrl+right"),
    ("\x1bOd", "ctrl+left"),
    ("\x1b[5$", "shift+pageUp"),
    ("\x1b[6$", "shift+pageDown"),
    ("\x1b[7$", "shift+home"),
    ("\x1b[8$", "shift+end"),
    ("\x1b[5^", "ctrl+pageUp"),
    ("\x1b[6^", "ctrl+pageDown"),
    ("\x1b[7^", "ctrl+home"),
    ("\x1b[8^", "ctrl+end"),
    ("\x1bOP", "f1"),
    ("\x1b[11~", "f1"),
    ("\x1b[[A", "f1"),
    ("\x1bOQ", "f2"),
    ("\x1b[12~", "f2"),
    ("\x1b[[B", "f2"),
    ("\x1bOR", "f3"),
    ("\x1b[13~", "f3"),
    ("\x1b[[C", "f3"),
    ("\x1bOS", "f4"),
    ("\x1b[14~", "f4"),
    ("\x1b[[D", "f4"),
    ("\x1b[[E", "f5"),
    ("\x1b[15~", "f5"),
    ("\x1b[17~", "f6"),
    ("\x1b[18~", "f7"),
    ("\x1b[19~", "f8"),
    ("\x1b[20~", "f9"),
    ("\x1b[21~", "f10"),
    ("\x1b[23~", "f11"),
    ("\x1b[24~", "f12"),
    ("\x1bb", "alt+left"),
    ("\x1bf", "alt+right"),
    ("\x1bp", "alt+up"),
    ("\x1bn", "alt+down"),
];

const FIXED_KEY_IDS: &[(&str, &str)] = &[
    ("\x1b", "escape"),
    ("\x1c", "ctrl+\\"),
    ("\x1d", "ctrl+]"),
    ("\x1f", "ctrl+-"),
    ("\x1b\x1b", "ctrl+alt+["),
    ("\x1b\x1c", "ctrl+alt+\\"),
    ("\x1b\x1d", "ctrl+alt+]"),
    ("\x1b\x1f", "ctrl+alt+-"),
    ("\t", "tab"),
    ("\r", "enter"),
    ("\x1bOM", "enter"),
    ("\x00", "ctrl+space"),
    (" ", "space"),
    ("\x7f", "backspace"),
    ("\x1b[Z", "shift+tab"),
    ("\x1b\x7f", "alt+backspace"),
    ("\x1b\x08", "alt+backspace"),
];

const NON_KITTY_KEY_IDS: &[(&str, &str)] = &[
    ("\n", "enter"),
    ("\x1b\r", "alt+enter"),
    ("\x1b ", "alt+space"),
    ("\x1bB", "alt+left"),
    ("\x1bF", "alt+right"),
];

const ARROW_KEY_IDS: &[(&str, &str)] = &[
    ("\x1b[A", "up"),
    ("\x1b[B", "down"),
    ("\x1b[C", "right"),
    ("\x1b[D", "left"),
    ("\x1b[H", "home"),
    ("\x1bOH", "home"),
    ("\x1b[F", "end"),
    ("\x1bOF", "end"),
    ("\x1b[3~", "delete"),
    ("\x1b[5~", "pageUp"),
    ("\x1b[6~", "pageDown"),
];

fn lookup_i64(table: &[(i64, i64)], key: i64) -> Option<i64> {
    for &(k, v) in table {
        if k == key {
            return Some(v);
        }
    }
    None
}

fn lookup_str(table: &[(i64, &'static str)], key: i64) -> Option<&'static str> {
    for &(k, v) in table {
        if k == key {
            return Some(v);
        }
    }
    None
}

fn lookup_seq(table: &[(&str, &'static str)], data: &str) -> Option<&'static str> {
    for &(seq, id) in table {
        if seq == data {
            return Some(id);
        }
    }
    None
}

fn is_symbol(cp: i64) -> bool {
    u8::try_from(cp).is_ok_and(|b| SYMBOL_KEYS.as_bytes().contains(&b))
}

fn normalize_kitty_functional(cp: i64) -> i64 {
    if (57399..=57408).contains(&cp) {
        return cp - 57399 + 48;
    }
    lookup_i64(KITTY_FUNCTIONAL_MAP, cp).unwrap_or(cp)
}

struct Kitty {
    codepoint: i64,
    base_layout: Option<i64>,
    modifier: u32,
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn num(s: &str) -> i64 {
    s.parse().unwrap_or(i64::MAX)
}

type CsiUMatch<'a> = (
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
);

fn opt_digits(val: Option<&str>, allow_empty: bool) -> bool {
    match val {
        None => true,
        Some("") => allow_empty,
        Some(s) => digits(s),
    }
}

type CsiUKeys<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);

fn parse_csi_u_keys(keys: &str, has_mods: bool) -> Option<CsiUKeys<'_>> {
    let mut kp = keys.split(':');
    let cp = kp.next().filter(|s| digits(s))?;
    let shifted = kp.next();
    let base = kp.next();
    let trailing_event = kp.next();
    if kp.next().is_some() || (trailing_event.is_some() && has_mods) {
        return None;
    }
    if !opt_digits(shifted, true) || !opt_digits(base, false) || !opt_digits(trailing_event, false) {
        return None;
    }
    Some((cp, shifted, base, trailing_event))
}

fn parse_csi_u_mods(m: &str) -> Option<(Option<&str>, Option<&str>)> {
    let mut mp = m.split(':');
    let mv = mp.next().filter(|s| digits(s))?;
    let ev = mp.next();
    if mp.next().is_some() || !opt_digits(ev, false) {
        return None;
    }
    Some((Some(mv), ev))
}

/// `^\x1b\[(\d+)(?::(\d*))?(?::(\d+))?(?:;(\d+))?(?::(\d+))?u$` -> (cp, shifted, base, mod, event)
fn match_csi_u(data: &str) -> Option<CsiUMatch<'_>> {
    let body = data.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let (keys, mods) = body.split_once(';').map_or((body, None), |(k, m)| (k, Some(m)));
    let (cp, shifted, base, trailing_event) = parse_csi_u_keys(keys, mods.is_some())?;
    let (modv, ev) = match mods {
        None => (None, trailing_event),
        Some(m) => parse_csi_u_mods(m)?,
    };
    Some((cp, shifted, base, modv, ev))
}

/// `^\x1b\[1;(\d+)(?::(\d+))?([<finals>])$`
fn match_csi1(data: &str, finals: &str) -> Option<(i64, char)> {
    let body = data.strip_prefix("\x1b[1;")?;
    let last = body.chars().last()?;
    if !finals.contains(last) {
        return None;
    }
    let body = &body[..body.len() - 1];
    let (m, ev) = body.split_once(':').map_or((body, None), |(m, e)| (m, Some(e)));
    if !digits(m) || ev.is_some_and(|e| !digits(e)) {
        return None;
    }
    Some((num(m), last))
}

fn parse_kitty_csi_u(data: &str) -> Option<Kitty> {
    let (cp, _, base, modv, _) = match_csi_u(data)?;
    Some(Kitty {
        codepoint: num(cp),
        base_layout: base.map(num),
        modifier: modv.map_or(1, num).wrapping_sub(1) as u32,
    })
}

fn parse_kitty_arrow(data: &str) -> Option<Kitty> {
    let (m, c) = match_csi1(data, "ABCD")?;
    let codepoint = match c {
        'A' => UP,
        'B' => DOWN,
        'C' => RIGHT,
        _ => LEFT,
    };
    Some(Kitty {
        codepoint,
        base_layout: None,
        modifier: (m - 1) as u32,
    })
}

fn parse_kitty_tilde(data: &str) -> Option<Kitty> {
    let body = data.strip_prefix("\x1b[")?.strip_suffix('~')?;
    let (head, ev) = body.split_once(':').map_or((body, None), |(h, e)| (h, Some(e)));
    let (k, m) = head.split_once(';').map_or((head, None), |(k, m)| (k, Some(m)));
    if !digits(k) || !opt_digits(m, false) || !opt_digits(ev, false) {
        return None;
    }
    let codepoint = lookup_i64(KITTY_TILDE_KEYS, num(k))?;
    Some(Kitty {
        codepoint,
        base_layout: None,
        modifier: (m.map_or(1, num) - 1) as u32,
    })
}

fn parse_kitty_home_end(data: &str) -> Option<Kitty> {
    let (m, c) = match_csi1(data, "HF")?;
    let codepoint = if c == 'H' { HOME } else { END };
    Some(Kitty {
        codepoint,
        base_layout: None,
        modifier: (m - 1) as u32,
    })
}

fn parse_kitty(data: &str) -> Option<Kitty> {
    if let Some(k) = parse_kitty_csi_u(data) {
        return Some(k);
    }
    if let Some(k) = parse_kitty_arrow(data) {
        return Some(k);
    }
    if let Some(k) = parse_kitty_tilde(data) {
        return Some(k);
    }
    parse_kitty_home_end(data)
}

/// `^\x1b\[27;(\d+);(\d+)~$` -> (codepoint, modifier)
fn parse_modify_other_keys(data: &str) -> Option<(i64, u32)> {
    let body = data.strip_prefix("\x1b[27;")?.strip_suffix('~')?;
    let (m, cp) = body.split_once(';')?;
    if !digits(m) || !digits(cp) {
        return None;
    }
    Some((num(cp), (num(m) - 1) as u32))
}

fn format_with_modifiers(name: &str, modifier: u32) -> Option<String> {
    let m = modifier & !LOCK_MASK;
    if m & !(SHIFT | CTRL | ALT | SUPER) != 0 {
        return None;
    }
    let mut out = String::new();
    for (bit, label) in [(SHIFT, "shift+"), (CTRL, "ctrl+"), (ALT, "alt+"), (SUPER, "super+")] {
        if m & bit != 0 {
            out.push_str(label);
        }
    }
    out.push_str(name);
    Some(out)
}

fn key_name_for_codepoint(cp: i64) -> Option<String> {
    if let Some(name) = lookup_str(NAMED_CODEPOINTS, cp) {
        return Some(name.to_string());
    }
    if (48..=57).contains(&cp) || (97..=122).contains(&cp) || is_symbol(cp) {
        return Some(char::from(cp as u8).to_string());
    }
    None
}

fn format_parsed(codepoint: i64, modifier: u32, base_layout: Option<i64>) -> Option<String> {
    let mut identity = normalize_kitty_functional(codepoint);
    if (modifier & !LOCK_MASK) & SHIFT != 0 && (65..=90).contains(&identity) {
        identity += 32;
    }
    let authoritative = (97..=122).contains(&identity) || (48..=57).contains(&identity) || is_symbol(identity);
    let cp = if authoritative {
        identity
    } else {
        base_layout.unwrap_or(identity)
    };
    let name = key_name_for_codepoint(cp)?;
    format_with_modifiers(&name, modifier)
}

fn legacy_key_id(data: &str) -> Option<&'static str> {
    lookup_seq(LEGACY_KEY_IDS, data)
}

fn env_non_empty(env: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    env(name).is_some_and(|v| !v.is_empty())
}

/// Parsing context the TS module reads from globals: Kitty protocol state and the Windows Terminal session check.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyContext {
    pub kitty_active: bool,
    pub windows_terminal: bool,
}

impl KeyContext {
    /// The state a TUI starts with: Kitty off until the terminal confirms it, Windows Terminal from the
    /// environment (`isWindowsTerminalSession()`: `WT_SESSION` set and none of `SSH_CONNECTION`, `SSH_CLIENT`,
    /// `SSH_TTY`; a raw BS byte is then Ctrl+Backspace).
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> KeyContext {
        KeyContext {
            kitty_active: false,
            windows_terminal: env_non_empty(env, "WT_SESSION")
                && !env_non_empty(env, "SSH_CONNECTION")
                && !env_non_empty(env, "SSH_CLIENT")
                && !env_non_empty(env, "SSH_TTY"),
        }
    }
}

fn parse_protocol_key(data: &str) -> Option<String> {
    if let Some(k) = parse_kitty(data) {
        return format_parsed(k.codepoint, k.modifier, k.base_layout);
    }
    if let Some((cp, m)) = parse_modify_other_keys(data) {
        return format_parsed(cp, m, None);
    }
    None
}

fn parse_fixed_key(data: &str, ctx: KeyContext) -> Option<&'static str> {
    if data == "\x08" {
        return Some(if ctx.windows_terminal {
            "ctrl+backspace"
        } else {
            "backspace"
        });
    }
    if let Some(id) = lookup_seq(FIXED_KEY_IDS, data) {
        return Some(id);
    }
    if !ctx.kitty_active {
        return lookup_seq(NON_KITTY_KEY_IDS, data);
    }
    None
}

fn parse_alt_utf16(units: &[u16], kitty_active: bool) -> Option<String> {
    if kitty_active || units.len() != 2 || units[0] != 0x1b {
        return None;
    }
    let code = i64::from(units[1]);
    if (1..=26).contains(&code) {
        return Some(format!("ctrl+alt+{}", char::from((code + 96) as u8)));
    }
    if (97..=122).contains(&code) || (48..=57).contains(&code) || is_symbol(code) {
        return Some(format!("alt+{}", char::from(code as u8)));
    }
    None
}

fn parse_single_utf16(units: &[u16], data: &str) -> Option<String> {
    if units.len() != 1 {
        return None;
    }
    let code = units[0];
    if (1..=26).contains(&code) {
        return Some(format!("ctrl+{}", char::from((code + 96) as u8)));
    }
    if (32..=126).contains(&code) {
        return Some(data.to_string());
    }
    None
}

/// Key identifier (`ctrl+shift+a`, `pageUp`, ...) for one input sequence.
pub fn parse_key(data: &str, ctx: KeyContext) -> Option<String> {
    if let Some(k) = parse_protocol_key(data) {
        return Some(k);
    }
    if ctx.kitty_active && (data == "\x1b\r" || data == "\n") {
        return Some("shift+enter".into());
    }
    if let Some(id) = legacy_key_id(data).or_else(|| parse_fixed_key(data, ctx)) {
        return Some(id.into());
    }
    let units: Vec<u16> = data.encode_utf16().collect();
    if let Some(alt) = parse_alt_utf16(&units, ctx.kitty_active) {
        return Some(alt);
    }
    if let Some(arrow) = lookup_seq(ARROW_KEY_IDS, data) {
        return Some(arrow.into());
    }
    parse_single_utf16(&units, data)
}

/// TS `isKeyRelease`: a Kitty flag-2 release event (never bracketed paste content), by the substrings TypeScript looks for.
pub fn is_key_release(data: &str) -> bool {
    !data.contains("\x1b[200~")
        && [":3u", ":3~", ":3A", ":3B", ":3C", ":3D", ":3H", ":3F"]
            .iter()
            .any(|p| data.contains(p))
}

/// Printable character carried by a Kitty CSI-u or modifyOtherKeys sequence (plain or shifted keys only).
pub fn decode_printable_key(data: &str) -> Option<String> {
    if let Some((cp, shifted, _, modv, _)) = match_csi_u(data) {
        let codepoint = num(cp);
        let modifier = (modv.map_or(1, num) - 1) as u32;
        if modifier & !(SHIFT | LOCK_MASK) != 0 || modifier & (ALT | CTRL) != 0 {
            return None;
        }
        let mut eff = codepoint;
        if modifier & SHIFT != 0
            && let Some(s) = shifted.filter(|s| !s.is_empty())
        {
            eff = num(s);
        }
        let eff = normalize_kitty_functional(eff);
        if eff < 32 {
            return None;
        }
        return u32::try_from(eff).ok().and_then(char::from_u32).map(String::from);
    }
    let (cp, m) = parse_modify_other_keys(data)?;
    if (m & !LOCK_MASK) & !SHIFT != 0 || cp < 32 {
        return None;
    }
    u32::try_from(cp).ok().and_then(char::from_u32).map(String::from)
}
