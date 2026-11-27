/// Remove ANSI, OSC and APC sequences while preserving visible text.
pub fn strip_terminal_sequences(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = ansi_len(s, i) {
            i += len;
            continue;
        }
        let c = s[i..].chars().next().unwrap_or('\0');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Expand Thai/Lao AM vowels and tabs the way terminals draw them (`normalizeTerminalOutput`).
pub fn normalize_terminal_output(s: &str) -> String {
    let s = s
        .replace('\u{0E33}', "\u{0E4D}\u{0E32}")
        .replace('\u{0EB3}', "\u{0ECD}\u{0EB2}");
    if !s.contains('\t') {
        return s;
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = ansi_len(&s, i) {
            out.push_str(&s[i..i + len]);
            i += len;
            continue;
        }
        let c = s[i..].chars().next().unwrap_or('\0');
        if c == '\t' {
            out.push_str("   ");
        } else {
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

/// Byte length of the CSI/OSC/APC sequence starting at `pos`, if any (`extractAnsiCode`).
pub fn ansi_len(s: &str, pos: usize) -> Option<usize> {
    let b = s.as_bytes();
    if b.get(pos) != Some(&0x1b) {
        return None;
    }
    match b.get(pos + 1) {
        Some(b'[') => {
            let end = b[pos + 2..].iter().position(|c| b"mGKHJ".contains(c))?;
            Some(end + 3)
        }
        Some(b']') | Some(b'_') => {
            let mut j = pos + 2;
            while j < b.len() {
                if b[j] == 0x07 {
                    return Some(j + 1 - pos);
                }
                if b[j] == 0x1b && b.get(j + 1) == Some(&b'\\') {
                    return Some(j + 2 - pos);
                }
                j += 1;
            }
            None
        }
        _ => None,
    }
}

/// `extractAnsiCode`: the escape sequence at `pos`.
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<&str> {
    ansi_len(s, pos).map(|len| &s[pos..pos + len])
}

/// End (byte offset) of the plain-text run starting at `i`.
pub(super) fn text_run_end(s: &str, i: usize, stop_at_tab: bool) -> usize {
    let b = s.as_bytes();
    let mut end = i;
    while end < b.len() {
        if (stop_at_tab && b[end] == b'\t') || ansi_len(s, end).is_some() {
            break;
        }
        end += 1;
    }
    end
}

#[derive(Clone, PartialEq)]
struct Hyperlink {
    params: String,
    url: String,
    bel: bool,
}

enum Osc8 {
    Other,
    Close,
    Open(Hyperlink),
}

fn parse_osc8(code: &str) -> Osc8 {
    let Some(rest) = code.strip_prefix("\x1b]8;") else {
        return Osc8::Other;
    };
    let bel = code.ends_with('\x07');
    let body = if bel {
        &rest[..rest.len().saturating_sub(1)]
    } else {
        &rest[..rest.len().saturating_sub(2)]
    };
    let Some(sep) = body.find(';') else {
        return Osc8::Other;
    };
    let url = &body[sep + 1..];
    if url.is_empty() {
        return Osc8::Close;
    }
    Osc8::Open(Hyperlink {
        params: body[..sep].to_string(),
        url: url.to_string(),
        bel,
    })
}

/// The OSC 8 params that mark a link target the terminal never sees ([`link_target`]).
macro_rules! target_params {
    () => {
        "viewport-only=1"
    };
}
const TARGET_PARAMS: &str = target_params!();
const TARGET_OPEN: &str = concat!("\x1b]8;", target_params!(), ";");
const TARGET_CLOSE: &str = concat!("\x1b]8;", target_params!(), ";\x1b\\");

/// No control character (C0 with ESC and BEL, DEL, C1) in `url`. One inside an OSC 8 sequence can end it early, so
/// what follows reaches the terminal as escape sequences of its own; such a URL never goes into one, and a link that
/// carries one is not followed.
pub(crate) fn clean_link_url(url: &str) -> bool {
    !url.chars().any(char::is_control)
}

/// `text` linked to `url` for the fullscreen viewport only: an OSC 8 link it reads under every column of `text`
/// (wrapping carries it onto each row), which [`strip_link_targets`] removes before the bytes reach the terminal.
/// A markdown link carries its whole URL this way where the terminal gets no OSC 8, so a click on any row of a link
/// that wraps opens all of it. A `url` with a control character gives `text` alone ([`clean_link_url`]).
pub fn link_target(text: &str, url: &str) -> String {
    if !clean_link_url(url) {
        return text.to_string();
    }
    format!("{TARGET_OPEN}{url}\x1b\\{text}{TARGET_CLOSE}")
}

/// `s` without its [`link_target`] sequences: the bytes it would be had those links never been there. Their closes
/// carry the mark too (see `osc8_close`), so no other OSC 8 sequence is touched, such as the link reset every
/// painted row ends with.
pub fn strip_link_targets(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains(TARGET_OPEN) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(TARGET_OPEN) {
        let Some(len) = ansi_len(rest, at) else {
            break;
        };
        out.push_str(&rest[..at]);
        rest = &rest[at + len..];
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

/// Where the [`link_target`] sequence that ends `s` starts, if one does. A row's trailing whitespace is found through
/// these (see `trim_line_end` in the wrapper): a target is not shown, so the row ends as it would without it.
pub(super) fn trailing_link_target(s: &str) -> Option<usize> {
    let at = s.rfind(TARGET_OPEN)?;
    (ansi_len(s, at) == Some(s.len() - at)).then_some(at)
}

/// The close of `link`, written where a row or a truncation ends inside it. A [`link_target`]'s close keeps its
/// mark so [`strip_link_targets`] needs no state; any other link closes with empty params, as in TypeScript.
fn osc8_close(link: &Hyperlink) -> &'static str {
    if link.params == TARGET_PARAMS {
        TARGET_CLOSE
    } else if link.bel {
        "\x1b]8;;\x07"
    } else {
        "\x1b]8;;\x1b\\"
    }
}

pub(super) fn active_osc8_close(prefix: &str) -> &'static str {
    if !prefix.contains("\x1b]8;") {
        return "";
    }
    let mut active: Option<Hyperlink> = None;
    let mut i = 0;
    while i < prefix.len() {
        if let Some(len) = ansi_len(prefix, i) {
            match parse_osc8(&prefix[i..i + len]) {
                Osc8::Open(h) => active = Some(h),
                Osc8::Close => active = None,
                Osc8::Other => {}
            }
            i += len;
        } else {
            i += 1;
        }
    }
    active.map_or("", |h| osc8_close(&h))
}

fn parse_extended_sgr(parts: &[&str], i: usize) -> Option<(String, usize)> {
    if parts.get(i + 1) == Some(&"5") && i + 2 < parts.len() {
        Some((parts[i..i + 3].join(";"), 3))
    } else if parts.get(i + 1) == Some(&"2") && i + 4 < parts.len() {
        Some((parts[i..i + 5].join(";"), 5))
    } else {
        None
    }
}

/// Tracks active SGR attributes and OSC 8 links so styles survive line breaks.
#[derive(Default)]
pub(crate) struct AnsiCodeTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    fg: Option<String>,
    bg: Option<String>,
    link: Option<Hyperlink>,
}

impl AnsiCodeTracker {
    fn apply_sgr_enable(&mut self, code: u32) -> bool {
        match code {
            1 => self.bold = true,
            2 => self.dim = true,
            3 => self.italic = true,
            4 => self.underline = true,
            5 => self.blink = true,
            7 => self.inverse = true,
            8 => self.hidden = true,
            9 => self.strikethrough = true,
            _ => return false,
        }
        true
    }

    fn apply_sgr_disable(&mut self, code: u32) -> bool {
        match code {
            21 => self.bold = false,
            22 => {
                self.bold = false;
                self.dim = false;
            }
            23 => self.italic = false,
            24 => self.underline = false,
            25 => self.blink = false,
            27 => self.inverse = false,
            28 => self.hidden = false,
            29 => self.strikethrough = false,
            _ => return false,
        }
        true
    }

    fn apply_sgr_color(&mut self, code: u32) {
        match code {
            39 => self.fg = None,
            49 => self.bg = None,
            c if (30..=37).contains(&c) || (90..=97).contains(&c) => self.fg = Some(c.to_string()),
            c if (40..=47).contains(&c) || (100..=107).contains(&c) => self.bg = Some(c.to_string()),
            _ => {}
        }
    }

    fn apply_sgr_code(&mut self, code: u32) {
        if code == 0 {
            self.reset();
            return;
        }
        if !self.apply_sgr_enable(code) && !self.apply_sgr_disable(code) {
            self.apply_sgr_color(code);
        }
    }

    fn process_sgr_parts(&mut self, params: &str) {
        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let Some(code) = parse_int(parts[i]) else {
                i += 1;
                continue;
            };
            if (code == 38 || code == 48)
                && let Some((color, used)) = parse_extended_sgr(&parts, i)
            {
                if code == 38 {
                    self.fg = Some(color);
                } else {
                    self.bg = Some(color);
                }
                i += used;
                continue;
            }
            self.apply_sgr_code(code);
            i += 1;
        }
    }

    pub(crate) fn process(&mut self, code: &str) {
        match parse_osc8(code) {
            Osc8::Open(h) => {
                self.link = Some(h);
                return;
            }
            Osc8::Close => {
                self.link = None;
                return;
            }
            Osc8::Other => {}
        }
        if !code.ends_with('m') {
            return;
        }
        let Some(params) = first_sgr_params(code) else {
            return;
        };
        if params.is_empty() || params == "0" {
            self.reset();
            return;
        }
        self.process_sgr_parts(params);
    }

    fn reset(&mut self) {
        let link = self.link.take();
        *self = Self {
            link,
            ..Self::default()
        };
    }

    pub(crate) fn active_codes(&self) -> String {
        let mut codes: Vec<&str> = Vec::new();
        for (on, c) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.inverse, "7"),
            (self.hidden, "8"),
            (self.strikethrough, "9"),
        ] {
            if on {
                codes.push(c);
            }
        }
        if let Some(fg) = &self.fg {
            codes.push(fg);
        }
        if let Some(bg) = &self.bg {
            codes.push(bg);
        }
        let mut out = if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        };
        if let Some(h) = &self.link {
            out.push_str(&format!(
                "\x1b]8;{};{}{}",
                h.params,
                h.url,
                if h.bel { "\x07" } else { "\x1b\\" }
            ));
        }
        out
    }

    pub(crate) fn line_end_reset(&self) -> String {
        let mut out = String::new();
        if self.underline {
            out.push_str("\x1b[24m");
        }
        if let Some(h) = &self.link {
            out.push_str(osc8_close(h));
        }
        out
    }

    pub(super) fn update_from_text(&mut self, text: &str) {
        let mut i = 0;
        while i < text.len() {
            if let Some(len) = ansi_len(text, i) {
                self.process(&text[i..i + len]);
                i += len;
            } else {
                i += 1;
            }
        }
    }
}

/// `/\x1b\[([\d;]*)m/`: parameters of the first SGR sequence in `code`.
fn first_sgr_params(code: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(off) = code[from..].find("\x1b[") {
        let start = from + off + 2;
        let rest = &code[start..];
        let n = rest.bytes().take_while(|b| b.is_ascii_digit() || *b == b';').count();
        if rest.as_bytes().get(n) == Some(&b'm') {
            return Some(&rest[..n]);
        }
        from = start;
    }
    None
}

/// JavaScript `Number.parseInt(s, 10)` for SGR parameter strings.
fn parse_int(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Background color active at the end of an ANSI-styled string.
pub fn get_active_background_ansi(text: &str) -> String {
    let mut t = AnsiCodeTracker::default();
    t.update_from_text(text);
    t.bg.map_or(String::new(), |bg| format!("\x1b[{bg}m"))
}
