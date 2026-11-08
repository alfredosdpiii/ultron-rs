//! Copying text to the system clipboard (`copyToClipboard` of packages/coding-agent utils/clipboard.ts): a native
//! writer off Linux, then the platform's clipboard tools (`pbcopy`, `clip`, `termux-clipboard-set`, `wl-copy`,
//! `xclip`, `xsel`), then on WSL the Windows clipboard through PowerShell, and OSC 52 for remote sessions and
//! display-less Linux. Direct writes come before OSC 52 so the terminal cannot race the native writer.
//!
//! The decisions are pure over [`CopyEnv`] and [`CopyIo`]: the product passes the process environment and real
//! commands ([`SystemCopyIo`]), tests pass recorders, so no test touches the desktop's clipboard. OSC 52 is returned
//! to the caller (the TUI writes it with its frames) instead of being written to stdout here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// What the copy decisions read: `os.platform()` (`linux`, `darwin`, `win32`), the environment and `/proc/version`.
pub struct CopyEnv<'a> {
    pub platform: &'a str,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// The contents of `/proc/version` (`None` when it cannot be read).
    pub proc_version: Option<String>,
}

impl CopyEnv<'_> {
    /// A variable that is set and not empty (`Boolean(env.X)`).
    fn set(&self, name: &str) -> bool {
        (self.var)(name).is_some_and(|v| !v.is_empty())
    }
}

/// The side effects a copy performs.
pub trait CopyIo {
    /// The native clipboard writer (`getNativeClipboard()?.setText`): `None` when there is none.
    fn native_set_text(&mut self, text: &str) -> Option<Result<(), String>>;
    /// `runClipboardCommand`: the command's stdout on success, `None` when it failed (or did not exist).
    fn run(&mut self, command: &str, args: &[String], input: Option<&str>, timeout_ms: u64) -> Option<Vec<u8>>;
    /// Write `text` to a new private temp file (`pi-wsl-clip-<uuid>.txt`); its path.
    fn write_temp(&mut self, text: &str) -> Result<PathBuf, String>;
    /// Remove a temp file (a missing file is fine).
    fn remove(&mut self, path: &Path);
}

fn is_remote_session(env: &CopyEnv) -> bool {
    env.set("SSH_CONNECTION") || env.set("SSH_CLIENT") || env.set("MOSH_CONNECTION")
}

/// `emitOsc52`: the sequence, or `None` when the payload is over the limit.
pub fn osc52(text: &str) -> Option<String> {
    let encoded = jsrt::base64::encode(text.as_bytes());
    (encoded.len() <= MAX_OSC52_ENCODED_LENGTH).then(|| format!("\x1b]52;c;{encoded}\x07"))
}

/// `copyViaWindowsClipboard`: PowerShell reads the text from a file, because `clip.exe` and PowerShell stdin decode
/// piped bytes with the console code page.
fn copy_via_windows_clipboard(text: &str, io: &mut dyn CopyIo) -> bool {
    let Ok(file) = io.write_temp(text) else {
        return false;
    };
    let copied = (|| {
        let win = io.run("wslpath", &["-w".into(), file.display().to_string()], None, 1000)?;
        let win_path = String::from_utf8_lossy(&win).trim().to_string();
        if win_path.is_empty() {
            return None;
        }
        let script = format!(
            "Set-Clipboard -Value ([System.IO.File]::ReadAllText('{}', [System.Text.Encoding]::UTF8))",
            win_path.replace('\'', "''")
        );
        io.run(
            "powershell.exe",
            &["-NoProfile".into(), "-Command".into(), script],
            None,
            5000,
        )
    })()
    .is_some();
    io.remove(&file);
    copied
}

/// `copyToClipboard`: `Ok` with the OSC 52 sequence to write when one is due, else why the copy failed.
pub fn copy_to_clipboard(text: &str, env: &CopyEnv, io: &mut dyn CopyIo) -> Result<Option<String>, String> {
    let p = env.platform;
    let mut copied = false;
    if p != "linux" && matches!(io.native_set_text(text), Some(Ok(()))) {
        copied = true;
    }
    if !copied {
        let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
        match p {
            "darwin" => commands.push(("pbcopy", vec![])),
            "win32" => commands.push(("clip", vec![])),
            _ => {
                if env.set("TERMUX_VERSION") {
                    commands.push(("termux-clipboard-set", vec![]));
                }
                if env.set("WAYLAND_DISPLAY") {
                    commands.push(("wl-copy", vec![]));
                }
                if env.set("DISPLAY") {
                    commands.push(("xclip", vec!["-selection", "clipboard"]));
                    commands.push(("xsel", vec!["--clipboard", "--input"]));
                }
            }
        }
        for (command, args) in commands {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            if io.run(command, &args, Some(text), 5000).is_some() {
                copied = true;
                break;
            }
        }
    }
    let mut emitted: Option<String> = None;
    if !copied && p == "linux" && crate::clipboard_image::is_wsl(env.var, env.proc_version.as_deref()) {
        // Windows Terminal supports OSC 52; prefer it over the slower PowerShell round trip.
        if env.set("WT_SESSION") {
            emitted = osc52(text);
        }
        copied = emitted.is_some() || copy_via_windows_clipboard(text, io);
    }
    // OSC 52 cannot be verified, so a desktop session with a display reports the failure instead. Without a display
    // the terminal is the only route; remote sessions always emit it to reach the client's clipboard.
    let headless = p == "linux" && !env.set("DISPLAY") && !env.set("WAYLAND_DISPLAY") && !env.set("TERMUX_VERSION");
    let mut oversized = false;
    if emitted.is_none() && (is_remote_session(env) || (!copied && headless)) {
        match osc52(text) {
            Some(sequence) => {
                emitted = Some(sequence);
                copied = true;
            }
            None => oversized = true,
        }
    }
    if copied {
        return Ok(emitted);
    }
    if oversized {
        return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".into());
    }
    if p == "linux" {
        if env.set("TERMUX_VERSION") {
            return Err("Clipboard unavailable: install the Termux:API app and `termux-api` package".into());
        }
        if env.set("WAYLAND_DISPLAY") {
            return Err("Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access".into());
        }
        if env.set("DISPLAY") {
            return Err("Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".into());
        }
    }
    Err("Clipboard unavailable".into())
}

/// The real side effects: arboard as the native writer (only consulted off Linux), commands on `path`.
pub struct SystemCopyIo {
    pub path: Option<String>,
    native: Option<arboard::Clipboard>,
}

impl SystemCopyIo {
    pub fn new(path: Option<String>) -> Self {
        Self { path, native: None }
    }
}

impl CopyIo for SystemCopyIo {
    fn native_set_text(&mut self, text: &str) -> Option<Result<(), String>> {
        if self.native.is_none() {
            self.native = arboard::Clipboard::new().ok();
        }
        let board = self.native.as_mut()?;
        Some(board.set_text(text.to_string()).map_err(|e| e.to_string()))
    }

    fn run(&mut self, command: &str, args: &[String], input: Option<&str>, timeout_ms: u64) -> Option<Vec<u8>> {
        crate::proc::run(
            command,
            self.path.as_deref(),
            args,
            input,
            Some(Duration::from_millis(timeout_ms)),
        )
    }

    fn write_temp(&mut self, text: &str) -> Result<PathBuf, String> {
        let path = crate::clipboard::wsl_temp_file("txt");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn remove(&mut self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}
