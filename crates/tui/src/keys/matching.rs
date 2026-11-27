//! `matchesKey` from packages/tui/src/keys.ts: does an input sequence match a key identifier such as `ctrl+shift+p`.

use super::{
    ALT, CTRL, DELETE, DOWN, END, HOME, INSERT, KeyContext, LEFT, LOCK_MASK, PAGE_DOWN, PAGE_UP, RIGHT, SHIFT, SUPER,
    UP, is_symbol, normalize_kitty_functional, parse_kitty, parse_modify_other_keys,
};

const ESCAPE: i64 = 27;
const TAB: i64 = 9;
const ENTER: i64 = 13;
const SPACE: i64 = 32;
const BACKSPACE: i64 = 127;
const KP_ENTER: i64 = 57414;

const LEGACY_SEQUENCES: &[(&str, &[&str])] = &[
    ("up", &["\x1b[A", "\x1bOA"]),
    ("down", &["\x1b[B", "\x1bOB"]),
    ("right", &["\x1b[C", "\x1bOC"]),
    ("left", &["\x1b[D", "\x1bOD"]),
    ("home", &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"]),
    ("end", &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"]),
    ("insert", &["\x1b[2~"]),
    ("delete", &["\x1b[3~"]),
    ("pageup", &["\x1b[5~", "\x1b[[5~"]),
    ("pagedown", &["\x1b[6~", "\x1b[[6~"]),
    ("clear", &["\x1b[E", "\x1bOE"]),
    ("f1", &["\x1bOP", "\x1b[11~", "\x1b[[A"]),
    ("f2", &["\x1bOQ", "\x1b[12~", "\x1b[[B"]),
    ("f3", &["\x1bOR", "\x1b[13~", "\x1b[[C"]),
    ("f4", &["\x1bOS", "\x1b[14~", "\x1b[[D"]),
    ("f5", &["\x1b[15~", "\x1b[[E"]),
    ("f6", &["\x1b[17~"]),
    ("f7", &["\x1b[18~"]),
    ("f8", &["\x1b[19~"]),
    ("f9", &["\x1b[20~"]),
    ("f10", &["\x1b[21~"]),
    ("f11", &["\x1b[23~"]),
    ("f12", &["\x1b[24~"]),
];

const LEGACY_MODIFIED_SEQUENCES: &[(&str, &str, &str)] = &[
    ("up", "\x1b[a", "\x1bOa"),
    ("down", "\x1b[b", "\x1bOb"),
    ("right", "\x1b[c", "\x1bOc"),
    ("left", "\x1b[d", "\x1bOd"),
    ("clear", "\x1b[e", "\x1bOe"),
    ("insert", "\x1b[2$", "\x1b[2^"),
    ("delete", "\x1b[3$", "\x1b[3^"),
    ("pageup", "\x1b[5$", "\x1b[5^"),
    ("pagedown", "\x1b[6$", "\x1b[6^"),
    ("home", "\x1b[7$", "\x1b[7^"),
    ("end", "\x1b[8$", "\x1b[8^"),
];

const FUNCTIONAL_KEYS: &[(&str, i64)] = &[
    ("insert", INSERT),
    ("delete", DELETE),
    ("home", HOME),
    ("end", END),
    ("pageup", PAGE_UP),
    ("pagedown", PAGE_DOWN),
];

const F_KEYS: &[&str] = &[
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
];

fn legacy(key: &str) -> &'static [&'static str] {
    for &(k, seqs) in LEGACY_SEQUENCES {
        if k == key {
            return seqs;
        }
    }
    &[]
}

fn legacy_modified(data: &str, key: &str, modifier: u32) -> bool {
    for &(k, shift, ctrl) in LEGACY_MODIFIED_SEQUENCES {
        if k == key {
            return (modifier == SHIFT && data == shift) || (modifier == CTRL && data == ctrl);
        }
    }
    false
}

fn shifted_identity(cp: i64, modifier: u32) -> i64 {
    if (modifier & !LOCK_MASK) & SHIFT != 0 && (65..=90).contains(&cp) {
        cp + 32
    } else {
        cp
    }
}

fn kitty(data: &str, expected_cp: i64, expected_mod: u32) -> bool {
    let Some(p) = parse_kitty(data) else { return false };
    if p.modifier & !LOCK_MASK != expected_mod & !LOCK_MASK {
        return false;
    }
    let cp = shifted_identity(normalize_kitty_functional(p.codepoint), p.modifier);
    if cp == shifted_identity(normalize_kitty_functional(expected_cp), expected_mod) {
        return true;
    }
    p.base_layout == Some(expected_cp) && !(97..=122).contains(&cp) && !is_symbol(cp)
}

fn modify_other_keys(data: &str, expected_cp: i64, expected_mod: u32) -> bool {
    parse_modify_other_keys(data) == Some((expected_cp, expected_mod))
}

fn printable_modify_other_keys(data: &str, expected_cp: i64, expected_mod: u32) -> bool {
    if expected_mod == 0 {
        return false;
    }
    match parse_modify_other_keys(data) {
        Some((cp, m)) if m == expected_mod => shifted_identity(cp, m) == shifted_identity(expected_cp, expected_mod),
        _ => false,
    }
}

fn raw_backspace(data: &str, expected_mod: u32, ctx: KeyContext) -> bool {
    match data {
        "\x7f" => expected_mod == 0,
        "\x08" => expected_mod == if ctx.windows_terminal { CTRL } else { 0 },
        _ => false,
    }
}

fn raw_ctrl_char(key: char) -> Option<char> {
    match key {
        'a'..='z' | '[' | '\\' | ']' | '_' => Some(char::from(key as u8 & 0x1f)),
        '-' => Some('\x1f'),
        _ => None,
    }
}

fn parse_key_id(key_id: &str) -> Option<(String, u32)> {
    let lower = key_id.to_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    let key = parts[parts.len() - 1];
    if key.is_empty() {
        return None;
    }
    let mut m = 0;
    for (name, bit) in [("shift", SHIFT), ("alt", ALT), ("ctrl", CTRL), ("super", SUPER)] {
        if parts.contains(&name) {
            m |= bit;
        }
    }
    Some((key.to_string(), m))
}

fn match_functional(data: &str, key: &str, m: u32, cp: i64) -> bool {
    if m == 0 {
        legacy(key).contains(&data) || kitty(data, cp, 0)
    } else {
        legacy_modified(data, key, m) || kitty(data, cp, m)
    }
}

fn match_table_key(data: &str, key: &str, m: u32) -> Option<bool> {
    for &(name, cp) in FUNCTIONAL_KEYS {
        if name == key {
            return Some(match_functional(data, key, m, cp));
        }
    }
    if F_KEYS.contains(&key) {
        return Some(m == 0 && legacy(key).contains(&data));
    }
    None
}

fn match_space(data: &str, m: u32, kitty_active: bool) -> bool {
    if !kitty_active && ((m == CTRL && data == "\0") || (m == ALT && data == "\x1b ")) {
        return true;
    }
    (m == 0 && data == " ") || kitty(data, SPACE, m) || modify_other_keys(data, SPACE, m)
}

fn match_tab(data: &str, m: u32) -> bool {
    match m {
        SHIFT => data == "\x1b[Z" || kitty(data, TAB, SHIFT) || modify_other_keys(data, TAB, SHIFT),
        0 => data == "\t" || kitty(data, TAB, 0),
        _ => kitty(data, TAB, m) || modify_other_keys(data, TAB, m),
    }
}

fn match_enter_legacy(data: &str, m: u32, kitty_active: bool) -> bool {
    match m {
        SHIFT => kitty_active && (data == "\x1b\r" || data == "\n"),
        ALT => !kitty_active && data == "\x1b\r",
        0 => data == "\r" || (!kitty_active && data == "\n") || data == "\x1bOM",
        _ => false,
    }
}

fn match_enter(data: &str, m: u32, kitty_active: bool) -> bool {
    if match_enter_legacy(data, m, kitty_active) || kitty(data, ENTER, m) || kitty(data, KP_ENTER, m) {
        return true;
    }
    m != 0 && modify_other_keys(data, ENTER, m)
}

fn raw_alt_backspace(data: &str) -> bool {
    data == "\x1b\x7f" || data == "\x1b\x08"
}

fn match_backspace(data: &str, m: u32, ctx: KeyContext) -> bool {
    let raw = match m {
        ALT => raw_alt_backspace(data),
        CTRL | 0 => raw_backspace(data, m, ctx),
        _ => false,
    };
    raw || kitty(data, BACKSPACE, m) || modify_other_keys(data, BACKSPACE, m)
}

fn match_editing_key(data: &str, key: &str, m: u32, ctx: KeyContext) -> Option<bool> {
    Some(match key {
        "escape" | "esc" => m == 0 && (data == "\x1b" || kitty(data, ESCAPE, 0) || modify_other_keys(data, ESCAPE, 0)),
        "space" => match_space(data, m, ctx.kitty_active),
        "tab" => match_tab(data, m),
        "enter" | "return" => match_enter(data, m, ctx.kitty_active),
        "backspace" => match_backspace(data, m, ctx),
        _ => return None,
    })
}

fn horizontal_spec(key: &str) -> (i64, &'static str, &'static str, &'static str, &'static str) {
    if key == "left" {
        (LEFT, "\x1b[1;3D", "\x1bB", "\x1bb", "\x1b[1;5D")
    } else {
        (RIGHT, "\x1b[1;3C", "\x1bF", "\x1bf", "\x1b[1;5C")
    }
}

fn match_horizontal_arrow(data: &str, key: &str, m: u32, kitty_active: bool) -> bool {
    let (cp, alt_csi, alt_upper, alt_lower, ctrl_csi) = horizontal_spec(key);
    match m {
        ALT => data == alt_csi || (!kitty_active && data == alt_upper) || data == alt_lower || kitty(data, cp, ALT),
        CTRL => data == ctrl_csi || match_functional(data, key, m, cp),
        _ => match_functional(data, key, m, cp),
    }
}

fn match_clear_key(data: &str, m: u32) -> bool {
    if m == 0 {
        legacy("clear").contains(&data)
    } else {
        legacy_modified(data, "clear", m)
    }
}

fn match_vertical_arrow(data: &str, key: &str, m: u32) -> bool {
    let (seq, cp) = if key == "up" { ("\x1bp", UP) } else { ("\x1bn", DOWN) };
    if m == ALT {
        return data == seq || kitty(data, cp, ALT);
    }
    match_functional(data, key, m, cp)
}

fn match_nav_key(data: &str, key: &str, m: u32, kitty_active: bool) -> Option<bool> {
    match key {
        "clear" => Some(match_clear_key(data, m)),
        "up" | "down" => Some(match_vertical_arrow(data, key, m)),
        "left" | "right" => Some(match_horizontal_arrow(data, key, m, kitty_active)),
        _ => None,
    }
}

fn single_matchable_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if c.is_ascii_lowercase() || c.is_ascii_digit() || is_symbol(c as i64) {
        Some(c)
    } else {
        None
    }
}

