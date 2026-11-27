//! Normalized mouse events and component results (`TuiMouseEvent` / `TuiMouseEventResult` of
//! packages/tui/src/tui.ts).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEventType {
    Press,
    Release,
    Move,
    Drag,
    Click,
    Wheel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

/// Zero-based cell event; `x`/`y` are local to the receiving component and may be negative after an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseEventType,
    pub button: MouseButton,
    pub x: i64,
    pub y: i64,
    pub screen_x: i64,
    pub screen_y: i64,
    pub width: usize,
    pub height: usize,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Logical lines; negative scrolls up.
    pub wheel_delta: Option<i64>,
    pub click_count: Option<u32>,
}

/// Unset fields are absent in TS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseResult {
    pub handled: Option<bool>,
    pub capture: Option<bool>,
    pub focus: Option<bool>,
    pub render: Option<bool>,
}

/// One decoded report. Rows and columns are 0-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SgrMouse {
    /// Raw button code (bit 32 = motion, bit 64 = wheel, low bits = button).
    pub button: u32,
    pub col: i64,
    pub row: i64,
    /// A release report (`m`).
    pub release: bool,
    /// Vertical wheel: -1 up, 1 down. Horizontal wheel reports (66/67) are not a direction.
    pub wheel: Option<i64>,
    /// The pointer moved (hover or drag) rather than clicked.
    pub motion: bool,
    /// A left-button press (not motion, release or wheel).
    pub left_click: bool,
}

fn split_sgr_body(data: &str) -> Option<(&str, bool)> {
    let body = data.strip_prefix("\x1b[<")?;
    match body.as_bytes().last()? {
        b'M' => Some((&body[..body.len() - 1], false)),
        b'm' => Some((&body[..body.len() - 1], true)),
        _ => None,
    }
}

fn next_sgr_number(parts: &mut std::str::Split<'_, char>) -> Option<i64> {
    let part = parts.next()?;
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse::<i64>().ok()
}

fn parse_sgr_coords(body: &str) -> Option<(u32, i64, i64)> {
    let mut parts = body.split(';');
    let button = next_sgr_number(&mut parts)?;
    let col = next_sgr_number(&mut parts)? - 1;
    let row = next_sgr_number(&mut parts)? - 1;
    if parts.next().is_some() {
        return None;
    }
    let button = u32::try_from(button).ok()?;
    Some((button, col, row))
}

fn sgr_wheel(button: u32) -> Option<i64> {
    if button & 64 == 0 || button & 2 != 0 {
        return None;
    }
    Some(if button & 1 != 0 { 1 } else { -1 })
}

fn decode_sgr_flags(button: u32, release: bool) -> (Option<i64>, bool, bool) {
    let wheel = sgr_wheel(button);
    let motion = button & 32 != 0 && wheel.is_none();
    let left_click = !release && wheel.is_none() && !motion && button & 3 == 0;
    (wheel, motion, left_click)
}

/// Decode `data`, or `None` when it is not exactly one SGR mouse report.
pub fn parse_sgr_mouse(data: &str) -> Option<SgrMouse> {
    let (body, release) = split_sgr_body(data)?;
    let (button, col, row) = parse_sgr_coords(body)?;
    let (wheel, motion, left_click) = decode_sgr_flags(button, release);
    Some(SgrMouse {
        button,
        col,
        row,
        release,
        wheel,
        motion,
        left_click,
    })
}
