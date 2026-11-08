//! A TUI on the main screen (`TuiMainScreen` of startup-ui.ts): raw mode, the differential renderer, and the row the
//! hardware cursor is left on (`hardwareCursorRow`), so stopping puts the cursor below the last line drawn.

use crate::{Renderer, Terminal};
use std::io::{Stdout, Write};

pub struct MainScreen {
    terminal: Terminal<Stdout>,
    renderer: Renderer,
    /// Where the hardware cursor is, as a row of the document.
    cursor_row: i64,
}

impl MainScreen {
    /// Raw mode on the process terminal, cursor hidden.
    pub fn start() -> std::io::Result<MainScreen> {
        let mut terminal = Terminal::new(std::io::stdout());
        terminal.start(true)?;
        terminal.hide_cursor()?;
        Ok(MainScreen {
            terminal,
            renderer: Renderer::new(),
            cursor_row: 0,
        })
    }

    /// The terminal's size, at least one cell each way.
    pub fn size() -> (usize, usize) {
        let (width, height) = crate::size();
        (width.max(1) as usize, height.max(1) as usize)
    }

    /// Draw `lines` (rendered for [`Self::size`]'s width).
    pub fn paint(&mut self, lines: Vec<String>) {
        let (width, height) = Self::size();
        if let Ok(out) = self.renderer.render(lines, width, height) {
            track(&mut self.cursor_row, &out);
            let _ = self.terminal.write(&out);
        }
    }

    /// `TuiMainScreen.stop`: below the last line, cursor shown, raw mode off.
    pub fn stop(&mut self) {
        let len = self.renderer.previous_lines().len() as i64;
        if len > 0 {
            let mut out = String::from(" ");
            let diff = len - self.cursor_row;
            if diff > 0 {
                out.push_str(&format!("\x1b[{diff}B"));
            } else if diff < 0 {
                out.push_str(&format!("\x1b[{}A", -diff));
            }
            out.push_str("\r\n");
            let _ = self.terminal.write(&out);
        }
        let _ = self.terminal.show_cursor();
        let _ = self.terminal.stop();
        let _ = std::io::stdout().flush();
    }
}

/// Advance `row` over the cursor moves of renderer output (`\x1b[nA`, `\x1b[nB`, CRLF, a clear to home).
fn track(row: &mut i64, out: &str) {
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            *row += 1;
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"\x1b[2J\x1b[H") {
            *row = 0;
            i += 7;
            continue;
        }
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2 && j < bytes.len() {
                let n: i64 = out[i + 2..j].parse().unwrap_or(0);
                match bytes[j] {
                    b'A' => *row -= n,
                    b'B' => *row += n,
                    _ => {}
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
}