fn esc_ctrl_matches(data: &str, r: char) -> bool {
    data.strip_prefix('\x1b')
        .is_some_and(|rest| rest.len() == 1 && rest.starts_with(r))
}

fn ctrl_matches(data: &str, r: char) -> bool {
    data.len() == 1 && data.starts_with(r)
}

fn match_printable_ctrl(data: &str, c: char, m: u32, kitty_active: bool) -> bool {
    let Some(r) = raw_ctrl_char(c) else {
        return false;
    };
    (m == CTRL + ALT && !kitty_active && esc_ctrl_matches(data, r)) || (m == CTRL && ctrl_matches(data, r))
}

fn match_printable_alt_or_shift(data: &str, key: &str, c: char, m: u32, kitty_active: bool) -> bool {
    (m == ALT && !kitty_active && data.strip_prefix('\x1b') == Some(key))
        || (m == SHIFT && c.is_ascii_lowercase() && data == key.to_uppercase())
}

fn match_printable_legacy_mod(data: &str, key: &str, c: char, m: u32, kitty_active: bool) -> bool {
    match_printable_ctrl(data, c, m, kitty_active) || match_printable_alt_or_shift(data, key, c, m, kitty_active)
}

fn match_printable_key(data: &str, key: &str, m: u32, kitty_active: bool) -> bool {
    let Some(c) = single_matchable_char(key) else {
        return false;
    };
    if match_printable_legacy_mod(data, key, c, m, kitty_active) {
        return true;
    }
    let cp = c as i64;
    if m == 0 {
        return data == key || kitty(data, cp, 0);
    }
    kitty(data, cp, m) || printable_modify_other_keys(data, cp, m)
}

/// True when `data` is the key `key_id` (`escape`, `ctrl+c`, `shift+enter`, `alt+left`, `super+k`, ...).
/// `ctx` stands in for the TS globals: Kitty protocol state and the Windows Terminal session check.
pub fn matches_key(data: &str, key_id: &str, ctx: KeyContext) -> bool {
    let Some((key, m)) = parse_key_id(key_id) else {
        return false;
    };
    if let Some(res) = match_editing_key(data, &key, m, ctx) {
        return res;
    }
    if let Some(res) = match_table_key(data, &key, m) {
        return res;
    }
    if let Some(res) = match_nav_key(data, &key, m, ctx.kitty_active) {
        return res;
    }
    match_printable_key(data, &key, m, ctx.kitty_active)
}
