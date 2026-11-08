//! What a rendered frame carries besides text, shared by every painter ([`crate::renderer`],
//! [`crate::viewport`], [`crate::screen`]) and the components that emit it: the cursor marker, image lines,
//! synchronized output, and the header of a Kitty graphics command.

use crate::utils::visible_width;

/// Marks the hardware cursor position inside a rendered line (APC, zero width).
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Synchronized output (DEC mode 2026): the terminal shows what is written between these at once.
pub const SYNC_BEGIN: &str = "\x1b[?2026h";
pub const SYNC_END: &str = "\x1b[?2026l";

/// A line that draws an image (Kitty or iTerm2) rather than text.
pub fn is_image_line(line: &str) -> bool {
    line.contains(KITTY_PREFIX) || line.contains("\x1b]1337;File=")
}

/// Strip the last [`CURSOR_MARKER`] of `lines` and return its (row, column).
pub fn extract_cursor(lines: &mut [String]) -> Option<(usize, usize)> {
    for (row, line) in lines.iter_mut().enumerate().rev() {
        if let Some(idx) = line.find(CURSOR_MARKER) {
            let col = visible_width(&line[..idx]);
            line.replace_range(idx..idx + CURSOR_MARKER.len(), "");
            return Some((row, col));
        }
    }
    None
}

pub(crate) const KITTY_PREFIX: &str = "\x1b_G";

/// `/\x1b_G([^;]*);/`: where the first Kitty command starts, its controls, and where its payload starts.
pub(crate) fn kitty_command(line: &str) -> Option<(usize, &str, usize)> {
    let at = line.find(KITTY_PREFIX)?;
    let controls_start = at + KITTY_PREFIX.len();
    let end = controls_start + line[controls_start..].find(';')?;
    Some((at, &line[controls_start..end], end + 1))
}
